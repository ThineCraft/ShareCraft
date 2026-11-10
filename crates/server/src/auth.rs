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

/// Recent failed sign-ins per email, in memory. Attempts still being checked count against the
/// limit too, so simultaneous requests cannot get past it: after [`MAX_FAILURES`] failures and
/// attempts in progress within [`WINDOW`], an email is refused until the oldest failure is
/// [`WINDOW`] old.
#[derive(Default)]
pub(crate) struct Throttle {
    entries: Mutex<HashMap<String, Entry>>,
}

/// What the throttle knows about one email.
#[derive(Default)]
struct Entry {
    /// Times of recent failures, oldest first.
    failures: VecDeque<Instant>,
    /// Attempts begun and not yet finished.
    in_flight: usize,
}

impl Entry {
    fn is_idle(&self) -> bool {
        self.failures.is_empty() && self.in_flight == 0
    }
}

/// A sign-in attempt holding one of its email's slots. Dropping it, also when the request is
/// cancelled, gives the slot back; [`Attempt::failed`] first records a failure.
pub(crate) struct Attempt<'a> {
    throttle: &'a Throttle,
    key: String,
}

impl Attempt<'_> {
    /// Counts this attempt as a failed sign-in at `now`.
    pub(crate) fn failed(self, now: Instant) {
        let mut entries = self.throttle.lock();
        if let Some(entry) = entries.get_mut(&self.key) {
            forget_expired(&mut entry.failures, now);
            entry.failures.push_back(now);
            while entry.failures.len() > MAX_FAILURES {
                entry.failures.pop_front();
            }
        }
    }

    /// Forgets the email's failures after a successful sign-in.
    pub(crate) fn succeeded(self) {
        if let Some(entry) = self.throttle.lock().get_mut(&self.key) {
            entry.failures.clear();
        }
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        let mut entries = self.throttle.lock();
        if let Some(entry) = entries.get_mut(&self.key) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
            if entry.is_idle() {
                entries.remove(&self.key);
            }
        }
    }
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

    /// Starts a sign-in attempt for `key` at `now`, or `None` when recent failures plus
    /// attempts in progress already reach the limit.
    pub(crate) fn try_begin(&self, key: &str, now: Instant) -> Option<Attempt<'_>> {
        let mut entries = self.lock();
        if !entries.contains_key(key) && entries.len() >= MAX_EMAILS {
            make_room(&mut entries, now);
        }
        let entry = entries.entry(key.to_owned()).or_default();
        forget_expired(&mut entry.failures, now);
        if entry.failures.len() + entry.in_flight >= MAX_FAILURES {
            if entry.is_idle() {
                entries.remove(key);
            }
            return None;
        }
        entry.in_flight += 1;
        Some(Attempt {
            throttle: self,
            key: key.to_owned(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // The map holds no invariant a panic elsewhere could break.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
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

/// Drops every email with no unexpired failure and no attempt in progress. If that frees
/// nothing, drops the idle email whose latest failure is oldest.
///
/// This is a trade-off: someone who fails sign-ins for 10,000 other emails within the window
/// can push out, and so reset, another email's count. The alternative, refusing new emails
/// while the table is full, would let the same flood lock everyone out. The count is a brake
/// on guessing, and Argon2 keeps each guess slow either way.
fn make_room(entries: &mut HashMap<String, Entry>, now: Instant) {
    entries.retain(|_, entry| {
        forget_expired(&mut entry.failures, now);
        !entry.is_idle()
    });
    if entries.len() < MAX_EMAILS {
        return;
    }
    let stalest = entries
        .iter()
        .filter(|(_, entry)| entry.in_flight == 0)
        .min_by_key(|(_, entry)| entry.failures.back().copied())
        .map(|(key, _)| key.clone());
    if let Some(key) = stalest {
        entries.remove(&key);
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

    /// Whether an attempt may start; the attempt is given back at once.
    fn allows(throttle: &Throttle, key: &str, now: Instant) -> bool {
        throttle.try_begin(key, now).is_some()
    }

    fn fail(throttle: &Throttle, key: &str, now: Instant) {
        throttle.try_begin(key, now).unwrap().failed(now);
    }

    #[test]
    fn throttle_refuses_after_five_failures_until_the_oldest_expires() {
        let throttle = Throttle::default();
        let start = Instant::now();
        let key = Throttle::key(" Ann@X.test ");
        assert_eq!(key, "ann@x.test");
        for i in 0..5 {
            fail(&throttle, &key, start + Duration::from_secs(i * 60));
        }
        let later = start + Duration::from_secs(5 * 60);
        assert!(!allows(&throttle, &key, later));
        assert!(allows(&throttle, "bob@x.test", later));
        // The first failure expires at start + WINDOW.
        assert!(!allows(
            &throttle,
            &key,
            start + WINDOW - Duration::from_secs(1)
        ));
        let attempt = throttle.try_begin(&key, start + WINDOW).unwrap();
        attempt.succeeded();
        assert!(allows(&throttle, &key, later));
        // Nothing is left behind once the email is idle.
        assert!(throttle.lock().is_empty());
    }

    #[test]
    fn attempts_in_progress_count_against_the_limit() {
        let throttle = Throttle::default();
        let now = Instant::now();
        fail(&throttle, "ann@x.test", now);
        let held: Vec<Attempt<'_>> = (0..4)
            .map(|_| throttle.try_begin("ann@x.test", now).unwrap())
            .collect();
        assert!(throttle.try_begin("ann@x.test", now).is_none());
        // Dropping an attempt (a cancelled request, or a database error) gives its slot back.
        drop(held);
        assert!(allows(&throttle, "ann@x.test", now));
        assert_eq!(throttle.lock()["ann@x.test"].in_flight, 0);
        assert_eq!(throttle.lock()["ann@x.test"].failures.len(), 1);
    }

    #[test]
    fn throttle_table_is_bounded() {
        let throttle = Throttle::default();
        let start = Instant::now();
        for i in 0..MAX_EMAILS {
            fail(&throttle, &format!("u{i}@x.test"), start);
        }
        // Nothing has expired: the stalest email makes room.
        fail(&throttle, "late@x.test", start + Duration::from_secs(1));
        assert_eq!(throttle.lock().len(), MAX_EMAILS);
        // Everything older than the window is dropped at once.
        fail(&throttle, "later@x.test", start + WINDOW);
        assert_eq!(throttle.lock().len(), 2);
    }
}
