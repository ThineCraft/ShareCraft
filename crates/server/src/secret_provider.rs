//! An outside secret provider: a cloud key vault adapter that several ShareCraft front ends
//! share. ShareCraft does not build the vault; it speaks one small contract over HTTPS.
//!
//! ```text
//! PUT    {base}/v1/secrets/{name}   body {"value":"<base64>"}   -> 204
//! GET    {base}/v1/secrets/{name}                                -> 200 {"value":"<base64>"} | 404
//! DELETE {base}/v1/secrets/{name}                                -> 204 | 404 (both mean gone)
//! Authorization: Bearer <token>      Content-Type: application/json
//! name = "sharecraft-" + the secret's uuid as 32 lower-case hex digits
//! ```
//!
//! It is set up by `SHARECRAFT_SECRET_PROVIDER` (the base address: `https://`, or `http://` to a
//! loopback address only) and `SHARECRAFT_SECRET_PROVIDER_TOKEN_FILE` (the path of a file
//! holding the bearer token, so the token stays out of environment dumps). A request that takes
//! more than ten seconds, or is not answered as the contract says, is `Unavailable`; the token
//! and the values are never in an error or a log line.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use records::SecretId;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::redirect::Policy;
use reqwest::{StatusCode, Url};
use serde::Deserialize;

use crate::secrets::{Secret, SecretError};

/// The environment variable naming the provider's base address.
pub const URL_VARIABLE: &str = "SHARECRAFT_SECRET_PROVIDER";
/// The environment variable naming the file that holds the provider's bearer token.
pub const TOKEN_FILE_VARIABLE: &str = "SHARECRAFT_SECRET_PROVIDER_TOKEN_FILE";
/// How long one request to the provider may take.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// The most a provider's answer may hold: a value is a few hundred bytes at most.
const MAX_ANSWER: usize = 1 << 20;
/// The sentence for a provider that could not be used. It never names a value.
const UNAVAILABLE: &str = "The secret store could not be reached.";

/// The name of the secret `id` at the provider: `sharecraft-` and 32 lower-case hex digits.
#[must_use]
pub fn provider_name(id: SecretId) -> String {
    format!("sharecraft-{}", id.0.simple())
}

/// Where the provider is and the token that opens it, checked. `Debug` never shows the token.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretProviderSettings {
    base: String,
    token: Secret,
}

impl fmt::Debug for SecretProviderSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretProviderSettings")
            .field("base", &self.base)
            .field("token", &self.token)
            .finish()
    }
}

/// Whether `url` names this machine: an IP loopback address or `localhost`.
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.parse::<IpAddr>().map_or_else(
        |_| host.eq_ignore_ascii_case("localhost"),
        |ip| ip.is_loopback(),
    )
}

impl SecretProviderSettings {
    /// The settings for the address `url` and the token file `token_file`; `None` when neither
    /// is given (no provider).
    ///
    /// # Errors
    /// A plain sentence, for the start-up message: only one of the two is given, the address is
    /// not `https://` (or `http://` to a loopback address), or the token file cannot be read or
    /// is empty. It never repeats an address that might hold a password, or the token.
    pub fn from_values(
        url: Option<&str>,
        token_file: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let url = url.map(str::trim).filter(|v| !v.is_empty());
        let token_file = token_file.map(str::trim).filter(|v| !v.is_empty());
        let (url, token_file) = match (url, token_file) {
            (None, None) => return Ok(None),
            (Some(_), None) => {
                return Err(format!(
                    "{URL_VARIABLE} is set but {TOKEN_FILE_VARIABLE} is not; name the file that \
                     holds the provider's token"
                ));
            }
            (None, Some(_)) => {
                return Err(format!(
                    "{TOKEN_FILE_VARIABLE} is set but {URL_VARIABLE} is not; set the provider's \
                     address too, or remove the token file setting"
                ));
            }
            (Some(url), Some(file)) => (url, file),
        };
        let bad_url = || {
            format!(
                "{URL_VARIABLE} is not an address like https://vault.example.com: give an \
                 https:// address (http:// only to a loopback address such as 127.0.0.1) with no \
                 user name, password, query or fragment"
            )
        };
        let parsed = Url::parse(url).map_err(|_| bad_url())?;
        let scheme_ok = match parsed.scheme() {
            "https" => true,
            "http" => is_loopback(&parsed),
            _ => false,
        };
        if !scheme_ok
            || parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(bad_url());
        }
        let text = std::fs::read_to_string(token_file).map_err(|e| {
            format!(
                "{TOKEN_FILE_VARIABLE} names a file that cannot be read ({}): check the path \
                 and that the server may read it",
                e.kind()
            )
        })?;
        let token = text.trim();
        if token.is_empty() || token.chars().any(char::is_control) {
            return Err(format!(
                "{TOKEN_FILE_VARIABLE} names a file that does not hold a token: it must hold \
                 the token on one line"
            ));
        }
        Ok(Some(SecretProviderSettings {
            base: parsed.as_str().trim_end_matches('/').to_owned(),
            token: Secret::new(token),
        }))
    }

    /// The settings from `SHARECRAFT_SECRET_PROVIDER` and
    /// `SHARECRAFT_SECRET_PROVIDER_TOKEN_FILE`. The token is read from the file only, never
    /// from an environment variable.
    ///
    /// # Errors
    /// As [`SecretProviderSettings::from_values`], or a variable that is not valid Unicode.
    pub fn from_env() -> Result<Option<Self>, String> {
        let read = |name: &str| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not valid Unicode")),
        };
        let url = read(URL_VARIABLE)?;
        let file = read(TOKEN_FILE_VARIABLE)?;
        Self::from_values(url.as_deref(), file.as_deref())
    }
}

/// The body of an answer to a `GET`.
#[derive(Deserialize)]
struct Held {
    value: String,
}

/// A client of the outside provider.
pub struct SecretProvider {
    base: String,
    token: Secret,
    client: reqwest::Client,
}

impl fmt::Debug for SecretProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretProvider")
            .field("base", &self.base)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

/// Logs why a request failed, without the request, the answer or any value.
fn log_failure(what: &str, status: Option<StatusCode>, error: Option<&reqwest::Error>) {
    let why = match (status, error) {
        (Some(status), _) => format!("it answered {}", status.as_u16()),
        (None, Some(e)) if e.is_timeout() => "it did not answer in time".to_owned(),
        (None, Some(e)) if e.is_connect() => "it could not be connected to".to_owned(),
        (None, _) => "its answer could not be used".to_owned(),
    };
    eprintln!("sharecraft-server: the secret provider could not {what}: {why}");
}

fn unavailable() -> SecretError {
    SecretError::Unavailable(UNAVAILABLE.to_owned())
}

impl SecretProvider {
    /// A client for the provider `settings` describe.
    ///
    /// # Errors
    /// A sentence when the HTTPS client cannot be set up.
    pub fn new(settings: &SecretProviderSettings) -> Result<Self, String> {
        // Redirects are not followed and no proxy is used (not even one set for the process by
        // `HTTP_PROXY` and the like): the token, and over a loopback `http://` address the
        // values in clear, must go only where the administrator set them up to go.
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| "the secret provider's HTTPS client could not be set up".to_owned())?;
        Ok(SecretProvider {
            base: settings.base.clone(),
            token: settings.token.clone(),
            client,
        })
    }

    /// The provider set up by the environment, if any.
    ///
    /// # Errors
    /// A sentence for the start-up message, as [`SecretProviderSettings::from_env`] gives, or
    /// when the client cannot be set up.
    pub fn from_env() -> Result<Option<SecretProvider>, String> {
        SecretProviderSettings::from_env()?
            .map(|settings| SecretProvider::new(&settings))
            .transpose()
    }

    fn url(&self, name: &str) -> String {
        format!("{}/v1/secrets/{name}", self.base)
    }

    fn request(&self, method: reqwest::Method, name: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(name))
            .header(AUTHORIZATION, format!("Bearer {}", self.token.expose()))
    }

    /// Stores `value` under `name`, replacing any value there.
    ///
    /// # Errors
    /// `Unavailable` when the provider could not be reached or did not accept it.
    pub async fn put(&self, name: &str, value: &[u8]) -> Result<(), SecretError> {
        let body = format!("{{\"value\":\"{}\"}}", STANDARD.encode(value));
        let sent = self
            .request(reqwest::Method::PUT, name)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await;
        match sent {
            Ok(answer) if answer.status().is_success() => Ok(()),
            Ok(answer) => {
                log_failure("store a value", Some(answer.status()), None);
                Err(unavailable())
            }
            Err(e) => {
                log_failure("store a value", None, Some(&e));
                Err(unavailable())
            }
        }
    }

    /// The value under `name`; `None` when the provider has none.
    ///
    /// # Errors
    /// `Unavailable` when the provider could not be reached or its answer could not be used.
    pub async fn get(&self, name: &str) -> Result<Option<Vec<u8>>, SecretError> {
        let answer = match self.request(reqwest::Method::GET, name).send().await {
            Ok(answer) => answer,
            Err(e) => {
                log_failure("read a value", None, Some(&e));
                return Err(unavailable());
            }
        };
        match answer.status() {
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::OK => {}
            status => {
                log_failure("read a value", Some(status), None);
                return Err(unavailable());
            }
        }
        let bytes = match answer.bytes().await {
            Ok(bytes) if bytes.len() <= MAX_ANSWER => bytes,
            Ok(_) => {
                log_failure("read a value", None, None);
                return Err(unavailable());
            }
            Err(e) => {
                log_failure("read a value", None, Some(&e));
                return Err(unavailable());
            }
        };
        let held: Held = serde_json::from_slice(&bytes).map_err(|_| {
            log_failure("read a value", None, None);
            unavailable()
        })?;
        STANDARD.decode(held.value).map(Some).map_err(|_| {
            log_failure("read a value", None, None);
            unavailable()
        })
    }

    /// Deletes the value under `name`. A value already gone is not an error.
    ///
    /// # Errors
    /// `Unavailable` when the provider could not be reached or did not answer as the contract
    /// says.
    pub async fn delete(&self, name: &str) -> Result<(), SecretError> {
        match self.request(reqwest::Method::DELETE, name).send().await {
            Ok(answer)
                if answer.status().is_success() || answer.status() == StatusCode::NOT_FOUND =>
            {
                Ok(())
            }
            Ok(answer) => {
                log_failure("delete a value", Some(answer.status()), None);
                Err(unavailable())
            }
            Err(e) => {
                log_failure("delete a value", None, Some(&e));
                Err(unavailable())
            }
        }
    }
}
