//! Who is calling: the session cookie, the `Signed` extractor and the sign-in throttle. The
//! same-origin guard is [`crate::deployment::Deployment::same_origin`].

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header;
use axum::http::request::Parts;
use records::{Session, SessionToken};

use crate::AppState;
use crate::api::ApiError;
use crate::deployment::Deployment;

/// A caller with a live session. Rejects with `401 {"error":"not_signed_in"}` when the request
/// has no session cookie, or one that is unknown, ended, expired or belongs to a disabled
/// account.
pub(crate) struct Signed {
    pub session: Session,
}

impl FromRequestParts<AppState> for Signed {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = session_token(&parts.headers, state.deployment().cookie_name())
            .ok_or(ApiError::NotSignedIn)?;
        match state.db().session(&token).await {
            Ok(Some(session)) => Ok(Signed { session }),
            Ok(None) => Err(ApiError::NotSignedIn),
            Err(e) => Err(ApiError::unavailable(&e)),
        }
    }
}

/// A caller who may or may not be signed in: `Some` with a live session, `None` without a
/// session cookie or with one that is unknown, ended, expired or a disabled account's. A
/// database failure is still answered `503`, never taken for "not signed in".
pub(crate) struct MaybeSigned(pub Option<Signed>);

impl MaybeSigned {
    /// The caller of a request with `headers`.
    pub(crate) async fn of(headers: &HeaderMap, state: &AppState) -> Result<Self, ApiError> {
        let Some(token) = session_token(headers, state.deployment().cookie_name()) else {
            return Ok(MaybeSigned(None));
        };
        match state.db().session(&token).await {
            Ok(session) => Ok(MaybeSigned(session.map(|session| Signed { session }))),
            Err(e) => Err(ApiError::unavailable(&e)),
        }
    }
}

impl FromRequestParts<AppState> for MaybeSigned {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        MaybeSigned::of(&parts.headers, state).await
    }
}

/// The session token in the request's cookies. Malformed values are ignored.
fn session_token(headers: &HeaderMap, cookie: &str) -> Option<SessionToken> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, _)| *name == cookie)
        .find_map(|(_, value)| SessionToken::parse(value.trim()))
}

/// The `Set-Cookie` value that stores `token` for the session's lifetime: in production mode
/// `__Host-sharecraft_session`, `Secure`.
pub(crate) fn session_cookie(deployment: &Deployment, token: &SessionToken) -> String {
    format!(
        "{}={}; {}; Max-Age={}",
        deployment.cookie_name(),
        token.as_cookie_value(),
        deployment.cookie_attributes(),
        records::SESSION_LIFETIME.whole_seconds()
    )
}

/// The `Set-Cookie` value that removes the session cookie.
pub(crate) fn cleared_cookie(deployment: &Deployment) -> String {
    format!(
        "{}=; {}; Max-Age=0",
        deployment.cookie_name(),
        deployment.cookie_attributes()
    )
}

/// Failed sign-ins allowed per email within [`WINDOW`].
const MAX_FAILURES: usize = 5;
/// Failed sign-ins allowed from one client address, for any accounts, within [`WINDOW`].
pub(crate) const MAX_ADDRESS_FAILURES: usize = 30;
/// Unknown anyone-link tokens one client address may ask about within [`WINDOW`].
pub(crate) const MAX_LINK_MISSES: usize = 60;
/// How long a failure counts against its key.
pub(crate) const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Most keys a throttle remembers.
const MAX_EMAILS: usize = 10_000;
/// Longest email address an account can have; longer keys are cut to this.
const MAX_KEY_CHARS: usize = 254;

/// Recent failures per key (an email, or a client address), in memory. Attempts still being
/// checked count against the limit too, so simultaneous requests cannot get past it: after
/// `limit` failures and attempts in progress within [`WINDOW`], a key is refused until the
/// oldest failure is [`WINDOW`] old. While refused, even the right password is refused.
pub(crate) struct Throttle {
    entries: Mutex<HashMap<String, Entry>>,
    limit: usize,
}

impl Default for Throttle {
    /// The per-email throttle: 5 failures in 15 minutes.
    fn default() -> Self {
        Throttle::new(MAX_FAILURES)
    }
}

/// What the throttle knows about one key.
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

/// A sign-in attempt holding one of its key's slots. Dropping it, also when the request is
/// cancelled, gives the slot back; [`Attempt::failed`] first records a failure.
pub(crate) struct Attempt<'a> {
    throttle: &'a Throttle,
    key: String,
}

impl Attempt<'_> {
    /// Counts this attempt as a failed sign-in at `now`. True when this failure is the one that
    /// reached the limit, so the key is locked from now on: the moment to record the lock once.
    pub(crate) fn failed(self, now: Instant) -> bool {
        let limit = self.throttle.limit;
        let mut entries = self.throttle.lock();
        let Some(entry) = entries.get_mut(&self.key) else {
            return false;
        };
        forget_expired(&mut entry.failures, now);
        let before = entry.failures.len();
        entry.failures.push_back(now);
        while entry.failures.len() > limit {
            entry.failures.pop_front();
        }
        before < limit && entry.failures.len() >= limit
    }

    /// Forgets the key's failures after a successful sign-in.
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
    /// A throttle allowing `limit` failures per key within [`WINDOW`].
    pub(crate) fn new(limit: usize) -> Self {
        Throttle {
            entries: Mutex::default(),
            limit,
        }
    }

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

    /// The key for a client address: an IPv4 address itself, an IPv6 address as its `/64`, since
    /// one person or household usually holds a whole `/64`.
    pub(crate) fn address_key(ip: IpAddr) -> String {
        match crate::deployment::canonical(ip) {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => {
                let prefix = u128::from(v6) & (u128::MAX << 64);
                format!("{}/64", Ipv6Addr::from(prefix))
            }
        }
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
        if entry.failures.len() + entry.in_flight >= self.limit {
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

    /// How long until `key` may try again: until its oldest failure is [`WINDOW`] old, at least
    /// one second (attempts in progress are over within seconds).
    pub(crate) fn retry_after(&self, key: &str, now: Instant) -> Duration {
        let entries = self.lock();
        let wait = entries
            .get(key)
            .and_then(|entry| entry.failures.front())
            .map_or(Duration::ZERO, |oldest| {
                WINDOW.saturating_sub(now.saturating_duration_since(*oldest))
            });
        wait.max(Duration::from_secs(1))
    }

    /// Whether `key` has reached the limit with failures alone. For counts where attempts in
    /// progress do not matter, such as anyone-link lookups.
    pub(crate) fn blocked(&self, key: &str, now: Instant) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(key) else {
            return false;
        };
        forget_expired(&mut entry.failures, now);
        let blocked = entry.failures.len() >= self.limit;
        if entry.is_idle() {
            entries.remove(key);
        }
        blocked
    }

    /// Counts a failure for `key` at `now`, outside any attempt.
    pub(crate) fn note_failure(&self, key: &str, now: Instant) {
        let mut entries = self.lock();
        if !entries.contains_key(key) && entries.len() >= MAX_EMAILS {
            make_room(&mut entries, now);
        }
        let entry = entries.entry(key.to_owned()).or_default();
        forget_expired(&mut entry.failures, now);
        entry.failures.push_back(now);
        while entry.failures.len() > self.limit {
            entry.failures.pop_front();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // The map holds no invariant a panic elsewhere could break.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How long a sign-in waits for a password check when every one is busy.
pub(crate) const PASSWORD_WAIT: Duration = Duration::from_secs(10);

/// The password checks that may run at once: as many as the computer has processor cores, at
/// least 2. Each check (Argon2) is deliberately slow, so a flood of guesses would otherwise take
/// the whole computer; beyond this, sign-ins wait up to [`PASSWORD_WAIT`] and then answer
/// `503 busy`.
pub(crate) struct PasswordChecks {
    turns: std::sync::Arc<tokio::sync::Semaphore>,
    size: usize,
    wait: Mutex<Duration>,
}

impl Default for PasswordChecks {
    fn default() -> Self {
        let size = std::thread::available_parallelism()
            .map_or(2, std::num::NonZeroUsize::get)
            .max(2);
        PasswordChecks {
            turns: std::sync::Arc::new(tokio::sync::Semaphore::new(size)),
            size,
            wait: Mutex::new(PASSWORD_WAIT),
        }
    }
}

impl PasswordChecks {
    /// A turn for one password check, or `None` after waiting the whole wait.
    pub(crate) async fn turn(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let wait = *self.wait.lock().unwrap_or_else(PoisonError::into_inner);
        tokio::time::timeout(wait, std::sync::Arc::clone(&self.turns).acquire_owned())
            .await
            .ok()?
            .ok()
    }

    /// Takes every turn until the answer is dropped. For tests.
    pub(crate) async fn hold_all(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let size = u32::try_from(self.size).ok()?;
        std::sync::Arc::clone(&self.turns)
            .acquire_many_owned(size)
            .await
            .ok()
    }

    /// Sets how long a sign-in waits for a turn. For tests.
    pub(crate) fn set_wait(&self, wait: Duration) {
        *self.wait.lock().unwrap_or_else(PoisonError::into_inner) = wait;
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

    const COOKIE: &str = "sharecraft_session";
    const PRODUCTION_COOKIE: &str = "__Host-sharecraft_session";

    #[test]
    fn session_token_is_found_among_cookies() {
        let token = "ab".repeat(32);
        let mut h = HeaderMap::new();
        h.append(header::COOKIE, HeaderValue::from_static("a=1"));
        h.append(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "b=2; {COOKIE}=bad; {COOKIE}={token}; __Host-{COOKIE}=x"
            ))
            .unwrap(),
        );
        assert_eq!(session_token(&h, COOKIE).unwrap().as_cookie_value(), token);
        // Production reads only the __Host- cookie: a development cookie is never accepted.
        assert!(session_token(&h, PRODUCTION_COOKIE).is_none());
        let mut prod = HeaderMap::new();
        prod.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{COOKIE}={token}; {PRODUCTION_COOKIE}={token}"))
                .unwrap(),
        );
        assert_eq!(
            session_token(&prod, PRODUCTION_COOKIE)
                .unwrap()
                .as_cookie_value(),
            token
        );

        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("sharecraft_session=zz"),
        );
        assert!(session_token(&h, COOKIE).is_none());
        assert!(session_token(&HeaderMap::new(), COOKIE).is_none());
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
    fn address_throttle_allows_thirty_and_says_how_long_to_wait() {
        let throttle = Throttle::new(MAX_ADDRESS_FAILURES);
        let start = Instant::now();
        let key = Throttle::address_key("198.51.100.7".parse().unwrap());
        let mut locked = Vec::new();
        for i in 0..MAX_ADDRESS_FAILURES {
            let at = start + Duration::from_secs(i as u64);
            locked.push(throttle.try_begin(&key, at).unwrap().failed(at));
        }
        // Only the failure that reached the limit reports the lock.
        assert_eq!(locked.iter().filter(|l| **l).count(), 1);
        assert!(locked[MAX_ADDRESS_FAILURES - 1]);
        let now = start + Duration::from_secs(60);
        assert!(throttle.try_begin(&key, now).is_none());
        assert_eq!(
            throttle.retry_after(&key, now),
            WINDOW - Duration::from_secs(60)
        );
        assert_eq!(throttle.retry_after("nobody", now), Duration::from_secs(1));
    }

    #[test]
    fn ipv6_addresses_count_per_64() {
        let a = Throttle::address_key("2001:db8:1:2:aaaa::1".parse().unwrap());
        let b = Throttle::address_key("2001:db8:1:2:ffff::9".parse().unwrap());
        let c = Throttle::address_key("2001:db8:1:3::1".parse().unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, "2001:db8:1:2::/64");
        assert_eq!(
            Throttle::address_key("::ffff:198.51.100.7".parse().unwrap()),
            "198.51.100.7"
        );
    }

    #[test]
    fn failures_only_count_for_link_lookups() {
        let throttle = Throttle::new(3);
        let now = Instant::now();
        assert!(!throttle.blocked("a", now));
        for _ in 0..3 {
            throttle.note_failure("a", now);
        }
        assert!(throttle.blocked("a", now));
        assert!(!throttle.blocked("b", now));
        assert!(!throttle.blocked("a", now + WINDOW));
        assert!(throttle.lock().is_empty());
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
