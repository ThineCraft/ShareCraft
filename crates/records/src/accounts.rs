//! Accounts, password sign-in and sessions.

use std::fmt;
use std::sync::OnceLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use collab_core::ids::{SessionId, UserId};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::events::{NewEvent, record};
use crate::{Db, DbError};

/// How long a session lasts after sign-in.
pub const SESSION_LIFETIME: Duration = Duration::days(30);

const MAX_EMAIL_CHARS: usize = 254;
const MAX_NAME_CHARS: usize = 60;
const MIN_PASSWORD_CHARS: usize = 10;
const MAX_PASSWORD_CHARS: usize = 256;
const UNIQUE_VIOLATION: &str = "23505";
const USER_COLUMNS: &str = "id, email, name, is_admin, (disabled_at is not null) as disabled";

/// An account, without its password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub name: String,
    pub is_admin: bool,
    pub disabled: bool,
}

/// An account to create.
pub struct NewUser<'a> {
    pub email: &'a str,
    pub name: &'a str,
    pub password: &'a str,
    pub is_admin: bool,
}

impl fmt::Debug for NewUser<'_> {
    /// Leaves the password out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewUser")
            .field("email", &self.email)
            .field("name", &self.name)
            .field("is_admin", &self.is_admin)
            .finish_non_exhaustive()
    }
}

/// The secret held in a browser cookie that proves a sign-in. Only its SHA-256 is stored.
#[derive(Clone)]
pub struct SessionToken([u8; 32]);

impl SessionToken {
    fn generate() -> Result<SessionToken, getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)?;
        Ok(SessionToken(bytes))
    }

    /// The token as 64 lowercase hexadecimal characters, for the cookie.
    #[must_use]
    pub fn as_cookie_value(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
        out
    }

    /// Reads a cookie value; `None` unless it is exactly 64 hexadecimal characters.
    #[must_use]
    pub fn parse(text: &str) -> Option<SessionToken> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        let (pairs, _) = text.as_bytes().as_chunks::<2>();
        for (slot, [high, low]) in bytes.iter_mut().zip(pairs) {
            *slot = (hex_value(*high)? << 4) | hex_value(*low)?;
        }
        Some(SessionToken(bytes))
    }

    fn digest(&self) -> Vec<u8> {
        Sha256::digest(self.0).to_vec()
    }
}

fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).unwrap_or('0')
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

/// A live sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: SessionId,
    pub user: User,
    pub expires_at: OffsetDateTime,
}

/// Why an account change failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("that email address is already in use")]
    EmailTaken,
    #[error("that is not a valid email address")]
    InvalidEmail,
    #[error("that is not a valid name")]
    InvalidName,
    #[error("the password must be between 10 and 256 characters")]
    WeakPassword,
    #[error("no such account")]
    NotFound,
    #[error("only an administrator may do that")]
    Forbidden,
    #[error("at least one administrator must stay enabled")]
    LastAdmin,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for AccountError {
    fn from(e: sqlx::Error) -> Self {
        AccountError::Db(e.into())
    }
}

/// Why a sign-in failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignInError {
    /// Unknown email, wrong password or disabled account; deliberately not distinguished.
    #[error("the email or password is wrong")]
    Invalid,
    #[error(transparent)]
    Db(#[from] DbError),
}

impl From<sqlx::Error> for SignInError {
    fn from(e: sqlx::Error) -> Self {
        SignInError::Db(e.into())
    }
}

fn validate_email(email: &str) -> Result<&str, AccountError> {
    let email = email.trim();
    let bad_char = |c: char| c.is_whitespace() || c.is_control();
    let well_formed = email.chars().count() <= MAX_EMAIL_CHARS
        && !email.chars().any(bad_char)
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && !domain.is_empty() && !domain.contains('@')
        });
    if well_formed {
        Ok(email)
    } else {
        Err(AccountError::InvalidEmail)
    }
}

fn validate_name(name: &str) -> Result<&str, AccountError> {
    let name = name.trim();
    let length = name.chars().count();
    let well_formed = (1..=MAX_NAME_CHARS).contains(&length)
        && !name.chars().any(|c| c.is_control() || c == '<' || c == '>')
        && name.chars().any(char::is_alphanumeric);
    if well_formed {
        Ok(name)
    } else {
        Err(AccountError::InvalidName)
    }
}

fn validate_password(password: &str) -> Result<(), AccountError> {
    let length = password.chars().count();
    if (MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&length) {
        Ok(())
    } else {
        Err(AccountError::WeakPassword)
    }
}

fn hash_failure(cause: impl fmt::Display) -> DbError {
    DbError::new(format!("password hashing failed: {cause}"))
}

/// Hashes `password` with Argon2id and a fresh salt, off the async threads.
async fn hash_password(password: &str) -> Result<String, DbError> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(hash_failure)
    })
    .await
    .map_err(hash_failure)?
}

/// Whether `password` matches the PHC string `hash`, off the async threads.
async fn verify_password(password: &str, hash: String) -> Result<bool, DbError> {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        hash.parse::<PasswordHash>().is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .map_err(hash_failure)
}

/// A hash nobody knows the password of, made with the same parameters as real hashes. An
/// unknown email is checked against it so that it costs the same as a wrong password.
///
/// If it cannot be made this is an error, never an empty hash that would make unknown emails
/// return instantly.
fn dummy_hash() -> Result<&'static str, DbError> {
    static DUMMY: OnceLock<Result<String, String>> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            let mut secret = [0u8; 32];
            getrandom::fill(&mut secret).map_err(|e| e.to_string())?;
            Argon2::default()
                .hash_password(&secret)
                .map(|hash| hash.to_string())
                .map_err(|e| e.to_string())
        })
        .as_deref()
        .map_err(hash_failure)
}

/// Computes the dummy hash now, so the first sign-in is not slower than the rest, and fails
/// if it cannot be made.
pub(crate) async fn warm_up() -> Result<(), DbError> {
    tokio::task::spawn_blocking(dummy_hash)
        .await
        .map_err(hash_failure)??;
    Ok(())
}

pub(crate) fn user_from_row(row: &PgRow) -> Result<User, sqlx::Error> {
    Ok(User {
        id: UserId(row.try_get("id")?),
        email: row.try_get("email")?,
        name: row.try_get("name")?,
        is_admin: row.try_get("is_admin")?,
        disabled: row.try_get("disabled")?,
    })
}

/// Fails with `Forbidden` unless `actor` is an enabled administrator.
async fn require_admin(conn: &mut PgConnection, actor: UserId) -> Result<(), AccountError> {
    let allowed: Option<bool> =
        sqlx::query_scalar("select is_admin and disabled_at is null from users where id = $1")
            .bind(actor.0)
            .fetch_optional(conn)
            .await?;
    if allowed == Some(true) {
        Ok(())
    } else {
        Err(AccountError::Forbidden)
    }
}

impl Db {
    /// Creates an account. `actor` is `None` for the command line; otherwise it must be an
    /// enabled administrator.
    pub async fn create_user(
        &self,
        actor: Option<UserId>,
        new: NewUser<'_>,
    ) -> Result<User, AccountError> {
        if let Some(actor) = actor {
            require_admin(&mut *self.pool().acquire().await?, actor).await?;
        }
        let email = validate_email(new.email)?;
        let name = validate_name(new.name)?;
        validate_password(new.password)?;
        let hash = hash_password(new.password).await?;
        let id = UserId(Uuid::new_v4());

        let mut tx = self.pool().begin().await?;
        if let Some(actor) = actor {
            require_admin(&mut tx, actor).await?;
        }
        let inserted = sqlx::query(
            "insert into users (id, email, name, password_hash, is_admin) \
             values ($1, $2, $3, $4, $5)",
        )
        .bind(id.0)
        .bind(email)
        .bind(name)
        .bind(&hash)
        .bind(new.is_admin)
        .execute(&mut *tx)
        .await;
        if let Err(e) = inserted {
            let taken = e
                .as_database_error()
                .and_then(|d| d.code())
                .is_some_and(|code| code == UNIQUE_VIOLATION);
            return Err(if taken {
                AccountError::EmailTaken
            } else {
                e.into()
            });
        }
        record(
            &mut tx,
            NewEvent {
                kind: "user.created",
                actor,
                subject: Some(id),
                document: None,
                detail: json!({ "is_admin": new.is_admin }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(User {
            id,
            email: email.to_owned(),
            name: name.to_owned(),
            is_admin: new.is_admin,
            disabled: false,
        })
    }

    /// The account with this id.
    pub async fn user(&self, id: UserId) -> Result<Option<User>, DbError> {
        let sql = format!("select {USER_COLUMNS} from users where id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.0)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(user_from_row).transpose()?)
    }

    /// The account with this email address, ignoring case.
    pub async fn user_by_email(&self, email: &str) -> Result<Option<User>, DbError> {
        let sql = format!("select {USER_COLUMNS} from users where lower(email) = lower($1)");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(email.trim())
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(user_from_row).transpose()?)
    }

    /// Every account, ordered by email.
    pub async fn users(&self) -> Result<Vec<User>, DbError> {
        let sql = format!("select {USER_COLUMNS} from users order by lower(email), id");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.pool())
            .await?;
        Ok(rows
            .iter()
            .map(user_from_row)
            .collect::<Result<_, sqlx::Error>>()?)
    }

    /// Disables or re-enables an account. Disabling ends its sessions, and the last enabled
    /// administrator cannot be disabled. `actor` must be an enabled administrator.
    pub async fn set_disabled(
        &self,
        actor: UserId,
        id: UserId,
        disabled: bool,
    ) -> Result<(), AccountError> {
        let mut tx = self.pool().begin().await?;
        require_admin(&mut tx, actor).await?;
        // Lock order is always: every enabled administrator by id, then the target. Two
        // simultaneous disables therefore take turns and cannot deadlock, and the count of
        // enabled administrators below cannot change under us.
        let enabled_admins: Vec<Uuid> = if disabled {
            sqlx::query_scalar(
                "select id from users where is_admin and disabled_at is null \
                 order by id for update",
            )
            .fetch_all(&mut *tx)
            .await?
        } else {
            Vec::new()
        };
        let target: Option<(bool, bool)> = sqlx::query_as(
            "select is_admin, disabled_at is not null from users where id = $1 for update",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((is_admin, was_disabled)) = target else {
            return Err(AccountError::NotFound);
        };
        if was_disabled == disabled {
            return Ok(());
        }
        let mut sessions_ended = 0u64;
        if disabled {
            if is_admin && enabled_admins.len() <= 1 {
                return Err(AccountError::LastAdmin);
            }
            sessions_ended = sqlx::query(
                "update sessions set ended_at = now() where user_id = $1 and ended_at is null",
            )
            .bind(id.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        sqlx::query(
            "update users set disabled_at = case when $2 then now() else null end where id = $1",
        )
        .bind(id.0)
        .bind(disabled)
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            NewEvent {
                kind: if disabled {
                    "user.disabled"
                } else {
                    "user.enabled"
                },
                actor: Some(actor),
                subject: Some(id),
                document: None,
                detail: json!({ "sessions_ended": sessions_ended }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Checks an email and password and starts a session.
    ///
    /// Every failure is `Invalid`, and an unknown email costs as much work as a wrong password.
    pub async fn sign_in(
        &self,
        email: &str,
        password: &str,
    ) -> Result<(Session, SessionToken), SignInError> {
        let sql = format!(
            "select {USER_COLUMNS}, password_hash from users where lower(email) = lower($1)"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(email.trim())
            .fetch_optional(self.pool())
            .await?;
        let dummy = dummy_hash()?.to_owned();
        // A password no account could have is checked against the dummy hash, so it costs
        // the same as any other wrong password and never reaches Argon2 at full length.
        let too_long = password.chars().count() > MAX_PASSWORD_CHARS;
        let Some(row) = row else {
            verify_password(if too_long { "" } else { password }, dummy).await?;
            return Err(SignInError::Invalid);
        };
        let user = user_from_row(&row)?;
        let hash: String = row.try_get("password_hash")?;
        let matches = if too_long {
            verify_password("", dummy).await?;
            false
        } else {
            verify_password(password, hash).await?
        };
        if !matches || user.disabled {
            let reason = if matches {
                "disabled"
            } else {
                "wrong_password"
            };
            let mut conn = self.pool().acquire().await?;
            record(
                &mut conn,
                NewEvent {
                    kind: "sign_in.failed",
                    actor: None,
                    subject: Some(user.id),
                    document: None,
                    detail: json!({ "reason": reason }),
                },
            )
            .await?;
            return Err(SignInError::Invalid);
        }

        let token = SessionToken::generate()
            .map_err(|e| DbError::new(format!("no randomness for a session token: {e}")))?;
        let id = SessionId(Uuid::new_v4());
        let wanted_expiry = OffsetDateTime::now_utc() + SESSION_LIFETIME;
        let mut tx = self.pool().begin().await?;
        // The account may have been disabled while the password was being checked. Sharing its
        // row lock makes a concurrent disable either finish first (no row is inserted) or wait
        // and then end this session too.
        let expires_at: Option<OffsetDateTime> = sqlx::query_scalar(
            "insert into sessions (id, token_hash, user_id, expires_at) \
             select $1::uuid, $2::bytea, u.id, $4::timestamptz from users u where u.id = $3 and u.disabled_at is null \
             for share of u returning expires_at",
        )
        .bind(id.0)
        .bind(token.digest())
        .bind(user.id.0)
        .bind(wanted_expiry)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(expires_at) = expires_at else {
            return Err(SignInError::Invalid);
        };
        record(
            &mut tx,
            NewEvent {
                kind: "session.started",
                actor: Some(user.id),
                subject: Some(user.id),
                document: None,
                detail: json!({ "session": id }),
            },
        )
        .await?;
        tx.commit().await?;
        Ok((
            Session {
                id,
                user,
                expires_at,
            },
            token,
        ))
    }

    /// The live session for this token; `None` if unknown, ended or expired, or if the account
    /// is disabled.
    pub async fn session(&self, token: &SessionToken) -> Result<Option<Session>, DbError> {
        let row = sqlx::query(
            "select s.id as session_id, s.expires_at, \
                    u.id, u.email, u.name, u.is_admin, (u.disabled_at is not null) as disabled \
             from sessions s join users u on u.id = s.user_id \
             where s.token_hash = $1 and s.ended_at is null and s.expires_at > now() \
               and u.disabled_at is null",
        )
        .bind(token.digest())
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(Session {
            id: SessionId(row.try_get("session_id")?),
            user: user_from_row(&row)?,
            expires_at: row.try_get("expires_at")?,
        }))
    }

    /// Ends a session (signing out). Ending one that is already over does nothing.
    pub async fn end_session(&self, id: SessionId) -> Result<(), DbError> {
        let mut tx = self.pool().begin().await?;
        let owner: Option<Uuid> = sqlx::query_scalar(
            "update sessions set ended_at = now() \
             where id = $1 and ended_at is null returning user_id",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(owner) = owner {
            let owner = UserId(owner);
            record(
                &mut tx,
                NewEvent {
                    kind: "session.ended",
                    actor: Some(owner),
                    subject: Some(owner),
                    document: None,
                    detail: json!({ "session": id }),
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
