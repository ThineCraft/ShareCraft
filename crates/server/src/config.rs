//! Server settings, read from the environment.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Address used when `SHARECRAFT_ADDR` is not set.
pub const DEFAULT_ADDR: &str = "127.0.0.1:8787";
/// Data directory used when `SHARECRAFT_DATA` is not set.
pub const DEFAULT_DATA: &str = "./data";
/// Static file directory used when `SHARECRAFT_WEB` is not set.
pub const DEFAULT_WEB: &str = "./web";

/// How the server runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Address to listen on; always a loopback address.
    pub addr: SocketAddr,
    /// Where documents are stored.
    pub data_dir: PathBuf,
    /// Static files served at `/`.
    pub web_dir: PathBuf,
    /// A checkpoint is written once no change has arrived for this long.
    pub checkpoint_idle: Duration,
    /// A checkpoint is written at the latest this long after the first unsaved change.
    pub checkpoint_max: Duration,
    /// How long a document's room stays open after its last participant leaves, so a browser
    /// that reconnects within this time resumes where it left off.
    pub room_grace: Duration,
    /// The PostgreSQL database holding accounts, sessions and access grants.
    pub database_url: DatabaseUrl,
}

/// A database URL. It usually holds a password, so `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct DatabaseUrl(String);

impl DatabaseUrl {
    /// Wraps `url`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        DatabaseUrl(url.into())
    }

    /// The URL itself, for connecting. Never print it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[hidden]")
    }
}

/// Why the settings could not be used.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The address could not be parsed.
    #[error("SHARECRAFT_ADDR {0:?} is not an IP address and port, for example 127.0.0.1:8787")]
    BadAddr(String),
    /// The address is reachable from other machines; there is no sign-in yet.
    #[error("SHARECRAFT_ADDR {0} is not a loopback address; only local connections are allowed")]
    NotLoopback(SocketAddr),
    /// A variable is set but is not valid Unicode.
    #[error("{0} is not valid Unicode")]
    NotUnicode(&'static str),
    /// `DATABASE_URL` is unset or empty.
    #[error("DATABASE_URL is not set")]
    NoDatabase,
}

impl Config {
    /// Reads `SHARECRAFT_ADDR`, `SHARECRAFT_DATA` and `SHARECRAFT_WEB`, using the defaults for
    /// any that are unset, and `DATABASE_URL`, which is required.
    ///
    /// # Errors
    /// `BadAddr` or `NotLoopback` for an unusable address, `NotUnicode` for an unreadable
    /// variable, `NoDatabase` when `DATABASE_URL` is unset or empty.
    pub fn from_env() -> Result<Self, ConfigError> {
        let addr = read_var("SHARECRAFT_ADDR")?;
        let data = read_var("SHARECRAFT_DATA")?;
        let web = read_var("SHARECRAFT_WEB")?;
        let database = read_var("DATABASE_URL")?;
        Self::from_values(
            addr.as_deref(),
            data.as_deref(),
            web.as_deref(),
            database.as_deref(),
        )
    }

    /// Builds the settings from the given values, using the defaults for any of the first three
    /// that are `None`. The database URL has no default.
    ///
    /// # Errors
    /// `BadAddr` or `NotLoopback` for an unusable address, `NoDatabase` for a missing or empty
    /// database URL.
    pub fn from_values(
        addr: Option<&str>,
        data_dir: Option<&str>,
        web_dir: Option<&str>,
        database_url: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let addr = parse_addr(addr.unwrap_or(DEFAULT_ADDR))?;
        let database_url = match database_url.map(str::trim) {
            Some(url) if !url.is_empty() => DatabaseUrl::new(url),
            _ => return Err(ConfigError::NoDatabase),
        };
        Ok(Self {
            addr,
            data_dir: PathBuf::from(data_dir.unwrap_or(DEFAULT_DATA)),
            web_dir: PathBuf::from(web_dir.unwrap_or(DEFAULT_WEB)),
            checkpoint_idle: Duration::from_secs(2),
            checkpoint_max: Duration::from_secs(30),
            room_grace: Duration::from_secs(60),
            database_url,
        })
    }
}

/// Parses a listening address and refuses any that is not loopback.
///
/// # Errors
/// `BadAddr` if `s` is not an IP address and port, `NotLoopback` if the host is not a loopback
/// address.
pub fn parse_addr(s: &str) -> Result<SocketAddr, ConfigError> {
    let addr: SocketAddr = s
        .trim()
        .parse()
        .map_err(|_| ConfigError::BadAddr(s.to_owned()))?;
    if !addr.ip().is_loopback() {
        return Err(ConfigError::NotLoopback(addr));
    }
    Ok(addr)
}

fn read_var(name: &'static str) -> Result<Option<String>, ConfigError> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode(name)),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_local() {
        let c = Config::from_values(None, None, None, Some("postgres://local")).unwrap();
        assert_eq!(c.addr, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.web_dir, PathBuf::from("./web"));
        assert_eq!(c.checkpoint_idle, Duration::from_secs(2));
        assert_eq!(c.checkpoint_max, Duration::from_secs(30));
        assert_eq!(c.room_grace, Duration::from_secs(60));
    }

    #[test]
    fn unparseable_address_is_an_error() {
        assert_eq!(
            parse_addr("localhost"),
            Err(ConfigError::BadAddr("localhost".into()))
        );
    }
}
