//! Signing in to a file server with its stored login before one of its shares is read.
//!
//! An administrator stores one login per file server (Settings › File shares). Before a share
//! on that server is listed or read, [`ShareLogins::ensure`] makes sure this process holds a
//! connection to the share (`\\host\share`) made with the current login. The password is
//! read through the [`crate::secret_store::SecretStore`] at that moment, which is how several
//! front ends share one login, and goes nowhere but the operating system's sign-in call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

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
    /// Drops this process's connection to `remote`, if any.
    fn disconnect(&self, remote: &str);
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

        fn disconnect(&self, remote: &str) {
            let remote = wide(remote);
            // SAFETY: `remote` is a NUL-terminated UTF-16 buffer that lives until the call
            // returns; the call only reads it. A failure (not connected) needs nothing done.
            let _ = unsafe { WNetCancelConnection2W(remote.as_ptr(), 0, 1) };
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

    fn disconnect(&self, _remote: &str) {}
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
    /// One sign-in at a time, so two reads of one share do not both sign in.
    turn: tokio::sync::Mutex<()>,
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
            turn: tokio::sync::Mutex::new(()),
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

    fn connected(&self) -> std::sync::MutexGuard<'_, HashMap<String, SecretId>> {
        self.connected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Makes sure this process is signed in to the share of `root` with the current stored
    /// login of its server. A root on a server with no stored login (or not on a file server
    /// at all) needs nothing.
    ///
    /// # Errors
    /// Why signing in failed; nothing is remembered then.
    pub async fn ensure(&self, state: &AppState, root: &SharePath) -> Result<(), LoginFailure> {
        let (Some(server), Some(remote)) = (root.server(), remote_of(root)) else {
            return Ok(());
        };
        let login = state.db().share_login(&server).await.map_err(|e| {
            eprintln!("sharecraft-server: a stored share login could not be read: {e}");
            LoginFailure::Other(0)
        })?;
        let Some(login) = login else {
            // The login was removed (perhaps by another front end, or by a removal that raced
            // a sign-in): nothing may go on using it.
            if self.holds_server(&server) {
                let _turn = self.turn.lock().await;
                self.forget_server(&server);
            }
            return Ok(());
        };
        if self.connected().get(&remote) == Some(&login.password_secret) {
            return Ok(());
        }
        let _turn = self.turn.lock().await;
        // Another read may have signed in while this one waited.
        if self.connected().get(&remote) == Some(&login.password_secret) {
            return Ok(());
        }
        let context = format!("share-login:{server}");
        let password = match state
            .secret_store()
            .get(state.db(), login.password_secret, context.as_bytes())
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
            .any(|(held, secret)| on_server(held, &server) && *secret != login.password_secret);
        if stale {
            self.forget_server(&server);
        }
        let connector = self.connector();
        connect(
            connector,
            remote.clone(),
            login.username,
            password,
            state.shares().operation_timeout(),
        )
        .await?;
        self.connected().insert(remote, login.password_secret);
        Ok(())
    }

    /// Forgets the connection to `remote` and drops it, so the next [`ShareLogins::ensure`]
    /// signs in again.
    pub fn invalidate(&self, remote: &str) {
        let remote = remote.to_lowercase();
        self.connected().remove(&remote);
        self.connector().disconnect(&remote);
    }

    /// Whether a connection to a share of `server` is recorded.
    fn holds_server(&self, server: &str) -> bool {
        self.connected()
            .keys()
            .any(|remote| on_server(remote, server))
    }

    /// Drops every connection this process made to a share of `server`.
    pub fn forget_server(&self, server: &str) {
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
        let connector = self.connector();
        for remote in remotes {
            connector.disconnect(&remote);
        }
    }

    /// Drops the connections to every server not in `keep`.
    pub(crate) fn forget_servers_except(&self, keep: &[String]) {
        let servers: Vec<String> = self
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
        for server in servers {
            self.forget_server(&server);
        }
    }

    /// Records that `remote` was signed in to with the login whose password is `secret`.
    pub(crate) fn remember(&self, remote: &str, secret: SecretId) {
        self.connected().insert(remote.to_lowercase(), secret);
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
    wait: std::time::Duration,
) -> Result<(), LoginFailure> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let seen = Arc::clone(&outcome);
    let undo = Arc::clone(&connector);
    let target = remote.clone();
    let task = tokio::task::spawn_blocking(move || {
        let answer = connector.connect(&remote, &username, &password);
        let mut seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if answer.is_ok() {
            if seen.abandoned {
                connector.disconnect(&remote);
            } else {
                seen.connected = true;
            }
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
            // Finished just as the wait ran out: undone here instead.
            if connected {
                undo.disconnect(&target);
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
        LoginFailure::Other(_) => {
            format!(r"The stored login for \\{server} could not be used, so nothing was changed.")
        }
    }
}
