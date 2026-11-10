//! Who is calling: the session cookie, the `Signed` extractor, the same-origin guard and the
//! sign-in throttle.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::{self, HOST, ORIGIN};
use axum::http::request::Parts;
use records::{Session, SessionToken};

use crate::AppState;
use crate::api::ApiError;

/// The name of the cookie holding the session token.
pub(crate) const COOKIE: &str = "sharecraft_session";

/// A caller with a live session. Rejects with `401 {"error":"not_signed_in"}` when the request
/// has no session cookie, or one that is unknown, ended, expired or belongs to a disabled
/// account.
pub(crate) struct Signed {
    pub session: Session,
}

impl FromRequestParts<AppState> for Signed {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = session_token(&parts.headers).ok_or(ApiError::NotSignedIn)?;
        match state.db().session(&token).await {
            Ok(Some(session)) => Ok(Signed { session }),
            Ok(None) => Err(ApiError::NotSignedIn),
            Err(e) => Err(ApiError::unavailable(&e)),
        }
    }
}

/// The session token in the request's cookies. Malformed values are ignored.
fn session_token(headers: &HeaderMap) -> Option<SessionToken> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == COOKIE)
        .find_map(|(_, value)| SessionToken::parse(value.trim()))
}

/// The `Set-Cookie` value that stores `token` for the session's lifetime.
pub(crate) fn session_cookie(token: &SessionToken) -> String {
    format!(
        "{COOKIE}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        token.as_cookie_value(),
        records::SESSION_LIFETIME.whole_seconds()
    )
}

/// The `Set-Cookie` value that removes the session cookie.
pub(crate) fn cleared_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

/// Whether the request names a loopback host and, if it comes from a page (it carries
/// `Origin`), that page was served by this server: `Origin` is `http://<Host>`, compared ignoring
/// case. This keeps other websites open in the user's browser, and DNS-rebinding hosts, away.
pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    if !host_part(host).is_some_and(is_loopback_host) {
        return false;
    }
    match headers.get(ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .is_ok_and(|origin| origin.eq_ignore_ascii_case(&format!("http://{host}"))),
    }
}

/// The host of a `Host` header value (`host`, `host:port`, `[v6]` or `[v6]:port`).
fn host_part(value: &str) -> Option<&str> {
    let (host, port) = match value.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            match after {
                "" => (host, None),
                _ => (host, Some(after.strip_prefix(':')?)),
            }
        }
        None => match value.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (value, None),
        },
    };
    if port.is_some_and(|p| p.parse::<u16>().is_err()) || host.is_empty() {
        return None;
    }
    Some(host)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Failed sign-ins allowed per email within [`WINDOW`].
const MAX_FAILURES: usize = 5;
/// How long a failed sign-in counts against its email.
const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Most emails the throttle remembers.
const MAX_EMAILS: usize = 10_000;
/// Longest email address an account can have; longer keys are cut to this.
const MAX_KEY_CHARS: usize = 254;

/// Recent failed sign-ins per email, in memory. After [`MAX_FAILURES`] failures within
/// [`WINDOW`], an email is refused until the oldest of them is [`WINDOW`] old.
#[derive(Default)]
pub(crate) struct Throttle {
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Throttle {
    /// The key for `email`: trimmed and lowercased, so case and spaces do not make a new email,
    /// and cut to the longest email an account can have, so the table stays small.
    pub(crate) fn key(email: &str) -> String {
        email
            .trim()
            .to_lowercase()
            .chars()
            .take(MAX_KEY_CHARS)
            .collect()
    }

    /// Whether a sign-in for `key` may be attempted at `now`.
    pub(crate) fn allows(&self, key: &str, now: Instant) -> bool {
        let mut failures = self.lock();
        let Some(times) = failures.get_mut(key) else {
            return true;
        };
        forget_expired(times, now);
        if times.is_empty() {
            failures.remove(key);
            return true;
        }
        times.len() < MAX_FAILURES
    }

    /// Counts a failed sign-in for `key` at `now`.
    pub(crate) fn failed(&self, key: &str, now: Instant) {
        let mut failures = self.lock();
        if !failures.contains_key(key) && failures.len() >= MAX_EMAILS {
            make_room(&mut failures, now);
        }
        let times = failures.entry(key.to_owned()).or_default();
        forget_expired(times, now);
        times.push_back(now);
        while times.len() > MAX_FAILURES {
            times.pop_front();
        }
    }

    /// Forgets the failures for `key` after a successful sign-in.
    pub(crate) fn succeeded(&self, key: &str) {
        self.lock().remove(key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VecDeque<Instant>>> {
        // The map holds no invariant a panic elsewhere could break.
        self.failures.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn forget_expired(times: &mut VecDeque<Instant>, now: Instant) {
    while times
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= WINDOW)
    {
        times.pop_front();
    }
}

/// Drops every email whose failures have all expired; if none has, drops the email whose
/// latest failure is oldest.
fn make_room(failures: &mut HashMap<String, VecDeque<Instant>>, now: Instant) {
    failures.retain(|_, times| {
        forget_expired(times, now);
        !times.is_empty()
    });
    if failures.len() < MAX_EMAILS {
        return;
    }
    let stalest = failures
        .iter()
        .min_by_key(|(_, times)| times.back().copied())
        .map(|(key, _)| key.clone());
    if let Some(key) = stalest {
        failures.remove(&key);
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
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn host_header_forms_are_parsed() {
        assert_eq!(host_part("127.0.0.1:8787"), Some("127.0.0.1"));
        assert_eq!(host_part("localhost"), Some("localhost"));
        assert_eq!(host_part("[::1]:8787"), Some("::1"));
        assert_eq!(host_part("[::1]"), Some("::1"));
        assert_eq!(host_part("evil.example:8787"), Some("evil.example"));
        assert_eq!(host_part("localhost:99999"), None);
        assert_eq!(host_part("[::1]x"), None);
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("LocalHost"));
        assert!(!is_loopback_host("evil.example"));
        assert!(!is_loopback_host("0.0.0.0"));
    }

    fn headers(host: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(HOST, HeaderValue::from_str(host).unwrap());
        }
        if let Some(origin) = origin {
            headers.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers
    }

    #[test]
    fn same_origin_needs_a_loopback_host_and_a_matching_origin() {
        assert!(same_origin(&headers(Some("127.0.0.1:8787"), None)));
        assert!(same_origin(&headers(
            Some("127.0.0.1:8787"),
            Some("http://127.0.0.1:8787")
        )));
        assert!(same_origin(&headers(
            Some("LOCALHOST:8787"),
            Some("http://localhost:8787")
        )));
        assert!(!same_origin(&headers(None, None)));
        assert!(!same_origin(&headers(Some("evil.example:8787"), None)));
        assert!(!same_origin(&headers(
            Some("127.0.0.1:8787"),
            Some("http://evil.example")
        )));
        assert!(!same_origin(&headers(
            Some("127.0.0.1:8787"),
            Some("http://127.0.0.1:9999")
        )));
        assert!(!same_origin(&headers(Some("127.0.0.1:8787"), Some("null"))));
    }

    #[test]
    fn session_token_is_found_among_cookies() {
        let token = "ab".repeat(32);
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1"));
        h.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("b=2; {COOKIE}=bad; {COOKIE}={token}")).unwrap(),
        );
        assert_eq!(session_token(&h).unwrap().as_cookie_value(), token);

        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("sharecraft_session=zz"),
        );
        assert!(session_token(&h).is_none());
        assert!(session_token(&HeaderMap::new()).is_none());
    }

    #[test]
    fn throttle_refuses_after_five_failures_until_the_oldest_expires() {
        let throttle = Throttle::default();
        let start = Instant::now();
        let key = Throttle::key(" Ann@X.test ");
        assert_eq!(key, "ann@x.test");
        for i in 0..5 {
            assert!(throttle.allows(&key, start));
            throttle.failed(&key, start + Duration::from_secs(i * 60));
        }
        let later = start + Duration::from_secs(5 * 60);
        assert!(!throttle.allows(&key, later));
        assert!(throttle.allows("bob@x.test", later));
        // The first failure expires at start + WINDOW.
        assert!(!throttle.allows(&key, start + WINDOW - Duration::from_secs(1)));
        assert!(throttle.allows(&key, start + WINDOW));
        throttle.succeeded(&key);
        assert!(throttle.allows(&key, later));
    }

    #[test]
    fn throttle_table_is_bounded() {
        let throttle = Throttle::default();
        let start = Instant::now();
        for i in 0..MAX_EMAILS {
            throttle.failed(&format!("u{i}@x.test"), start);
        }
        // Nothing has expired: the stalest email makes room.
        throttle.failed("late@x.test", start + Duration::from_secs(1));
        assert_eq!(throttle.lock().len(), MAX_EMAILS);
        // Everything older than the window is dropped at once.
        throttle.failed("later@x.test", start + WINDOW);
        assert_eq!(throttle.lock().len(), 2);
    }
}
