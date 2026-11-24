//! The S3 HTTP client: the only code that sends a request to an S3 endpoint.
//!
//! One `reqwest` client per approved endpoint, built lazily and kept: operating-system TLS,
//! HTTPS only (except the loopback test form), no redirects, no proxy, no cookies, and a DNS
//! resolver of our own that drops every address [`address_allowed`] refuses, so the check is
//! made at connection time for every name the client connects to. An address literal never
//! reaches a resolver, so it is checked here before each request.
//!
//! Every request is signed with Signature Version 4 at the current time and sends exactly the
//! headers it signed. Errors carry no text: an S3 error body is read (at most 64 KiB) only for
//! its `Code` word, which is written to the server log; its `Message` is never kept.
//!
//! Phase 1: no method here sends `PUT`, `POST` or `DELETE`.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use collab_core::ids::NodeId;
use records::{
    IpRange, S3_ENDPOINTS, S3Endpoint, Usage, Versioning, address_allowed, find_endpoint,
    valid_bucket,
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, redirect};
use time::OffsetDateTime;

use revisions::BlobError;

use crate::AppState;
use crate::blobs::{Blobs, ChannelReader, Held};
use crate::s3_sign::{self, Credentials, EMPTY_SHA256, ToSign};
use crate::s3_xml::{self, ListPage};
use crate::source_sync::SyncError;

/// How long a connection may take to open.
pub const CONNECT_WAIT: Duration = Duration::from_secs(10);
/// How long an answer may go without sending a byte.
pub const IDLE_WAIT: Duration = Duration::from_secs(30);
/// How many times a throttled or failed request is tried again.
pub const RETRIES: u32 = 3;

/// The first wait before a retry; each later wait is twice the one before (0.5, 1, 2 s).
const FIRST_RETRY_WAIT: Duration = Duration::from_millis(500);
/// The longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
/// The most of an error body read for its code.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// The most of an XML document (a listing page, a versioning answer) read.
const MAX_DOCUMENT: usize = 8 * 1024 * 1024;
const USER_AGENT: &str = "ShareCraft";

/// Why an S3 request failed. It carries no text from the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum S3Error {
    #[error("the object is not in the bucket")]
    NotFound,
    #[error("the bucket does not exist")]
    NoBucket,
    #[error("the store refused access")]
    Denied,
    #[error("the store did not accept the access key")]
    BadKeys,
    #[error("the bucket is in another region")]
    WrongRegion,
    #[error("the server's clock differs too much from the store's")]
    ClockSkew,
    #[error("the store is asking for fewer requests")]
    Throttled,
    #[error("the object changed")]
    Precondition,
    #[error("the object is archived")]
    Archived,
    #[error("the store does not support this request")]
    Unsupported,
    #[error("the endpoint's address is not allowed")]
    AddressRefused,
    #[error("the store could not be reached")]
    Unreachable,
    #[error("the store stopped answering")]
    TimedOut,
    #[error("the store's answer could not be read")]
    Malformed,
    #[error("the request to the store failed")]
    Other,
}

/// What a request is counted as against a library's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    List,
    Read,
    Write,
}

/// What `HEAD` (or the headers of a `GET`) says about an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHead {
    /// The object's whole size, also for a range.
    pub size: u64,
    /// Exactly as received, quotes included.
    pub etag: String,
    pub modified_s: i64,
    pub class: Option<String>,
    /// True only when a restore of an archived object has finished.
    pub restored: bool,
    pub version: Option<String>,
}

/// Where requests for one bucket go and how they are signed.
#[derive(Clone)]
pub struct S3Target {
    endpoint: S3Endpoint,
    bucket: String,
    prefix: String,
    credentials: Credentials,
    library: Option<NodeId>,
}

impl fmt::Debug for S3Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Target")
            .field("endpoint", &self.endpoint.name)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("library", &self.library)
            .finish_non_exhaustive()
    }
}

impl S3Target {
    /// The approved endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &S3Endpoint {
        &self.endpoint
    }

    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The library's prefix in the bucket: empty, or ending in `/`.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// The library whose budget the requests are counted against, if any.
    #[must_use]
    pub fn library(&self) -> Option<NodeId> {
        self.library
    }

    /// The same target, its requests counted against `library`.
    #[must_use]
    pub fn with_library(mut self, library: NodeId) -> S3Target {
        self.library = Some(library);
        self
    }
}

#[derive(Default)]
struct Meter {
    usage: Mutex<HashMap<NodeId, Usage>>,
}

impl Meter {
    fn hit(&self, library: NodeId) {
        let mut usage = lock(&self.usage);
        let entry = usage.entry(library).or_default();
        entry.cache_hits = entry.cache_hits.saturating_add(1);
    }

    fn add(&self, library: NodeId, class: Class, requests: u64, bytes_down: u64) {
        let mut usage = lock(&self.usage);
        let entry = usage.entry(library).or_default();
        let count = match class {
            Class::List => &mut entry.list,
            Class::Read => &mut entry.read,
            Class::Write => &mut entry.write,
        };
        *count = count.saturating_add(requests);
        entry.bytes_down = entry.bytes_down.saturating_add(bytes_down);
    }
}

#[derive(Clone, Copy)]
struct Waits {
    connect: Duration,
    idle: Duration,
    retry: Duration,
}

/// How many objects may be read from S3 at once across the server. Its own permits: a hung file
/// share must not starve S3, nor the reverse.
pub const S3_READS: usize = 4;

/// The S3 client of a running server: clients per endpoint, the usage meter, the read permits
/// and the offline memory of each library.
pub struct S3 {
    read: Arc<tokio::sync::Semaphore>,
    read_wait: Mutex<Duration>,
    offline_until: Mutex<HashMap<NodeId, Instant>>,
    plain_loopback: bool,
    clients: Mutex<HashMap<String, reqwest::Client>>,
    waits: Mutex<Waits>,
    overrides: Arc<Mutex<HashMap<String, SocketAddr>>>,
    meter: Arc<Meter>,
    listing_progress: Mutex<HashMap<NodeId, Arc<AtomicUsize>>>,
    list_time_for_tests: Mutex<Option<Duration>>,
}

/// The files seen so far by a bucket listing in progress; forgotten when dropped.
pub(crate) struct ListingProgress<'a> {
    owner: &'a S3,
    library: NodeId,
    count: Arc<AtomicUsize>,
}

impl ListingProgress<'_> {
    pub(crate) fn set(&self, files: usize) {
        self.count.store(files, Ordering::SeqCst);
    }
}

impl Drop for ListingProgress<'_> {
    fn drop(&mut self) {
        let mut progress = lock(&self.owner.listing_progress);
        if progress
            .get(&self.library)
            .is_some_and(|current| Arc::ptr_eq(current, &self.count))
        {
            progress.remove(&self.library);
        }
    }
}

impl fmt::Debug for S3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("S3")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The resolver's refusal, found again in the error chain of the failed request.
#[derive(Debug)]
struct Refused;

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("every address of the name is refused")
    }
}

impl StdError for Refused {}

type BoxError = Box<dyn StdError + Send + Sync>;

/// Resolves names with the operating system and keeps only the allowed addresses.
struct CheckedResolver {
    allow: Vec<IpRange>,
    overrides: Arc<Mutex<HashMap<String, SocketAddr>>>,
}

impl Resolve for CheckedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let fixed = lock(&self.overrides).get(&host).copied();
        Box::pin(resolve_checked(host, fixed, self.allow.clone()))
    }
}

async fn resolve_checked(
    host: String,
    fixed: Option<SocketAddr>,
    allow: Vec<IpRange>,
) -> Result<Addrs, BoxError> {
    let found: Vec<SocketAddr> = match fixed {
        Some(addr) => vec![addr],
        None => tokio::net::lookup_host((host.as_str(), 0)).await?.collect(),
    };
    let allowed: Vec<SocketAddr> = found
        .into_iter()
        .filter(|addr| address_allowed(addr.ip(), &allow))
        .collect();
    if allowed.is_empty() {
        return Err(Box::new(Refused));
    }
    Ok(Box::new(allowed.into_iter()))
}

fn refused_by_resolver(error: &reqwest::Error) -> bool {
    let mut next: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(error) = next {
        if error.is::<Refused>() {
            return true;
        }
        next = error.source();
    }
    false
}

fn transport(error: &reqwest::Error) -> S3Error {
    if error.is_timeout() {
        S3Error::TimedOut
    } else if error.is_body() || error.is_decode() {
        S3Error::Malformed
    } else {
        S3Error::Unreachable
    }
}

/// The error an S3 status and error code mean.
fn mapped(status: u16, code: Option<&str>) -> S3Error {
    match (status, code) {
        (_, Some("NoSuchBucket")) => S3Error::NoBucket,
        (501, _) | (_, Some("NotImplemented")) => S3Error::Unsupported,
        (400 | 403, Some("InvalidObjectState")) => S3Error::Archived,
        (403, Some("InvalidAccessKeyId" | "SignatureDoesNotMatch")) => S3Error::BadKeys,
        (403, Some("RequestTimeTooSkewed")) => S3Error::ClockSkew,
        (403, _) => S3Error::Denied,
        (404, _) => S3Error::NotFound,
        (301 | 307, _)
        | (400, Some("AuthorizationHeaderMalformed" | "PermanentRedirect" | "InvalidRegionName")) => {
            S3Error::WrongRegion
        }
        (412, _) => S3Error::Precondition,
        (429 | 500 | 502 | 503, _) => S3Error::Throttled,
        _ => S3Error::Other,
    }
}

fn retried(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503)
}

/// A code fit for the log: a short word of letters and digits, or nothing.
fn loggable(code: &str) -> Option<&str> {
    (!code.is_empty() && code.len() <= 64 && code.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(code)
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    header_text(headers, "content-length").and_then(|text| text.trim().parse().ok())
}

/// The object's details from the headers of a `HEAD` or `GET` answer.
fn object_head(headers: &HeaderMap, size: u64) -> Result<ObjectHead, S3Error> {
    let etag = header_text(headers, "etag").ok_or(S3Error::Malformed)?;
    if etag.is_empty() || etag.len() > 1024 {
        return Err(S3Error::Malformed);
    }
    let modified_s = header_text(headers, "last-modified")
        .and_then(s3_xml::http_date)
        .ok_or(S3Error::Malformed)?;
    let restored = header_text(headers, "x-amz-restore")
        .is_some_and(|text| text.contains("ongoing-request=\"false\""));
    Ok(ObjectHead {
        size,
        etag: etag.to_owned(),
        modified_s,
        class: header_text(headers, "x-amz-storage-class").map(str::to_owned),
        restored,
        version: header_text(headers, "x-amz-version-id").map(str::to_owned),
    })
}

/// `bytes first-last/total` of a `Content-Range` header.
fn content_range(headers: &HeaderMap) -> Option<(u64, u64, u64)> {
    let text = header_text(headers, "content-range")?.strip_prefix("bytes ")?;
    let (span, total) = text.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    Some((first.parse().ok()?, last.parse().ok()?, total.parse().ok()?))
}

/// The URL base (scheme and authority), the encoded path, and the `Host` header of a request.
fn parts(t: &S3Target, key: Option<&str>) -> (String, String, String) {
    let endpoint = &t.endpoint;
    let scheme = if endpoint.https() { "https" } else { "http" };
    let authority = endpoint
        .url
        .split_once("://")
        .map_or(endpoint.url.as_str(), |(_, rest)| rest);
    let key_path = key.map(s3_sign::key_path);
    // A dotted bucket breaks the certificate name; an address literal has no virtual-host
    // form (`bucket.127.0.0.1` is not a host name a URL accepts).
    let literal = endpoint.host().parse::<IpAddr>().is_ok() || endpoint.host().contains(':');
    let path_style = endpoint.path_style || t.bucket.contains('.') || literal;
    if path_style {
        let path = match key_path {
            Some(key) => format!("/{}{key}", t.bucket),
            None => format!("/{}", t.bucket),
        };
        (
            format!("{scheme}://{authority}"),
            path,
            authority.to_owned(),
        )
    } else {
        let host = format!("{}.{authority}", t.bucket);
        let path = key_path.unwrap_or_else(|| "/".to_owned());
        (format!("{scheme}://{host}"), path, host)
    }
}

/// A key the client will not ask for: empty, or with a `.` or `..` segment, which a URL
/// would rewrite into another key.
fn unaskable(key: &str) -> bool {
    key.is_empty()
        || key
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

/// An answer's body, read a chunk at a time, each within the idle limit, and checked against
/// the length the answer promised.
pub struct Body {
    response: reqwest::Response,
    idle: Duration,
    expected: Option<u64>,
    received: u64,
    meter: Option<(Arc<Meter>, NodeId)>,
    done: bool,
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Body")
            .field("expected", &self.expected)
            .field("received", &self.received)
            .finish_non_exhaustive()
    }
}

impl Body {
    /// The length the answer promised, if it named one.
    #[must_use]
    pub fn length(&self) -> Option<u64> {
        self.expected
    }

    /// The next chunk; `None` at the end. A chunk that does not come within the idle limit is
    /// `TimedOut`; a body shorter or longer than promised is `Malformed`.
    ///
    /// # Errors
    /// As above, or `Malformed` for a body that cannot be read.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, S3Error> {
        if self.done {
            return Ok(None);
        }
        let next = tokio::time::timeout(self.idle, self.response.chunk()).await;
        match next {
            Err(_) => {
                self.done = true;
                Err(S3Error::TimedOut)
            }
            Ok(Err(e)) => {
                self.done = true;
                Err(if e.is_timeout() {
                    S3Error::TimedOut
                } else {
                    S3Error::Malformed
                })
            }
            Ok(Ok(Some(bytes))) => {
                let n = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                self.received = self.received.saturating_add(n);
                if let Some((meter, library)) = &self.meter {
                    meter.add(*library, Class::Read, 0, n);
                }
                if self
                    .expected
                    .is_some_and(|expected| self.received > expected)
                {
                    self.done = true;
                    return Err(S3Error::Malformed);
                }
                Ok(Some(bytes))
            }
            Ok(Ok(None)) => {
                self.done = true;
                if self
                    .expected
                    .is_some_and(|expected| self.received != expected)
                {
                    return Err(S3Error::Malformed);
                }
                Ok(None)
            }
        }
    }

    /// The whole body, at most `cap` bytes: `Malformed` beyond.
    async fn read_capped(mut self, cap: usize) -> Result<Vec<u8>, S3Error> {
        let mut out = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            if out.len().saturating_add(chunk.len()) > cap {
                return Err(S3Error::Malformed);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// At most `cap` bytes of the body, ignoring what does not arrive or does not fit.
    async fn read_some(mut self, cap: usize) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(Some(chunk)) = self.chunk().await {
            let room = cap.saturating_sub(out.len());
            out.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
            if out.len() >= cap {
                break;
            }
        }
        out
    }
}

impl S3 {
    /// A client; `plain_loopback` lets an endpoint be `http://127.0.0.1:<port>` (tests only).
    #[must_use]
    pub fn new(plain_loopback: bool) -> S3 {
        S3 {
            read: Arc::new(tokio::sync::Semaphore::new(S3_READS)),
            read_wait: Mutex::new(crate::external::READ_WAIT),
            offline_until: Mutex::new(HashMap::new()),
            plain_loopback,
            clients: Mutex::new(HashMap::new()),
            waits: Mutex::new(Waits {
                connect: CONNECT_WAIT,
                idle: IDLE_WAIT,
                retry: FIRST_RETRY_WAIT,
            }),
            overrides: Arc::new(Mutex::new(HashMap::new())),
            meter: Arc::new(Meter::default()),
            listing_progress: Mutex::new(HashMap::new()),
            list_time_for_tests: Mutex::new(None),
        }
    }

    /// An S3 read permit, waited for at most the read wait; `None` when none came in time.
    /// Dropping the permit (also by cancelling the read holding it) gives it back.
    pub(crate) async fn read_turn(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let wait = *lock(&self.read_wait);
        tokio::time::timeout(wait, Arc::clone(&self.read).acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
    }

    /// Sets how long a read waits for a permit before answering busy, without changing its
    /// 20-second production default.
    pub fn set_read_wait(&self, wait: Duration) {
        *lock(&self.read_wait) = wait;
    }

    /// The read permits free now, for tests.
    #[doc(hidden)]
    #[must_use]
    pub fn read_permits_for_tests(&self) -> usize {
        self.read.available_permits()
    }

    /// Whether `library`'s store was found unreachable recently enough that no request is sent.
    pub(crate) fn offline(&self, library: NodeId) -> bool {
        let now = Instant::now();
        let mut offline = lock(&self.offline_until);
        offline.retain(|_, until| *until > now);
        offline.get(&library).is_some_and(|until| *until > now)
    }

    /// Remembers `library`'s store as unreachable for `for_time`.
    pub(crate) fn mark_offline(&self, library: NodeId, for_time: Duration) {
        lock(&self.offline_until).insert(library, Instant::now() + for_time);
    }

    /// Counts one read answered from the cache against `library`.
    pub(crate) fn count_cache_hit(&self, library: NodeId) {
        self.meter.hit(library);
    }

    /// Files seen so far by a listing of `library`'s bucket currently in progress.
    #[must_use]
    pub fn listing_progress(&self, library: NodeId) -> Option<usize> {
        lock(&self.listing_progress)
            .get(&library)
            .map(|count| count.load(Ordering::SeqCst))
    }

    pub(crate) fn begin_listing(&self, library: NodeId) -> ListingProgress<'_> {
        let count = Arc::new(AtomicUsize::new(0));
        lock(&self.listing_progress).insert(library, Arc::clone(&count));
        ListingProgress {
            owner: self,
            library,
            count,
        }
    }

    /// The longest a listing pass may take: `configured`, unless a test set another.
    pub(crate) fn list_time(&self, configured: Duration) -> Duration {
        lock(&self.list_time_for_tests).unwrap_or(configured)
    }

    /// Sets (or with `None` clears) the listing time limit, for tests; the stored setting is
    /// unchanged.
    pub fn set_list_time_for_tests(&self, limit: Option<Duration>) {
        *lock(&self.list_time_for_tests) = limit;
    }

    /// What has been counted against `library` and not yet taken by [`S3::take_usage`].
    #[must_use]
    pub fn unflushed_usage(&self, library: NodeId) -> Usage {
        lock(&self.meter.usage)
            .get(&library)
            .copied()
            .unwrap_or_default()
    }

    /// The endpoint `name` of the stored approvals, read with the plain-HTTP loopback form
    /// allowed. Only a client built with that test switch looks.
    async fn loopback_endpoint(&self, state: &AppState, name: &str) -> Option<S3Endpoint> {
        if !self.plain_loopback {
            return None;
        }
        let value = state.db().setting(S3_ENDPOINTS).await.ok()??;
        value.as_array()?.iter().find_map(|entry| {
            let object = entry.as_object()?;
            if object.get("name")?.as_str()? != name {
                return None;
            }
            let allow = object
                .get("allow")?
                .as_array()?
                .iter()
                .map(|range| range.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()?;
            S3Endpoint::parse(
                name,
                object.get("url")?.as_str()?,
                object.get("region")?.as_str()?,
                object.get("path_style")?.as_bool()?,
                &allow,
                true,
            )
            .ok()
        })
    }

    /// The approved endpoint named `name` (a built-in `aws:<region>` or one of the stored
    /// approvals), read afresh; `None` when there is none of that name.
    ///
    /// # Errors
    /// When the settings cannot be read.
    pub(crate) async fn approved_endpoint(
        &self,
        state: &AppState,
        name: &str,
    ) -> Result<Option<S3Endpoint>, records::DbError> {
        let settings = state.db().s3_settings().await?;
        Ok(match find_endpoint(name, &settings) {
            Some(endpoint) => Some(endpoint),
            None => self.loopback_endpoint(state, name).await,
        })
    }

    /// The target of the S3 library `library`: its place, its approved endpoint and its
    /// unsealed key. `AddressRefused` if the endpoint is no longer approved; `BadKeys` if the
    /// stored key cannot be opened.
    ///
    /// # Errors
    /// As above; `Other` if the library is not connected to a bucket or the database fails.
    pub async fn target(&self, state: &AppState, library: NodeId) -> Result<S3Target, S3Error> {
        let place = state
            .db()
            .s3_place(library)
            .await
            .map_err(|_| S3Error::Other)?
            .ok_or(S3Error::Other)?;
        let endpoint = self
            .approved_endpoint(state, &place.endpoint)
            .await
            .map_err(|_| S3Error::Other)?
            .ok_or(S3Error::AddressRefused)?;
        let (config, _) = state
            .db()
            .source_of(library)
            .await
            .map_err(|_| S3Error::Other)?
            .ok_or(S3Error::Other)?;
        let secret = crate::source_sync::token_of(state, &config)
            .await
            .map_err(|e| match e {
                SyncError::NotConnected(_) => S3Error::BadKeys,
                _ => S3Error::Other,
            })?
            .ok_or(S3Error::BadKeys)?;
        let credentials = Credentials {
            access_key_id: place.access_key_id,
            secret,
            session_token: None,
        };
        let mut target = self.target_from(&endpoint, &place.bucket, credentials);
        target.prefix = place.prefix;
        target.library = Some(library);
        Ok(target)
    }

    /// A target for `bucket` at `endpoint` with `creds`, counted against no library: for the
    /// connect probe.
    #[must_use]
    pub fn target_from(&self, endpoint: &S3Endpoint, bucket: &str, creds: Credentials) -> S3Target {
        S3Target {
            endpoint: endpoint.clone(),
            bucket: bucket.to_owned(),
            prefix: String::new(),
            credentials: creds,
            library: None,
        }
    }

    /// One `ListObjectsV2` page of keys starting with `prefix`, no delimiter.
    ///
    /// # Errors
    /// See [`S3Error`].
    pub async fn list_page(
        &self,
        t: &S3Target,
        prefix: &str,
        token: Option<&str>,
        max_keys: u16,
    ) -> Result<ListPage, S3Error> {
        let mut query = vec![
            ("list-type".to_owned(), "2".to_owned()),
            ("encoding-type".to_owned(), "url".to_owned()),
            ("max-keys".to_owned(), max_keys.to_string()),
            ("prefix".to_owned(), prefix.to_owned()),
        ];
        if let Some(token) = token {
            query.push(("continuation-token".to_owned(), token.to_owned()));
        }
        let response = self
            .send(t, Class::List, Method::GET, None, &query, &[])
            .await?;
        let text = self.document(t, response).await?;
        s3_xml::list_page(&text).map_err(|_| S3Error::Malformed)
    }

    /// `HEAD` of `key`.
    ///
    /// # Errors
    /// See [`S3Error`]; a bare `404` is `NotFound`.
    pub async fn head(&self, t: &S3Target, key: &str) -> Result<ObjectHead, S3Error> {
        let response = self
            .send(t, Class::Read, Method::HEAD, Some(key), &[], &[])
            .await?;
        let headers = response.headers();
        let size = content_length(headers).ok_or(S3Error::Malformed)?;
        object_head(headers, size)
    }

    /// `GET` of `key` with `If-Match: if_match`, and of only bytes `first..=last` when `range`
    /// is given. The answer's ETag must equal `if_match`.
    ///
    /// # Errors
    /// `Precondition` when the object changed; otherwise see [`S3Error`].
    pub async fn get(
        &self,
        t: &S3Target,
        key: &str,
        if_match: &str,
        range: Option<(u64, u64)>,
    ) -> Result<(ObjectHead, Body), S3Error> {
        let mut extra = vec![("if-match".to_owned(), if_match.to_owned())];
        if let Some((first, last)) = range {
            if first > last {
                return Err(S3Error::Other);
            }
            extra.push(("range".to_owned(), format!("bytes={first}-{last}")));
        }
        let response = self
            .send(t, Class::Read, Method::GET, Some(key), &[], &extra)
            .await?;
        let headers = response.headers();
        let length = content_length(headers).ok_or(S3Error::Malformed)?;
        let size = match (range, response.status()) {
            (None, StatusCode::OK) => length,
            (Some((first, last)), StatusCode::PARTIAL_CONTENT) => {
                let (from, to, total) = content_range(headers).ok_or(S3Error::Malformed)?;
                if from != first || to > last || to < from || to - from + 1 != length {
                    return Err(S3Error::Malformed);
                }
                total
            }
            _ => return Err(S3Error::Malformed),
        };
        let head = object_head(headers, size)?;
        if head.etag != if_match {
            return Err(S3Error::Precondition);
        }
        let body = self.body(t, response, Some(length));
        Ok((head, body))
    }

    /// The bucket's versioning state: `GET ?versioning`.
    ///
    /// # Errors
    /// See [`S3Error`]; a store without the call answers `Unsupported` (or `Denied`), which the
    /// caller records as `Unknown`.
    pub async fn bucket_versioning(&self, t: &S3Target) -> Result<Versioning, S3Error> {
        let query = [("versioning".to_owned(), String::new())];
        let response = self
            .send(t, Class::Read, Method::GET, None, &query, &[])
            .await?;
        let text = self.document(t, response).await?;
        match s3_xml::versioning(&text).map_err(|_| S3Error::Malformed)? {
            None => Ok(Versioning::None),
            Some(status) if status == "Enabled" => Ok(Versioning::Enabled),
            Some(status) if status == "Suspended" => Ok(Versioning::Suspended),
            Some(_) => Err(S3Error::Malformed),
        }
    }

    /// Counts `requests` of `class` and `bytes_down` received against `library`.
    pub fn count(&self, library: NodeId, class: Class, requests: u64, bytes_down: u64) {
        self.meter.add(library, class, requests, bytes_down);
    }

    /// The counts since the last call, per library, and starts again from zero.
    #[must_use]
    pub fn take_usage(&self) -> Vec<(NodeId, Usage)> {
        lock(&self.meter.usage).drain().collect()
    }

    /// The URL of a request without its query, and its `Host` header. Pure.
    #[must_use]
    pub fn request_url(t: &S3Target, key: Option<&str>) -> (String, String) {
        let (base, path, host) = parts(t, key);
        (format!("{base}{path}"), host)
    }

    /// Shorter connect and idle limits, for tests. Clients are made again with them.
    pub fn set_timeouts_for_tests(&self, connect: Duration, idle: Duration) {
        {
            let mut waits = lock(&self.waits);
            waits.connect = connect;
            waits.idle = idle;
        }
        lock(&self.clients).clear();
    }

    /// A shorter first wait before a retry (later waits double it), for tests.
    pub fn set_retry_wait_for_tests(&self, first: Duration) {
        lock(&self.waits).retry = first;
    }

    /// Resolves `host` to `to` instead of asking DNS, for tests. The address is still checked
    /// against the endpoint's allowed ranges.
    pub fn resolve_for_tests(&self, host: &str, to: SocketAddr) {
        lock(&self.overrides).insert(host.to_ascii_lowercase(), to);
    }

    fn waits(&self) -> Waits {
        *lock(&self.waits)
    }

    /// The kept client for `endpoint`, made on first use.
    fn client(&self, endpoint: &S3Endpoint) -> Result<reqwest::Client, S3Error> {
        let loopback_test =
            self.plain_loopback && endpoint.host() == "127.0.0.1" && endpoint.port().is_some();
        if !endpoint.https() && !loopback_test {
            return Err(S3Error::AddressRefused);
        }
        let ranges = endpoint
            .allow
            .iter()
            .map(IpRange::display)
            .collect::<Vec<_>>()
            .join(",");
        let key = format!("{}\n{}\n{ranges}", endpoint.name, endpoint.url);
        let mut clients = lock(&self.clients);
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let waits = self.waits();
        let resolver = CheckedResolver {
            allow: endpoint.allow.clone(),
            overrides: Arc::clone(&self.overrides),
        };
        let client = reqwest::Client::builder()
            .tls_backend_native()
            .https_only(endpoint.https())
            .redirect(redirect::Policy::none())
            .connect_timeout(waits.connect)
            .read_timeout(waits.idle)
            .user_agent(USER_AGENT)
            .no_proxy()
            .dns_resolver(Arc::new(resolver))
            .build()
            .map_err(|_| S3Error::Other)?;
        clients.insert(key, client.clone());
        Ok(client)
    }

    fn counted(&self, t: &S3Target, class: Class, requests: u64, bytes_down: u64) {
        if let Some(library) = t.library {
            self.meter.add(library, class, requests, bytes_down);
        }
    }

    fn body(&self, t: &S3Target, response: reqwest::Response, expected: Option<u64>) -> Body {
        Body {
            response,
            idle: self.waits().idle,
            expected,
            received: 0,
            meter: t.library.map(|library| (Arc::clone(&self.meter), library)),
            done: false,
        }
    }

    /// An XML document answer as text, at most 8 MiB.
    async fn document(&self, t: &S3Target, response: reqwest::Response) -> Result<String, S3Error> {
        let expected = content_length(response.headers());
        let bytes = self
            .body(t, response, expected)
            .read_capped(MAX_DOCUMENT)
            .await?;
        String::from_utf8(bytes).map_err(|_| S3Error::Malformed)
    }

    /// The error a failed answer means, from its status and its body's `Code`.
    async fn failure(&self, t: &S3Target, method: &Method, response: reqwest::Response) -> S3Error {
        let status = response.status().as_u16();
        let code = if *method == Method::HEAD {
            None
        } else {
            let expected = content_length(response.headers());
            let bytes = self
                .body(t, response, expected)
                .read_some(MAX_ERROR_BODY)
                .await;
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(s3_xml::error_code)
        };
        let code = code.as_deref().and_then(loggable);
        eprintln!(
            "sharecraft-server: S3 answered {status} {}",
            code.unwrap_or("(no code)")
        );
        mapped(status, code)
    }

    /// Signs and sends one request, trying a throttled or failed one again up to [`RETRIES`]
    /// times. Answers the first successful response.
    async fn send(
        &self,
        t: &S3Target,
        class: Class,
        method: Method,
        key: Option<&str>,
        query: &[(String, String)],
        extra: &[(String, String)],
    ) -> Result<reqwest::Response, S3Error> {
        if !valid_bucket(&t.bucket) || key.is_some_and(unaskable) {
            return Err(S3Error::Other);
        }
        let client = self.client(&t.endpoint)?;
        let (base, path, host) = parts(t, key);
        let query_text = query
            .iter()
            .map(|(name, value)| {
                format!(
                    "{}={}",
                    s3_sign::uri_encode(name, false),
                    s3_sign::uri_encode(value, false)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let full = if query_text.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{query_text}")
        };
        let url = reqwest::Url::parse(&full).map_err(|_| S3Error::Other)?;
        // The path sent must be the path signed.
        if url.path() != path {
            return Err(S3Error::Other);
        }
        // An address literal is never resolved, so it is checked here.
        let host_name = url.host_str().ok_or(S3Error::Other)?;
        let bare = host_name.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = bare.parse::<IpAddr>()
            && !address_allowed(ip, &t.endpoint.allow)
        {
            return Err(S3Error::AddressRefused);
        }

        let mut attempt: u32 = 0;
        loop {
            let headers = self.signed_headers(t, &method, &host, &path, query, extra)?;
            let sent = client
                .request(method.clone(), url.clone())
                .headers(headers)
                .send()
                .await;
            if let Err(e) = &sent
                && refused_by_resolver(e)
            {
                return Err(S3Error::AddressRefused);
            }
            self.counted(t, class, 1, 0);
            let response = sent.map_err(|e| transport(&e))?;
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            if !retried(status) {
                return Err(self.failure(t, &method, response).await);
            }
            let retry_after = header_text(response.headers(), "retry-after")
                .and_then(|text| text.trim().parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
            if method != Method::HEAD {
                let _ = self.body(t, response, None).read_some(MAX_ERROR_BODY).await;
            }
            if attempt >= RETRIES {
                eprintln!("sharecraft-server: S3 still answered {status} after retries");
                return Err(S3Error::Throttled);
            }
            let wait = retry_after.unwrap_or_else(|| {
                self.waits()
                    .retry
                    .saturating_mul(2_u32.saturating_pow(attempt))
            });
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// The headers of one try: exactly the ones signed, plus the `Authorization` header.
    fn signed_headers(
        &self,
        t: &S3Target,
        method: &Method,
        host: &str,
        path: &str,
        query: &[(String, String)],
        extra: &[(String, String)],
    ) -> Result<HeaderMap, S3Error> {
        let signed = s3_sign::sign(
            &t.credentials,
            &t.endpoint.region,
            OffsetDateTime::now_utc(),
            &ToSign {
                method: method.as_str(),
                host,
                path,
                query,
                headers: extra,
                payload_sha256: EMPTY_SHA256,
            },
        );
        let mut headers = HeaderMap::new();
        let mut put = |name: &str, value: &str| -> Result<(), S3Error> {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| S3Error::Other)?;
            let mut value = HeaderValue::from_str(value).map_err(|_| S3Error::Other)?;
            value.set_sensitive(name == reqwest::header::AUTHORIZATION);
            headers.insert(name, value);
            Ok(())
        };
        put("host", host)?;
        put("x-amz-date", &signed.amz_date)?;
        put("x-amz-content-sha256", EMPTY_SHA256)?;
        if let Some(token) = &t.credentials.session_token {
            put("x-amz-security-token", token.expose())?;
        }
        for (name, value) in extra {
            put(name, value)?;
        }
        put("authorization", &signed.authorization)?;
        Ok(headers)
    }
}

/// How many body chunks may wait between the answer and the content store.
const BODY_QUEUE: usize = 2;

/// Why an object's body was not stored.
#[derive(Debug)]
pub(crate) enum StoreFailure {
    /// The body was shorter or longer than its length: the object is changing, or the answer
    /// was cut.
    Torn,
    /// The body stopped coming (the idle limit passed) or the connection failed.
    Stream(S3Error),
    /// The content store refused it.
    Store(BlobError),
}

/// Reads from the body channel, and fails rather than ending before `remaining` bytes: a body
/// cut short is never placed in the store, also when the read is cancelled part way.
struct ExactReader {
    inner: ChannelReader,
    remaining: u64,
}

impl std::io::Read for ExactReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::io::Read::read(&mut self.inner, buf)?;
        if n == 0 && self.remaining > 0 && !buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the S3 body ended early",
            ));
        }
        self.remaining = self
            .remaining
            .saturating_sub(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }
}

/// Streams `body` into the content store through a bounded channel, at most `size` bytes and
/// exactly `size` bytes, and holds what it stored. The store reads on a blocking worker; the
/// body is read here. When the store stops early its end of the channel closes and the next
/// send fails, so neither side waits on the other for ever.
pub(crate) async fn store_body(
    blobs: &Arc<Blobs>,
    mut body: Body,
    size: u64,
) -> Result<Held, StoreFailure> {
    let (tx, rx) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(BODY_QUEUE);
    let reader = ExactReader {
        inner: ChannelReader::new(rx),
        remaining: size,
    };
    let write = blobs.write_reader(Box::new(reader), size);
    let pump = async move {
        loop {
            match body.chunk().await {
                Ok(Some(bytes)) => {
                    if tx.send(Ok(bytes)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(error) => {
                    let _ = tx
                        .send(Err(std::io::Error::other("the S3 body failed")))
                        .await;
                    return Err(error);
                }
            }
        }
    };
    let (written, pumped) = tokio::join!(write, pump);
    match (pumped, written) {
        (Ok(()), Ok(held)) => Ok(held),
        (Err(S3Error::Malformed), _) => Err(StoreFailure::Torn),
        (Err(error), _) => Err(StoreFailure::Stream(error)),
        (Ok(()), Err(BlobError::TooLarge { .. })) => Err(StoreFailure::Torn),
        (Ok(()), Err(BlobError::Io(error)))
            if error.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            Err(StoreFailure::Torn)
        }
        (Ok(()), Err(error)) => Err(StoreFailure::Store(error)),
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
    fn statuses_and_codes_map() {
        assert_eq!(mapped(404, None), S3Error::NotFound);
        assert_eq!(mapped(404, Some("NoSuchBucket")), S3Error::NoBucket);
        assert_eq!(mapped(403, Some("SignatureDoesNotMatch")), S3Error::BadKeys);
        assert_eq!(mapped(403, None), S3Error::Denied);
        assert_eq!(mapped(400, Some("InvalidObjectState")), S3Error::Archived);
        assert_eq!(mapped(501, None), S3Error::Unsupported);
        assert_eq!(mapped(418, None), S3Error::Other);
    }

    #[test]
    fn only_plain_words_are_logged() {
        assert_eq!(loggable("NoSuchKey"), Some("NoSuchKey"));
        assert_eq!(loggable("No Such\nKey"), None);
        assert_eq!(loggable(""), None);
    }

    #[test]
    fn dot_segments_are_never_asked_for() {
        assert!(unaskable("a/../b"));
        assert!(unaskable("./a"));
        assert!(unaskable(""));
        assert!(!unaskable("a/.b/c."));
    }
}
