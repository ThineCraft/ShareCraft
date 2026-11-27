//! A stand-in for signing in to a file server: it records each call and answers from a script.
//! Tests set it on the server's state so no test reaches a real file server.
//!
//! Like Windows, it refuses (`Conflict`) a sign-in to a server that already holds a live
//! connection made with another user name or password.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use server::secrets::Secret;
use server::share_login::{LoginFailure, ShareConnector};

/// One sign-in the fake was asked for. The password is kept only so a test can compare it; a
/// test must never print it.
#[derive(Clone)]
pub struct Connect {
    pub remote: String,
    pub username: String,
    pub password: Secret,
}

#[derive(Clone, Default)]
pub struct FakeConnector {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    connects: Mutex<Vec<Connect>>,
    disconnects: Mutex<Vec<String>>,
    script: Mutex<VecDeque<Result<(), LoginFailure>>>,
    always: Mutex<Option<LoginFailure>>,
    /// Each live connection and the user name and password it was made with.
    live: Mutex<HashMap<String, (String, Secret)>>,
    delay: Mutex<Duration>,
}

fn host_of(remote: &str) -> String {
    remote
        .trim_start_matches('\\')
        .split('\\')
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

impl FakeConnector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The answers of the next connects, in order; after them, connects succeed.
    pub fn answer(&self, answers: impl IntoIterator<Item = Result<(), LoginFailure>>) {
        self.inner
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(answers);
    }

    /// Every later connect fails with `failure` (`None`: they succeed again).
    pub fn always(&self, failure: Option<LoginFailure>) {
        *self.inner.always.lock().unwrap_or_else(|e| e.into_inner()) = failure;
    }

    /// Every later connect takes `delay` before it answers.
    pub fn delay(&self, delay: Duration) {
        *self.inner.delay.lock().unwrap_or_else(|e| e.into_inner()) = delay;
    }

    pub fn connects(&self) -> Vec<Connect> {
        self.inner
            .connects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The remotes connected to, in order.
    pub fn remotes(&self) -> Vec<String> {
        self.connects().into_iter().map(|c| c.remote).collect()
    }

    pub fn disconnects(&self) -> Vec<String> {
        self.inner
            .disconnects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The remotes connected to now, sorted.
    pub fn live(&self) -> Vec<String> {
        let mut live: Vec<String> = self
            .inner
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        live.sort();
        live
    }

    /// Sets this fake as the connector of `state`.
    pub fn install(&self, state: &server::AppState) {
        state
            .share_logins()
            .set_connector_for_tests(Arc::new(self.clone()));
    }
}

impl ShareConnector for FakeConnector {
    fn connect(&self, remote: &str, username: &str, password: &Secret) -> Result<(), LoginFailure> {
        self.inner
            .connects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Connect {
                remote: remote.to_owned(),
                username: username.to_owned(),
                password: password.clone(),
            });
        let delay = *self.inner.delay.lock().unwrap_or_else(|e| e.into_inner());
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let always = *self.inner.always.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(failure) = always {
            return Err(failure);
        }
        let scripted = self
            .inner
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(Ok(()));
        scripted?;
        let mut live = self.inner.live.lock().unwrap_or_else(|e| e.into_inner());
        let host = host_of(remote);
        let conflict = live.iter().any(|(held, (user, secret))| {
            host_of(held) == host && (user != username || secret != password)
        });
        if conflict {
            return Err(LoginFailure::Conflict);
        }
        live.insert(remote.to_owned(), (username.to_owned(), password.clone()));
        Ok(())
    }

    fn disconnect(&self, remote: &str) {
        self.inner
            .disconnects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(remote.to_owned());
        self.inner
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(remote);
    }
}
