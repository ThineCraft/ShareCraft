//! A stand-in for signing in to a file server: it records each call and answers from a script.
//! Tests set it on the server's state so no test reaches a real file server.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
        let always = *self.inner.always.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(failure) = always {
            return Err(failure);
        }
        self.inner
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(Ok(()))
    }

    fn disconnect(&self, remote: &str) {
        self.inner
            .disconnects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(remote.to_owned());
    }
}
