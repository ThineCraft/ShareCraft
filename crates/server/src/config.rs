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
/// Directory of installed editors used when `SHARECRAFT_EDITORS_DIR` is not set.
pub const DEFAULT_EDITORS: &str = "./editors";
/// Largest file a document may hold, in bytes, when `SHARECRAFT_MAX_FILE_BYTES` is not set:
/// 10 GiB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// How the server runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Address to listen on; always a loopback address.
    pub addr: SocketAddr,
    /// Where documents are stored: the library repositories, and the content store in
    /// `blobs`.
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
    /// How long one write to a client's socket may take before the connection is dropped. A
    /// client that stopped reading would otherwise keep its connection task, and its place in
    /// the room, forever.
    pub write_timeout: Duration,
    /// The PostgreSQL database holding accounts, sessions and access grants.
    pub database_url: DatabaseUrl,
    /// Where the built editors are installed, one folder per editor, served at `/editors`.
    pub editors_dir: PathBuf,
    /// The largest file, in bytes, a file document may hold.
    pub max_file_bytes: u64,
    /// How long a chunk of an upload may go without sending a byte before it is cut off,
    /// keeping what arrived; a stalled connection would otherwise hold its session for ever.
    pub upload_idle: Duration,
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
    /// The file size limit is not a whole number of bytes above zero.
    #[error("SHARECRAFT_MAX_FILE_BYTES {0:?} is not a number of bytes above zero")]
    BadMaxFileBytes(String),
}

impl Config {
    /// Reads `SHARECRAFT_ADDR`, `SHARECRAFT_DATA`, `SHARECRAFT_WEB`, `SHARECRAFT_EDITORS_DIR` and
    /// `SHARECRAFT_MAX_FILE_BYTES`, using the defaults for any that are unset, and
    /// `DATABASE_URL`, which is required.
    ///
    /// # Errors
    /// `BadAddr` or `NotLoopback` for an unusable address, `BadMaxFileBytes` for an unusable
    /// limit, `NotUnicode` for an unreadable variable, `NoDatabase` when `DATABASE_URL` is unset
    /// or empty.
    pub fn from_env() -> Result<Self, ConfigError> {
        let addr = read_var("SHARECRAFT_ADDR")?;
        let data = read_var("SHARECRAFT_DATA")?;
        let web = read_var("SHARECRAFT_WEB")?;
        let editors = read_var("SHARECRAFT_EDITORS_DIR")?;
        let max_file_bytes = read_var("SHARECRAFT_MAX_FILE_BYTES")?;
        let database = read_var(DATABASE_VARIABLE)?;
        Self::from_values(
            addr.as_deref(),
            data.as_deref(),
            web.as_deref(),
            database.as_deref(),
            editors.as_deref(),
            max_file_bytes.as_deref(),
        )
    }

    /// Builds the settings from the given values, using the defaults for any that are `None`,
    /// except the database URL, which has none.
    ///
    /// # Errors
    /// `BadAddr` or `NotLoopback` for an unusable address, `NoDatabase` for a missing or empty
    /// database URL, `BadMaxFileBytes` for a limit that is not a number above zero.
    pub fn from_values(
        addr: Option<&str>,
        data_dir: Option<&str>,
        web_dir: Option<&str>,
        database_url: Option<&str>,
        editors_dir: Option<&str>,
        max_file_bytes: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let addr = parse_addr(addr.unwrap_or(DEFAULT_ADDR))?;
        let database_url = database_url_from(database_url)?;
        let max_file_bytes = match max_file_bytes {
            None => DEFAULT_MAX_FILE_BYTES,
            Some(text) => match text.trim().parse::<u64>() {
                Ok(bytes) if bytes > 0 => bytes,
                _ => return Err(ConfigError::BadMaxFileBytes(text.to_owned())),
            },
        };
        Ok(Self {
            addr,
            data_dir: PathBuf::from(data_dir.unwrap_or(DEFAULT_DATA)),
            web_dir: PathBuf::from(web_dir.unwrap_or(DEFAULT_WEB)),
            checkpoint_idle: Duration::from_secs(2),
            checkpoint_max: Duration::from_secs(30),
            room_grace: Duration::from_secs(60),
            write_timeout: Duration::from_secs(10),
            database_url,
            editors_dir: PathBuf::from(editors_dir.unwrap_or(DEFAULT_EDITORS)),
            max_file_bytes,
            upload_idle: Duration::from_secs(60),
        })
    }
}

/// The variable naming the database.
const DATABASE_VARIABLE: &str = "DATABASE_URL";

/// Reads `DATABASE_URL` alone, for commands that need only the database.
///
/// # Errors
/// `NoDatabase` when it is unset or empty, `NotUnicode` when it is unreadable.
pub fn database_url_from_env() -> Result<DatabaseUrl, ConfigError> {
    database_url_from(read_var(DATABASE_VARIABLE)?.as_deref())
}

/// The database URL from `value`, trimmed; `NoDatabase` when it is missing or empty.
fn database_url_from(value: Option<&str>) -> Result<DatabaseUrl, ConfigError> {
    match value.map(str::trim) {
        Some(url) if !url.is_empty() => Ok(DatabaseUrl::new(url)),
        _ => Err(ConfigError::NoDatabase),
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
        let c =
            Config::from_values(None, None, None, Some("postgres://local"), None, None).unwrap();
        assert_eq!(c.addr, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(c.data_dir, PathBuf::from("./data"));
        assert_eq!(c.web_dir, PathBuf::from("./web"));
        assert_eq!(c.checkpoint_idle, Duration::from_secs(2));
        assert_eq!(c.checkpoint_max, Duration::from_secs(30));
        assert_eq!(c.room_grace, Duration::from_secs(60));
        assert_eq!(c.write_timeout, Duration::from_secs(10));
        assert_eq!(c.editors_dir, PathBuf::from("./editors"));
        assert_eq!(c.max_file_bytes, 10 * 1024 * 1024 * 1024);
    }

    #[test]
    fn editors_directory_and_file_limit_can_be_set() {
        let c = Config::from_values(
            None,
            None,
            None,
            Some("postgres://local"),
            Some("/srv/editors"),
            Some(" 1048576 "),
        )
        .unwrap();
        assert_eq!(c.editors_dir, PathBuf::from("/srv/editors"));
        assert_eq!(c.max_file_bytes, 1_048_576);
    }

    #[test]
    fn unusable_file_limit_is_an_error() {
        for bad in ["", "0", "-5", "lots", "1.5"] {
            let result =
                Config::from_values(None, None, None, Some("postgres://local"), None, Some(bad));
            assert_eq!(result, Err(ConfigError::BadMaxFileBytes(bad.into())));
        }
    }

    #[test]
    fn unparseable_address_is_an_error() {
        assert_eq!(
            parse_addr("localhost"),
            Err(ConfigError::BadAddr("localhost".into()))
        );
    }
}
