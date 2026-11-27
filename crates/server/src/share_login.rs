//! Signing in to a file server with its stored login before one of its shares is read.
//!
//! An administrator stores one login per file server (Settings › File shares). Before a share
//! on that server is listed or read, [`ShareLogins::ensure`] makes sure this process holds a
//! connection to the share (`\\host\share`) made with the current login. The password is
//! read through the [`crate::secret_store::SecretStore`] at that moment, which is how several
//! front ends share one login, and goes nowhere but the operating system's sign-in call.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use records::{SecretId, SharePath};

use crate::AppState;
use crate::secrets::{Secret, SecretError};

/// Why signing in to a share failed. Never carries a user name or password.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginFailure {
    /// The file server refused the user name and password.
    Refused,
    /// This server already holds a connection to the file server with other credentials (for
    /// example a drive the service account mapped).
    Conflict,
    /// The file server or the share could not be reached.
    Unreachable,
    /// The secret store (the key file or the outside provider) could not be reached, so the
    /// password could not be read. Worth trying again later.
    SecretStoreUnavailable,
    /// The provider answers but no longer holds the stored password. It has to be set again.
    SecretLost,
    /// Anything else, with the operating system's error code (0 when there is none).
    Other(u32),
}

/// What signs in to and out of a share. `remote` is `\\host\share`.
pub trait ShareConnector: Send + Sync {
    /// Signs in to `remote` with `username` and `password` for this process.
    ///
    /// # Errors
    /// Why the sign-in failed.
    fn connect(&self, remote: &str, username: &str, password: &Secret) -> Result<(), LoginFailure>;
    /// Drops this process's connection to `remote`, if any. With `force`, files still open
    /// through it are closed too; without, a connection with open files is left to close when
    /// they do. It may block (a file server that has gone), so it is called on a blocking
    /// thread.
    fn disconnect(&self, remote: &str, force: bool);
}

/// Maps a Windows network error code to a [`LoginFailure`].
#[must_use]
pub fn failure_of(code: u32) -> LoginFailure {
    match code {
        // ERROR_LOGON_FAILURE, ERROR_ACCESS_DENIED, ERROR_INVALID_PASSWORD
        1326 | 5 | 86 => LoginFailure::Refused,
        // ERROR_SESSION_CREDENTIAL_CONFLICT
        1219 => LoginFailure::Conflict,
        // ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME, ERROR_NO_NET_OR_BAD_PATH
        53 | 67 | 1203 => LoginFailure::Unreachable,
        other => LoginFailure::Other(other),
    }
}

/// Signs in with `WNetAddConnection2W`, for this logon session only and with no drive letter.
#[cfg(windows)]
pub struct WNetConnector;

#[cfg(windows)]
#[allow(unsafe_code)]
mod wnet {
    use windows_sys::Win32::NetworkManagement::WNet::{
        CONNECT_TEMPORARY, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
        WNetCancelConnection2W,
    };

    use super::{LoginFailure, ShareConnector, WNetConnector, failure_of};
    use crate::secrets::Secret;

    /// `text` as UTF-16 ending in a NUL, allocated once at its final size so no partial copy
    /// (of a password) is left behind by a reallocation.
    fn wide(text: &str) -> Vec<u16> {
        let mut wide = Vec::with_capacity(text.encode_utf16().count().saturating_add(1));
        wide.extend(text.encode_utf16());
        wide.push(0);
        wide
    }

    /// Overwrites `buffer` with zeros in a way the optimiser keeps.
    fn wipe(buffer: &mut [u16]) {
        for unit in buffer.iter_mut() {
            // SAFETY: `unit` is a valid, aligned, exclusive reference into `buffer`.
            unsafe { std::ptr::write_volatile(unit, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }

    impl ShareConnector for WNetConnector {
        fn connect(
            &self,
            remote: &str,
            username: &str,
            password: &Secret,
        ) -> Result<(), LoginFailure> {
            let mut remote = wide(remote);
            let user = wide(username);
            let mut secret = wide(password.expose());
            let resource = NETRESOURCEW {
                dwType: RESOURCETYPE_DISK,
                lpLocalName: std::ptr::null_mut(),
                lpRemoteName: remote.as_mut_ptr(),
                ..Default::default()
            };
            // SAFETY: `resource` and the three NUL-terminated UTF-16 buffers it and the call
            // point to live until the call returns; the call only reads them.
            let code = unsafe {
                WNetAddConnection2W(
                    &raw const resource,
                    secret.as_ptr(),
                    user.as_ptr(),
                    CONNECT_TEMPORARY,
                )
            };
            wipe(&mut secret);
            if code == 0 {
                Ok(())
            } else {
                Err(failure_of(code))
            }
        }

        fn disconnect(&self, remote: &str, force: bool) {
            let remote = wide(remote);
            // SAFETY: `remote` is a NUL-terminated UTF-16 buffer that lives until the call
            // returns; the call only reads it. A failure (not connected, or files still open
            // without `force`) needs nothing done.
            let _ = unsafe { WNetCancelConnection2W(remote.as_ptr(), 0, i32::from(force)) };
        }
    }
}

/// Where stored logins cannot be used (anywhere but Windows): every sign-in fails.
pub struct NoConnector;

impl ShareConnector for NoConnector {
    fn connect(
        &self,
        _remote: &str,
        _username: &str,
        _password: &Secret,
    ) -> Result<(), LoginFailure> {
        Err(LoginFailure::Other(0))
    }

    fn disconnect(&self, _remote: &str, _force: bool) {}
}

/// The share of `root` as `\\host\share`, lower-cased; `None` for a drive or POSIX path.
#[must_use]
pub fn remote_of(root: &SharePath) -> Option<String> {
    root.server()?;
    let display = root.display();
    let rest = display.strip_prefix(r"\\")?;
    let mut parts = rest.split('\\');
    let host = parts.next()?;
    let share = parts.next()?;
    Some(format!(r"\\{host}\{share}").to_lowercase())
}

/// Whether `remote` (`\\host\share`) is on `server`.
fn on_server(remote: &str, server: &str) -> bool {
    remote
        .strip_prefix(r"\\")
        .and_then(|rest| rest.split('\\').next())
        .is_some_and(|host| host.eq_ignore_ascii_case(server))
}

/// The connections this process made with stored logins.
pub struct ShareLogins {
    connector: RwLock<Arc<dyn ShareConnector>>,
    /// Each connected remote and the secret of the login it was made with.
    connected: Mutex<HashMap<String, SecretId>>,
    /// Each server (lower-case) whose stored login the file server refused, and the secret of
    /// that login. While the login is unchanged it is not tried again: every try could count
    /// toward locking the account.
    refused: Mutex<HashMap<String, SecretId>>,
    /// One sign-in or sign-out at a time, so two reads of one share do not both sign in and
    /// a sign-out never races a sign-in.
    turn: tokio::sync::Mutex<()>,
    /// Whether stored logins can be used here (Windows only).
    supported: AtomicBool,
}

/// Holding [`ShareLogins`]' turn: no other sign-in or sign-out runs meanwhile.
pub(crate) struct Turn<'a> {
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl Default for ShareLogins {
    fn default() -> Self {
        #[cfg(windows)]
        let connector: Arc<dyn ShareConnector> = Arc::new(WNetConnector);
        #[cfg(not(windows))]
        let connector: Arc<dyn ShareConnector> = Arc::new(NoConnector);
        Self {
            connector: RwLock::new(connector),
            connected: Mutex::new(HashMap::new()),
            refused: Mutex::new(HashMap::new()),
            turn: tokio::sync::Mutex::new(()),
            supported: AtomicBool::new(cfg!(windows)),
        }
    }
}

impl ShareLogins {
    /// The connector in use.
    #[must_use]
    pub fn connector(&self) -> Arc<dyn ShareConnector> {
        Arc::clone(
            &self
                .connector
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Replaces the connector. Used by tests, so that none reaches a real file server.
    #[doc(hidden)]
    pub fn set_connector_for_tests(&self, connector: Arc<dyn ShareConnector>) {
        *self
            .connector
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = connector;
    }

    /// Whether stored logins can be used here: only a server running on Windows signs in to a
    /// file server.
    #[must_use]
    pub fn supported(&self) -> bool {
        self.supported.load(Ordering::SeqCst)
    }

    /// Says whether stored logins can be used here. Used by tests, so that the paths of storing
    /// a login (with a fake connector) run on every system.
    #[doc(hidden)]
    pub fn set_supported_for_tests(&self, supported: bool) {
        self.supported.store(supported, Ordering::SeqCst);
    }

    fn connected(&self) -> std::sync::MutexGuard<'_, HashMap<String, SecretId>> {
        self.connected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn refused(&self) -> std::sync::MutexGuard<'_, HashMap<String, SecretId>> {
        self.refused
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits for the turn: no other sign-in or sign-out runs until it is dropped.
    pub(crate) async fn turn(&self) -> Turn<'_> {
        Turn {
            _guard: self.turn.lock().await,
        }
    }

    /// Whether the login whose password is `secret` was refused for `server`, and is still the
    /// one stored. A refusal of another (older) login is forgotten.
    fn still_refused(&self, server: &str, secret: SecretId) -> bool {
        let mut refused = self.refused();
        match refused.get(server) {
            Some(held) if *held == secret => true,
            Some(_) => {
                refused.remove(server);
                false
            }
            None => false,
        }
    }

    /// Makes sure this process is signed in to the share of `root` with the current stored
    /// login of its server. A root on a server with no stored login (or not on a file server
    /// at all) needs nothing. A login the file server refused is not tried again until it is
    /// set again: it answers `Refused` at once.
    ///
    /// # Errors
    /// Why signing in failed; nothing is remembered then but a refusal.
    pub async fn ensure(&self, state: &AppState, root: &SharePath) -> Result<(), LoginFailure> {
        let (Some(server), Some(remote)) = (root.server(), remote_of(root)) else {
            return Ok(());
        };
        let wait = state.shares().operation_timeout();
        // Most reads find the connection made already, without waiting for the turn.
        match login_of(state, &server).await? {
            Some(login) => {
                if self.still_refused(&server, login.password_secret) {
                    return Err(LoginFailure::Refused);
                }
                if self.connected().get(&remote) == Some(&login.password_secret) {
                    return Ok(());
                }
            }
            None if !self.holds_server(&server) => {
                self.refused().remove(&server);
                return Ok(());
            }
            None => {}
        }
        let turn = self.turn().await;
        // Read again in the turn: a login set while this read waited (the settings page signs
        // in with a new login in the turn) is the one used, not the one it replaced.
        let Some(login) = login_of(state, &server).await? else {
            // The login was removed (perhaps by another front end, or by a removal that raced
            // a sign-in): nothing may go on using it.
            self.sign_out_of_server(&turn, &server, wait).await;
            return Ok(());
        };
        let secret = login.password_secret;
        // Another read may have signed in, or been refused, while this one waited.
        if self.still_refused(&server, secret) {
            return Err(LoginFailure::Refused);
        }
        if self.connected().get(&remote) == Some(&secret) {
            return Ok(());
        }
        let context = format!("share-login:{server}");
        let password = match state
            .secret_store()
            .get(state.db(), secret, context.as_bytes())
            .await
        {
            Ok(Some(bytes)) => Secret::new(String::from_utf8(bytes).map_err(|_| {
                eprintln!("sharecraft-server: the stored login of {server} is not text");
                LoginFailure::Other(0)
            })?),
            Ok(None) => {
                eprintln!("sharecraft-server: the stored login of {server} has no password");
                return Err(LoginFailure::Other(0));
            }
            Err(SecretError::Unavailable(_)) => return Err(LoginFailure::SecretStoreUnavailable),
            Err(SecretError::Lost) => return Err(LoginFailure::SecretLost),
            Err(e) => {
                eprintln!("sharecraft-server: the stored login of {server} did not open: {e}");
                return Err(LoginFailure::Other(0));
            }
        };
        // A connection to any share of the server made with a login since replaced (here or by
        // another front end) is dropped first: Windows refuses a second login to one server.
        let stale = self
            .connected()
            .iter()
            .any(|(held, held_secret)| on_server(held, &server) && *held_secret != secret);
        if stale {
            self.sign_out_of_server(&turn, &server, wait).await;
        }
        let connector = self.connector();
        let answer = connect(connector, remote.clone(), login.username, password, wait).await;
        match answer {
            Ok(()) => {
                self.connected().insert(remote, secret);
                Ok(())
            }
            Err(LoginFailure::Refused) => {
                self.refused().insert(server, secret);
                Err(LoginFailure::Refused)
            }
            Err(failure) => Err(failure),
        }
    }

    /// Forgets the connection to `remote` and drops it, so the next [`ShareLogins::ensure`]
    /// signs in again. Files still open through it (other reads of the share) are not closed:
    /// the connection then goes when they do.
    pub async fn invalidate(&self, state: &AppState, remote: &str) {
        let remote = remote.to_lowercase();
        let _turn = self.turn().await;
        self.connected().remove(&remote);
        sign_out(
            self.connector(),
            vec![remote],
            false,
            state.shares().operation_timeout(),
        )
        .await;
    }

    /// Whether a connection to a share of `server` is recorded.
    fn holds_server(&self, server: &str) -> bool {
        self.connected()
            .keys()
            .any(|remote| on_server(remote, server))
    }

    /// Drops every connection this process made to a share of `server`, closing files still
    /// open through them (its login was replaced or removed), and forgets that its login was
    /// refused.
    pub async fn forget_server(&self, state: &AppState, server: &str) {
        let turn = self.turn().await;
        self.sign_out_of_server(&turn, server, state.shares().operation_timeout())
            .await;
    }

    /// [`ShareLogins::forget_server`], for the holder of the turn. Waits no longer than `wait`
    /// for the sign-outs.
    pub(crate) async fn sign_out_of_server(&self, _turn: &Turn<'_>, server: &str, wait: Duration) {
        self.refused().remove(&server.to_lowercase());
        let remotes: Vec<String> = {
            let mut connected = self.connected();
            let remotes = connected
                .keys()
                .filter(|remote| on_server(remote, server))
                .cloned()
                .collect();
            connected.retain(|remote, _| !on_server(remote, server));
            remotes
        };
        sign_out(self.connector(), remotes, true, wait).await;
    }

    /// Drops the connections to every server not in `keep`.
    pub(crate) async fn forget_servers_except(&self, state: &AppState, keep: &[String]) {
        let mut servers: Vec<String> = self
            .connected()
            .keys()
            .filter_map(|remote| {
                remote
                    .strip_prefix(r"\\")
                    .and_then(|rest| rest.split('\\').next())
                    .map(str::to_owned)
            })
            .filter(|server| !keep.iter().any(|kept| kept.eq_ignore_ascii_case(server)))
            .collect();
        servers.sort();
        servers.dedup();
        self.refused()
            .retain(|server, _| keep.iter().any(|kept| kept.eq_ignore_ascii_case(server)));
        if servers.is_empty() {
            return;
        }
        let turn = self.turn().await;
        for server in servers {
            self.sign_out_of_server(&turn, &server, state.shares().operation_timeout())
                .await;
        }
    }

    /// Records that `remote` was signed in to with the login whose password is `secret`, for
    /// the holder of the turn.
    pub(crate) fn remember(&self, _turn: &Turn<'_>, remote: &str, secret: SecretId) {
        self.connected().insert(remote.to_lowercase(), secret);
    }
}

/// The stored login of `server`.
///
/// # Errors
/// `Other(0)` when the database could not be read.
async fn login_of(
    state: &AppState,
    server: &str,
) -> Result<Option<records::ShareLogin>, LoginFailure> {
    state.db().share_login(server).await.map_err(|e| {
        eprintln!("sharecraft-server: a stored share login could not be read: {e}");
        LoginFailure::Other(0)
    })
}
/// Signs out of each of `remotes` on blocking threads, all at once, and waits for them no
/// longer than `wait`: a file server that has gone can hold a sign-out for a long time, and
/// none may hold up the runtime or the turn beyond that. A sign-out still running then is left
/// to finish on its own.
pub(crate) async fn sign_out(
    connector: Arc<dyn ShareConnector>,
    remotes: Vec<String>,
    force: bool,
    wait: Duration,
) {
    if remotes.is_empty() {
        return;
    }
    let tasks: Vec<_> = remotes
        .into_iter()
        .map(|remote| {
            let connector = Arc::clone(&connector);
            tokio::task::spawn_blocking(move || connector.disconnect(&remote, force))
        })
        .collect();
    let all = async {
        for task in tasks {
            let _ = task.await;
        }
    };
    if tokio::time::timeout(wait, all).await.is_err() {
        eprintln!("sharecraft-server: signing out of a file share did not finish in time");
    }
}

/// What became of a sign-in on its blocking thread, as seen by the thread and by its waiter.
#[derive(Default)]
struct Outcome {
    /// The waiter gave up: a sign-in that succeeds after this must be undone.
    abandoned: bool,
    /// The sign-in succeeded.
    connected: bool,
}

/// Signs in on a blocking thread, waiting no longer than `wait`. A sign-in still running then
/// counts as unreachable here, and is undone (disconnected) if it succeeds later, so no
/// connection is left that nobody recorded.
///
/// # Errors
/// Why signing in failed.
pub(crate) async fn connect(
    connector: Arc<dyn ShareConnector>,
    remote: String,
    username: String,
    password: Secret,
    wait: Duration,
) -> Result<(), LoginFailure> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let seen = Arc::clone(&outcome);
    let undo = Arc::clone(&connector);
    let target = remote.clone();
    let task = tokio::task::spawn_blocking(move || {
        let answer = connector.connect(&remote, &username, &password);
        let undo_now = {
            let mut seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if answer.is_ok() && !seen.abandoned {
                seen.connected = true;
            }
            answer.is_ok() && seen.abandoned
        };
        // Not under the lock: the waiter takes it on the runtime, and a sign-out can block.
        if undo_now {
            connector.disconnect(&remote, true);
        }
        answer
    });
    match tokio::time::timeout(wait, task).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => Err(LoginFailure::Other(0)),
        Err(_) => {
            let connected = {
                let mut seen = outcome
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                seen.abandoned = true;
                seen.connected
            };
            // Finished just as the wait ran out: undone here instead, on a blocking thread
            // nobody waits for.
            if connected {
                drop(tokio::task::spawn_blocking(move || {
                    undo.disconnect(&target, true);
                }));
            }
            Err(LoginFailure::Unreachable)
        }
    }
}

/// The plain sentence a library shows for a failed sign-in to `server`.
#[must_use]
pub fn failure_sentence(failure: LoginFailure, server: &str) -> String {
    match failure {
        LoginFailure::Refused => format!(
            r"The stored login for \\{server} was refused. An administrator can set it again under Settings › File shares."
        ),
        LoginFailure::Conflict => format!(
            r"This server already holds a connection to \\{server} with other credentials, so the stored login could not be used. An administrator can remove that connection."
        ),
        LoginFailure::Unreachable => {
            format!(r"The file server \\{server} could not be reached, so nothing was changed.")
        }
        LoginFailure::SecretStoreUnavailable => "The secret store could not be reached.".to_owned(),
        LoginFailure::SecretLost => format!(
            r"The stored password for \\{server} is gone from the secret store. Set the login again."
        ),
        LoginFailure::Other(_) => {
            format!(r"The stored login for \\{server} could not be used, so nothing was changed.")
        }
    }
}
