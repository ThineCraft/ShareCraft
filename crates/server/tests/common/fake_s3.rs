//! A fake S3 for tests: an axum app on `127.0.0.1:0` holding buckets in memory. It speaks
//! path-style addressing and, when the `Host` header starts with a known bucket and a dot,
//! virtual-host style. It serves `ListObjectsV2` (with `encoding-type=url`), `HEAD`, `GET` with
//! ranges and `If-Match`, `?versioning`, and S3-shaped error XML, and has switches that force
//! S3's failures. Every request is logged (never the `Authorization` value).
//!
//! Writes: `PUT` stores bytes (checking `x-amz-content-sha256` against them, honouring
//! `If-Match` and `If-None-Match: *` with `412 PreconditionFailed`, keeping the
//! `x-amz-meta-sharecraft-sha256` header) and `DELETE` removes. Both count as `write` and are
//! logged by `writes()` with a condition word and never a header value. Any other write method
//! is answered `501 NotImplemented`.
//!
//! Copy and multipart: a `PUT` carrying `x-amz-copy-source` is a `CopyObject` (never stored as
//! an ordinary `PUT`): it honours `x-amz-copy-source-if-match` and the destination's
//! `If-None-Match` and `If-Match`, and keeps the source's content hash. `POST ?uploads`
//! creates an upload, `PUT ?partNumber=&uploadId=` stores a part (its hash checked like any
//! `PUT`), `POST ?uploadId=` completes (conditions honoured; every part but the last must be at
//! least [`FakeS3::min_part_bytes`], 5 MiB unless lowered) and `DELETE ?uploadId=` aborts.
//! `writes()` also logs `("COPY", to, condition)`, `("PART", key, number)`,
//! `("COMPLETE", key, condition)` and `("ABORT", key, "")`; creating an upload is not logged.
//!
//! It stops accepting connections when dropped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use server::s3_sign::{self, Credentials, ToSign};
use server::s3_xml;
use server::secrets::Secret;
use time::format_description::{self, OwnedFormatItem};
use time::{OffsetDateTime, PrimitiveDateTime};

/// How an object is stored, beyond its bytes.
#[derive(Clone, Debug, Default)]
pub struct Opts {
    /// The ETag, quoted, exactly as it is to be served; an MD5-like one when unset.
    pub etag: Option<String>,
    /// The storage class header; none (standard) when unset.
    pub class: Option<String>,
    /// `x-amz-restore`: `Some(true)` for a finished restore, `Some(false)` for one under way.
    pub restored: Option<bool>,
}

/// One request as the fake received it. Header names only: never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<String>,
}

#[derive(Clone)]
struct Object {
    bytes: Vec<u8>,
    etag: String,
    modified_s: i64,
    class: Option<String>,
    restored: Option<bool>,
    version: Option<String>,
    sha256: Option<String>,
}

/// An object as the fake holds it, for tests to inspect.
#[derive(Clone, Debug)]
pub struct Stored {
    pub bytes: Vec<u8>,
    pub etag: String,
    pub modified_s: i64,
    /// The `x-amz-meta-sharecraft-sha256` the object was written with.
    pub sha256: Option<String>,
}

/// A multipart upload that has been started and not completed or aborted.
struct Upload {
    key: String,
    sha256: Option<String>,
    parts: BTreeMap<u16, (Vec<u8>, String)>,
}

#[derive(Default)]
struct Bucket {
    objects: BTreeMap<String, Object>,
    versioning: Option<&'static str>,
    versions_made: u64,
    uploads: BTreeMap<String, Upload>,
    uploads_made: u64,
}

/// S3's smallest part but the last: 5 MiB.
const MIN_PART: u64 = 5 * 1024 * 1024;

struct Model {
    buckets: BTreeMap<String, Bucket>,
    keys: Option<(String, String)>,
    denied: Vec<(String, String)>,
    throttle_next: u32,
    fail_next: Option<(u16, String)>,
    slow: HashMap<String, Duration>,
    hung: HashSet<String>,
    truncated: HashMap<String, usize>,
    change_before_get: HashMap<String, Vec<Vec<u8>>>,
    appear_after_miss: HashMap<String, Vec<u8>>,
    page_size: usize,
    unsorted: bool,
    repeat_token: bool,
    truncated_without_token: bool,
    stray_key: Option<String>,
    skew: bool,
    redirect: Option<String>,
    no_versioning_api: bool,
    fail_continued: Option<(u16, String)>,
    slow_list: Option<Duration>,
    counts: HashMap<String, usize>,
    requests: Vec<Logged>,
    last_headers: HeaderMap,
    ignore_conditions: bool,
    read_only: bool,
    deny_writes_after: Option<usize>,
    lose_answer: bool,
    writes: Vec<(String, String, String)>,
    last_write_body_len: Option<usize>,
    error_in_200: bool,
    fail_parts: HashSet<u16>,
    min_part: u64,
    claimed_sizes: HashMap<String, u64>,
    fail_copies: HashMap<String, (u16, String)>,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            buckets: BTreeMap::new(),
            keys: None,
            denied: Vec::new(),
            throttle_next: 0,
            fail_next: None,
            slow: HashMap::new(),
            hung: HashSet::new(),
            truncated: HashMap::new(),
            change_before_get: HashMap::new(),
            appear_after_miss: HashMap::new(),
            page_size: 1000,
            unsorted: false,
            repeat_token: false,
            truncated_without_token: false,
            stray_key: None,
            skew: false,
            redirect: None,
            no_versioning_api: false,
            fail_continued: None,
            slow_list: None,
            counts: HashMap::new(),
            requests: Vec::new(),
            last_headers: HeaderMap::new(),
            ignore_conditions: false,
            read_only: false,
            deny_writes_after: None,
            lose_answer: false,
            writes: Vec::new(),
            last_write_body_len: None,
            error_in_200: false,
            fail_parts: HashSet::new(),
            min_part: MIN_PART,
            claimed_sizes: HashMap::new(),
            fail_copies: HashMap::new(),
        }
    }
}

/// A running fake S3. Dropping it stops it.
pub struct FakeS3 {
    pub addr: SocketAddr,
    model: Arc<Mutex<Model>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeS3 {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An MD5-like quoted ETag of `bytes`: 32 hex digits (the start of their SHA-256).
fn etag_of(bytes: &[u8]) -> String {
    let hex = s3_sign::sha256_hex(bytes);
    format!("\"{}\"", &hex[..32])
}

fn now_s() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn format(description: &str) -> OwnedFormatItem {
    format_description::parse_owned::<2>(description).unwrap()
}

fn http_date(s: i64) -> String {
    OffsetDateTime::from_unix_timestamp(s)
        .unwrap()
        .format(&format(
            "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT",
        ))
        .unwrap()
}

fn iso_date(s: i64) -> String {
    OffsetDateTime::from_unix_timestamp(s)
        .unwrap()
        .format(&format(
            "[year]-[month]-[day]T[hour]:[minute]:[second].000Z",
        ))
        .unwrap()
}

impl FakeS3 {
    /// Starts a fake with no buckets.
    pub async fn start() -> FakeS3 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let model = Arc::new(Mutex::new(Model::default()));
        let app = Router::new().fallback(handle).with_state(model.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeS3 { addr, model, task }
    }

    fn model(&self) -> std::sync::MutexGuard<'_, Model> {
        self.model.lock().unwrap()
    }

    /// `http://127.0.0.1:<port>`.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Makes the empty bucket `name`.
    pub fn bucket(&self, name: &str) {
        self.model().buckets.entry(name.to_owned()).or_default();
    }

    /// Stores `bytes` at `key`, modified now.
    pub fn put(&self, bucket: &str, key: &str, bytes: &[u8]) {
        self.put_full(bucket, key, bytes, now_s(), Opts::default());
    }

    /// Stores `bytes` at `key`, modified at `modified_s` (seconds since 1970).
    pub fn put_at(&self, bucket: &str, key: &str, bytes: &[u8], modified_s: i64) {
        self.put_full(bucket, key, bytes, modified_s, Opts::default());
    }

    /// Stores `bytes` at `key` with the ETag, class and restore state of `opts`.
    pub fn put_with(&self, bucket: &str, key: &str, bytes: &[u8], opts: Opts) {
        self.put_full(bucket, key, bytes, now_s(), opts);
    }

    /// Stores ytes at key with the ETag, class and restore state of opts, modified at
    /// modified_s.
    pub fn put_with_at(&self, bucket: &str, key: &str, bytes: &[u8], modified_s: i64, opts: Opts) {
        self.put_full(bucket, key, bytes, modified_s, opts);
    }

    /// Answers every listing request that carries a continuation token (every page after the
    /// first) with status and an error body carrying code.
    pub fn fail_continued_lists(&self, status: u16, code: &str) {
        self.model().fail_continued = Some((status, code.to_owned()));
    }

    /// Waits wait before answering each listing page.
    pub fn slow_list(&self, wait: Duration) {
        self.model().slow_list = Some(wait);
    }

    fn put_full(&self, bucket: &str, key: &str, bytes: &[u8], modified_s: i64, opts: Opts) {
        let mut model = self.model();
        let b = model.buckets.entry(bucket.to_owned()).or_default();
        store(b, key, bytes.to_vec(), modified_s, opts);
    }

    /// Removes `key`.
    pub fn remove(&self, bucket: &str, key: &str) {
        if let Some(b) = self.model().buckets.get_mut(bucket) {
            b.objects.remove(key);
        }
    }

    pub fn enable_versioning(&self, bucket: &str) {
        self.model()
            .buckets
            .entry(bucket.to_owned())
            .or_default()
            .versioning = Some("Enabled");
    }

    pub fn suspend_versioning(&self, bucket: &str) {
        self.model()
            .buckets
            .entry(bucket.to_owned())
            .or_default()
            .versioning = Some("Suspended");
    }

    /// Checks every request's signature against this key from now on.
    pub fn require_keys(&self, id: &str, secret: &str) {
        self.model().keys = Some((id.to_owned(), secret.to_owned()));
    }

    /// Answers `403 AccessDenied` for keys (and listing prefixes) starting with
    /// `key_or_prefix` in `bucket`.
    pub fn deny(&self, bucket: &str, key_or_prefix: &str) {
        self.model()
            .denied
            .push((bucket.to_owned(), key_or_prefix.to_owned()));
    }

    /// Answers the next `n` requests with `503 SlowDown` (0 clears it).
    pub fn throttle_next(&self, n: u32) {
        self.model().throttle_next = n;
    }

    /// Answers the next request with `status` and an error body carrying `code` (no body when
    /// `code` is empty).
    pub fn fail_next(&self, status: u16, code: &str) {
        self.model().fail_next = Some((status, code.to_owned()));
    }

    /// Waits `wait` before answering a `GET` of `key`.
    pub fn slow_get(&self, key: &str, wait: Duration) {
        self.model().slow.insert(key.to_owned(), wait);
    }

    /// A `GET` of `key` sends its headers and half its body, then nothing more.
    pub fn hang_get(&self, key: &str) {
        self.model().hung.insert(key.to_owned());
    }

    /// A `GET` of `key` promises the whole length but ends after `at` bytes.
    pub fn truncate_get(&self, key: &str, at: usize) {
        self.model().truncated.insert(key.to_owned(), at);
    }

    /// After the next `HEAD` of `key` the object is replaced by `bytes`. Called again before
    /// that `HEAD`, the replacements queue: each later `HEAD` makes the next one.
    pub fn change_before_get(&self, key: &str, bytes: &[u8]) {
        self.model()
            .change_before_get
            .entry(key.to_owned())
            .or_default()
            .push(bytes.to_vec());
    }

    /// Same as [`FakeS3::change_before_get`]: after the next `HEAD` of `key` the object is
    /// replaced by `bytes`.
    pub fn change_after_head(&self, key: &str, bytes: &[u8]) {
        self.change_before_get(key, bytes);
    }

    /// The next `HEAD` of `key` is answered as it is (not found, when nothing is there) and the
    /// key then appears with `bytes`: a new file someone else made between a check and a write.
    pub fn appear_after_miss(&self, key: &str, bytes: &[u8]) {
        self.model()
            .appear_after_miss
            .insert(key.to_owned(), bytes.to_vec());
    }

    /// The store accepts and ignores `If-Match` and `If-None-Match` on a write.
    pub fn ignore_conditions(&self) {
        self.model().ignore_conditions = true;
    }

    /// Every `PUT` and `DELETE` is answered `403 AccessDenied`.
    pub fn read_only(&self) {
        self.model().read_only = true;
    }

    /// The first `n` writes (`PUT` or `DELETE`) are accepted; every later one is answered
    /// `403 AccessDenied`.
    pub fn deny_writes_after(&self, n: usize) {
        self.model().deny_writes_after = Some(n);
    }

    /// Writes are accepted again: [`FakeS3::read_only`] and [`FakeS3::deny_writes_after`] end.
    pub fn allow_writes(&self) {
        let mut model = self.model();
        model.read_only = false;
        model.deny_writes_after = None;
    }

    /// The next write is stored (or removed), and then answered `503 SlowDown`.
    pub fn lose_answer_after_write(&self) {
        self.model().lose_answer = true;
    }

    /// Every `PUT` and `DELETE` received, in order: method, key and a condition word (`none`,
    /// `absent` for `If-None-Match` or `match` for `If-Match`). Never a header value.
    pub fn writes(&self) -> Vec<(String, String, String)> {
        self.model().writes.clone()
    }

    /// The next copy or completion is answered `200` with an `<Error>` body (`InternalError`)
    /// and changes nothing.
    pub fn error_in_200_next(&self) {
        self.model().error_in_200 = true;
    }

    /// Listings and `HEAD` say `key` holds `size` bytes, whatever it really holds: an object
    /// too large to store in a test.
    pub fn claim_size(&self, key: &str, size: u64) {
        self.model().claimed_sizes.insert(key.to_owned(), size);
    }

    /// The next copy to `key` is answered with `status` and an error body carrying `code`, and
    /// changes nothing. Other requests are answered as usual.
    pub fn fail_copy_to(&self, key: &str, status: u16, code: &str) {
        self.model()
            .fail_copies
            .insert(key.to_owned(), (status, code.to_owned()));
    }

    /// Every `PUT` of part number `n` is answered `500 InternalError`.
    pub fn fail_part(&self, n: u16) {
        self.model().fail_parts.insert(n);
    }

    /// Completion requires every part but the last to hold at least `bytes` (5 MiB at first).
    pub fn min_part_bytes(&self, bytes: u64) {
        self.model().min_part = bytes;
    }

    /// The multipart uploads started and not yet completed or aborted: key and upload id.
    pub fn open_multiparts(&self) -> Vec<(String, String)> {
        self.model()
            .buckets
            .values()
            .flat_map(|b| b.uploads.iter().map(|(id, u)| (u.key.clone(), id.clone())))
            .collect()
    }

    /// The length of the body of the last `PUT` received.
    pub fn last_write_body_len(&self) -> Option<usize> {
        self.model().last_write_body_len
    }

    /// The object at `key` in `bucket`.
    pub fn object(&self, bucket: &str, key: &str) -> Option<Stored> {
        let model = self.model();
        let object = model.buckets.get(bucket)?.objects.get(key)?;
        Some(Stored {
            bytes: object.bytes.clone(),
            etag: object.etag.clone(),
            modified_s: object.modified_s,
            sha256: object.sha256.clone(),
        })
    }

    /// Listing pages hold at most `n` keys.
    pub fn page_size(&self, n: usize) {
        self.model().page_size = n.max(1);
    }

    /// Listing pages come in reverse order.
    pub fn unsorted_pages(&self) {
        self.model().unsorted = true;
    }

    /// A listing asked with a continuation token answers that same token again.
    pub fn repeat_token(&self) {
        self.model().repeat_token = true;
    }

    /// A truncated listing page carries no continuation token.
    pub fn truncated_without_token(&self) {
        self.model().truncated_without_token = true;
    }

    /// Every listing page also names `key`, whatever prefix was asked.
    pub fn stray_key(&self, key: &str) {
        self.model().stray_key = Some(key.to_owned());
    }

    /// The fake's clock runs an hour ahead: every request is too skewed.
    pub fn skew_clock(&self) {
        self.model().skew = true;
    }

    /// Every request is answered with `307` to the same path at `origin`.
    pub fn redirect_to(&self, origin: &str) {
        self.model().redirect = Some(origin.to_owned());
    }

    /// `?versioning` is answered `501 NotImplemented`.
    pub fn no_versioning_api(&self) {
        self.model().no_versioning_api = true;
    }

    /// Requests received of a kind: `list`, `head`, `get`, `versioning` or `write`.
    pub fn count(&self, kind: &str) -> usize {
        self.model().counts.get(kind).copied().unwrap_or(0)
    }

    /// Every request received, in order.
    pub fn requests(&self) -> Vec<Logged> {
        self.model().requests.clone()
    }

    /// The headers of the last request, without `Authorization`.
    pub fn last_headers(&self) -> HeaderMap {
        self.model().last_headers.clone()
    }
}

fn store(b: &mut Bucket, key: &str, bytes: Vec<u8>, modified_s: i64, opts: Opts) {
    let version = match b.versioning {
        Some("Enabled") => {
            b.versions_made += 1;
            Some(format!("v{}", b.versions_made))
        }
        Some(_) => Some("null".to_owned()),
        None => None,
    };
    let etag = opts.etag.unwrap_or_else(|| etag_of(&bytes));
    b.objects.insert(
        key.to_owned(),
        Object {
            bytes,
            etag,
            modified_s,
            class: opts.class,
            restored: opts.restored,
            version,
            sha256: None,
        },
    );
}

fn error(status: u16, code: &str) -> Response {
    let status = StatusCode::from_u16(status).unwrap();
    if code.is_empty() {
        return Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap();
    }
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code>\
         <Message>The fake S3 says no.</Message><RequestId>FAKE</RequestId></Error>"
    );
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(body))
        .unwrap()
}

fn query_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            (
                s3_xml::percent_decode(name).unwrap_or_default(),
                s3_xml::percent_decode(value).unwrap_or_default(),
            )
        })
        .collect()
}

fn hex_decode(text: &str) -> Option<String> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    String::from_utf8(bytes).ok()
}

/// Checks the request's signature as S3 would, recomputing it from the request as received.
fn check_signature(
    method: &Method,
    path: &str,
    query: &str,
    headers: &HeaderMap,
    body: &[u8],
    keys: &(String, String),
) -> Result<(), &'static str> {
    const MISMATCH: &str = "SignatureDoesNotMatch";
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(MISMATCH)?;
    let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ").ok_or(MISMATCH)?;
    let mut credential = None;
    let mut signed = None;
    let mut signature = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("Credential=") {
            credential = Some(v);
        } else if let Some(v) = part.strip_prefix("SignedHeaders=") {
            signed = Some(v);
        } else if let Some(v) = part.strip_prefix("Signature=") {
            signature = Some(v);
        }
    }
    let (credential, signed, signature) = (
        credential.ok_or(MISMATCH)?,
        signed.ok_or(MISMATCH)?,
        signature.ok_or(MISMATCH)?,
    );
    let scope: Vec<&str> = credential.split('/').collect();
    if scope.len() != 5 {
        return Err(MISMATCH);
    }
    if scope[0] != keys.0 {
        return Err("InvalidAccessKeyId");
    }
    let region = scope[2];
    let names: Vec<&str> = signed.split(';').collect();
    for required in ["host", "x-amz-content-sha256", "x-amz-date"] {
        if !names.contains(&required) {
            return Err(MISMATCH);
        }
    }
    // Every signed header was sent, and every header that must be signed was.
    for name in &names {
        if !headers.contains_key(*name) {
            return Err(MISMATCH);
        }
    }
    for name in headers.keys() {
        let name = name.as_str();
        let must_sign = name.starts_with("x-amz-")
            || matches!(name, "range" | "if-match" | "if-none-match" | "content-md5");
        if must_sign && !names.contains(&name) {
            return Err(MISMATCH);
        }
    }
    let value = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or(MISMATCH)
    };
    let payload = value("x-amz-content-sha256")?;
    if payload != s3_sign::sha256_hex(body) {
        return Err(MISMATCH);
    }
    let amz_date = value("x-amz-date")?;
    let at = PrimitiveDateTime::parse(
        &amz_date,
        &format("[year][month][day]T[hour][minute][second]Z"),
    )
    .map_err(|_| MISMATCH)?
    .assume_utc();
    let host = value("host")?;
    let mut extra = Vec::new();
    for name in &names {
        if !matches!(
            *name,
            "host" | "x-amz-content-sha256" | "x-amz-date" | "x-amz-security-token"
        ) {
            extra.push(((*name).to_owned(), value(name)?));
        }
    }
    let query = query_pairs(query);
    let to_sign = ToSign {
        method: method.as_str(),
        host: &host,
        path,
        query: &query,
        headers: &extra,
        payload_sha256: &payload,
    };
    let creds = Credentials {
        access_key_id: keys.0.clone(),
        secret: Secret::new(keys.1.clone()),
        session_token: None,
    };
    let expected = s3_sign::sign(&creds, region, at, &to_sign);
    if expected.signature != signature || !expected.authorization.contains(signed) {
        return Err(MISMATCH);
    }
    Ok(())
}

/// The bucket and decoded key a request names; `None` when the path cannot be decoded.
fn address(model: &Model, host: &str, path: &str) -> Option<(String, String)> {
    let host = host.to_ascii_lowercase();
    let virtual_bucket = model
        .buckets
        .keys()
        .find(|b| host.starts_with(&format!("{b}.")))
        .cloned();
    let path = path.strip_prefix('/').unwrap_or(path);
    match virtual_bucket {
        Some(bucket) => Some((bucket, s3_xml::percent_decode(path).ok()?)),
        None => {
            let (bucket, key) = path.split_once('/').unwrap_or((path, ""));
            Some((bucket.to_owned(), s3_xml::percent_decode(key).ok()?))
        }
    }
}

async fn handle(State(model): State<Arc<Mutex<Model>>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 16 << 20)
        .await
        .unwrap_or_default();
    let method = parts.method.clone();
    let path = parts.uri.path().to_owned();
    let raw_query = parts.uri.query().unwrap_or("").to_owned();
    let query = query_pairs(&raw_query);
    let has = |name: &str| query.iter().any(|(n, _)| n == name);
    let param = |name: &str| {
        query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    };
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();

    let mut wait = None;
    let response = {
        let mut m = model.lock().unwrap();
        let mut names: Vec<String> = parts
            .headers
            .keys()
            .map(|k| k.as_str().to_owned())
            .collect();
        names.sort();
        m.requests.push(Logged {
            method: method.to_string(),
            path: path.clone(),
            query: raw_query.clone(),
            headers: names,
        });
        let mut shown = parts.headers.clone();
        shown.remove(header::AUTHORIZATION);
        m.last_headers = shown;

        let Some((bucket, key)) = address(&m, &host, &path) else {
            return error(400, "InvalidURI");
        };
        let kind = match method {
            Method::HEAD => "head",
            Method::GET if key.is_empty() && has("versioning") => "versioning",
            Method::GET if key.is_empty() => "list",
            Method::GET => "get",
            _ => "write",
        };
        *m.counts.entry(kind.to_owned()).or_default() += 1;
        let copying = method == Method::PUT && parts.headers.contains_key("x-amz-copy-source");
        let part_number = (method == Method::PUT && !copying)
            .then(|| param("partNumber"))
            .flatten();
        if kind == "write" && matches!(method, Method::PUT | Method::DELETE | Method::POST) {
            let condition = if parts.headers.contains_key(header::IF_NONE_MATCH) {
                "absent"
            } else if parts.headers.contains_key(header::IF_MATCH) {
                "match"
            } else {
                "none"
            };
            let entry = if copying {
                Some(("COPY".to_owned(), condition.to_owned()))
            } else if let Some(number) = &part_number {
                Some(("PART".to_owned(), number.clone()))
            } else if method == Method::POST && has("uploadId") {
                Some(("COMPLETE".to_owned(), condition.to_owned()))
            } else if method == Method::DELETE && has("uploadId") {
                Some(("ABORT".to_owned(), String::new()))
            } else if method == Method::POST {
                None
            } else {
                Some((method.to_string(), condition.to_owned()))
            };
            if let Some((word, detail)) = entry {
                m.writes.push((word, key.clone(), detail));
            }
            if method == Method::PUT && !copying {
                m.last_write_body_len = Some(body.len());
            }
        }

        if let Some(origin) = m.redirect.clone() {
            return Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header(header::LOCATION, format!("{origin}{path}"))
                .body(Body::empty())
                .unwrap();
        }
        if m.throttle_next > 0 {
            m.throttle_next -= 1;
            return error(503, "SlowDown");
        }
        if let Some((status, code)) = m.fail_next.take() {
            return error(status, &code);
        }
        if copying && let Some((status, code)) = m.fail_copies.remove(&key) {
            return error(status, &code);
        }
        if kind == "list"
            && has("continuation-token")
            && let Some((status, code)) = m.fail_continued.clone()
        {
            return error(status, &code);
        }
        if kind == "list" {
            wait = m.slow_list;
        }
        if kind == "write"
            && method == Method::PUT
            && let Some(claimed) = parts
                .headers
                .get("x-amz-content-sha256")
                .and_then(|v| v.to_str().ok())
            && claimed != s3_sign::sha256_hex(&body)
        {
            return error(400, "XAmzContentSHA256Mismatch");
        }
        if m.skew {
            return error(403, "RequestTimeTooSkewed");
        }
        if let Some(keys) = m.keys.clone()
            && let Err(code) =
                check_signature(&method, &path, &raw_query, &parts.headers, &body, &keys)
        {
            return error(403, code);
        }
        let multipart_post = method == Method::POST && (has("uploads") || has("uploadId"));
        if kind == "write" && !matches!(method, Method::PUT | Method::DELETE) && !multipart_post {
            return error(501, "NotImplemented");
        }
        let denied = m.denied.iter().any(|(b, p)| {
            *b == bucket
                && (key.starts_with(p.as_str())
                    || (kind == "list"
                        && param("prefix").unwrap_or_default().starts_with(p.as_str())))
        });
        let page_size = m.page_size;
        let unsorted = m.unsorted;
        let repeat_token = m.repeat_token;
        let without_token = m.truncated_without_token;
        let stray = m.stray_key.clone();
        let claimed = m.claimed_sizes.clone();
        let no_versioning = m.no_versioning_api;
        let replacement = if kind == "head" {
            m.change_before_get
                .get_mut(&key)
                .and_then(|queue| (!queue.is_empty()).then(|| queue.remove(0)))
        } else {
            None
        };
        let appears = if kind == "head" {
            m.appear_after_miss.remove(&key)
        } else {
            None
        };
        if kind == "get" {
            wait = m.slow.get(&key).copied();
        }
        let hung = m.hung.contains(&key);
        let truncated = m.truncated.get(&key).copied();
        let read_only = m.read_only || m.deny_writes_after.is_some_and(|n| m.writes.len() > n);
        let copy_source = if copying {
            Some(copy_source_of(&m, &parts.headers))
        } else {
            None
        };
        let wants_200_error = kind == "write"
            && (copying || (method == Method::POST && has("uploadId")))
            && std::mem::take(&mut m.error_in_200);
        let write_flags = Flags {
            ignore_conditions: m.ignore_conditions,
            lose_answer: kind == "write" && std::mem::take(&mut m.lose_answer),
            error_in_200: wants_200_error,
            fail_part: part_number
                .as_deref()
                .and_then(|n| n.parse::<u16>().ok())
                .is_some_and(|n| m.fail_parts.contains(&n)),
            min_part: m.min_part,
            copy_source,
        };
        let Some(b) = m.buckets.get_mut(&bucket) else {
            return error(404, "NoSuchBucket");
        };
        if denied {
            return error(403, "AccessDenied");
        }
        if kind == "write" {
            if read_only {
                return error(403, "AccessDenied");
            }
            return write(
                b,
                &key,
                &method,
                &parts.headers,
                &query,
                body.to_vec(),
                write_flags,
            );
        }
        match kind {
            "versioning" => {
                if no_versioning {
                    return error(501, "NotImplemented");
                }
                let status = b
                    .versioning
                    .map(|s| format!("<Status>{s}</Status>"))
                    .unwrap_or_default();
                xml(format!(
                    "<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                     {status}</VersioningConfiguration>"
                ))
            }
            "list" => list(
                b,
                &bucket,
                &param,
                page_size,
                unsorted,
                repeat_token,
                without_token,
                stray.as_deref(),
                &claimed,
            ),
            _ => {
                let Some(object) = b.objects.get(&key).cloned() else {
                    if let Some(bytes) = appears {
                        store(b, &key, bytes, now_s(), Opts::default());
                    }
                    return error(404, "NoSuchKey");
                };
                if let Some(bytes) = replacement {
                    let opts = Opts {
                        etag: None,
                        class: object.class.clone(),
                        restored: object.restored,
                    };
                    store(b, &key, bytes, now_s(), opts);
                }
                let claimed = (kind == "head")
                    .then(|| claimed.get(&key).copied())
                    .flatten();
                object_answer(
                    &object,
                    kind == "head",
                    &parts.headers,
                    hung,
                    truncated,
                    claimed,
                )
            }
        }
    };
    if let Some(wait) = wait {
        tokio::time::sleep(wait).await;
    }
    response
}

struct Flags {
    ignore_conditions: bool,
    lose_answer: bool,
    /// Answer the copy or completion `200` with an `<Error>` body.
    error_in_200: bool,
    /// The part being put is to fail.
    fail_part: bool,
    min_part: u64,
    /// For a copy: the source object, or the error that names why there is none.
    copy_source: Option<Result<Object, (u16, &'static str)>>,
}

/// The object a copy names in `x-amz-copy-source` (`/bucket/key`, the key percent-encoded).
fn copy_source_of(m: &Model, headers: &HeaderMap) -> Result<Object, (u16, &'static str)> {
    let value = headers
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
        .ok_or((400, "InvalidArgument"))?;
    let value = value.strip_prefix('/').unwrap_or(value);
    let (bucket, key) = value.split_once('/').ok_or((400, "InvalidArgument"))?;
    let key = s3_xml::percent_decode(key).map_err(|_| (400, "InvalidArgument"))?;
    m.buckets
        .get(bucket)
        .ok_or((404, "NoSuchBucket"))?
        .objects
        .get(&key)
        .cloned()
        .ok_or((404, "NoSuchKey"))
}

/// A `200` whose body is an `<Error>`, as S3 sends when a copy or completion fails after it
/// has begun to answer.
fn error_in_200() -> Response {
    xml(
        "<Error><Code>InternalError</Code><Message>We encountered an internal error. \
         Please try again.</Message><RequestId>FAKE</RequestId></Error>"
            .to_owned(),
    )
}

/// Whether the destination's conditions refuse the write.
fn refused_by_conditions(existing: Option<&Object>, headers: &HeaderMap) -> bool {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    (text("if-none-match") == Some("*") && existing.is_some())
        || text("if-match").is_some_and(|wanted| existing.is_none_or(|o| o.etag != wanted))
}

/// A `PUT`, `POST` or `DELETE` of `key`: an ordinary write, a copy or a multipart call.
fn write(
    b: &mut Bucket,
    key: &str,
    method: &Method,
    headers: &HeaderMap,
    query: &[(String, String)],
    bytes: Vec<u8>,
    mut flags: Flags,
) -> Response {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let param = |name: &str| {
        query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    if *method == Method::POST {
        return if param("uploads").is_some() {
            create_upload(b, key, headers)
        } else if let Some(id) = param("uploadId") {
            complete_upload(b, key, id, headers, &bytes, &flags)
        } else {
            error(501, "NotImplemented")
        };
    }
    if *method == Method::DELETE
        && let Some(id) = param("uploadId")
    {
        return if b.uploads.remove(id).is_some() {
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap()
        } else {
            error(404, "NoSuchUpload")
        };
    }
    if *method == Method::PUT && param("partNumber").is_some() {
        return put_part(
            b,
            key,
            param("partNumber"),
            param("uploadId"),
            bytes,
            &flags,
        );
    }
    if let Some(source) = flags.copy_source.take() {
        return copy_object(b, key, headers, source, &flags);
    }
    if *method == Method::DELETE {
        b.objects.remove(key);
        if flags.lose_answer {
            return error(503, "SlowDown");
        }
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap();
    }
    if !flags.ignore_conditions {
        let existing = b.objects.get(key);
        if text("if-none-match") == Some("*") && existing.is_some() {
            return error(412, "PreconditionFailed");
        }
        if let Some(wanted) = text("if-match")
            && existing.is_none_or(|object| object.etag != wanted)
        {
            return error(412, "PreconditionFailed");
        }
    }
    let sha256 = text("x-amz-meta-sharecraft-sha256").map(str::to_owned);
    store(b, key, bytes, now_s(), Opts::default());
    let object = b.objects.get_mut(key).unwrap();
    object.sha256 = sha256;
    if flags.lose_answer {
        return error(503, "SlowDown");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, object.etag.clone())
        .body(Body::empty())
        .unwrap()
}

fn xml(text: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/xml")
        .body(Body::from(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{text}"
        )))
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn list(
    b: &Bucket,
    bucket: &str,
    param: &dyn Fn(&str) -> Option<String>,
    page_size: usize,
    unsorted: bool,
    repeat_token: bool,
    without_token: bool,
    stray: Option<&str>,
    claimed: &HashMap<String, u64>,
) -> Response {
    if param("list-type").as_deref() != Some("2") {
        return error(400, "InvalidArgument");
    }
    let prefix = param("prefix").unwrap_or_default();
    let given = param("continuation-token");
    let after = match &given {
        Some(token) => match hex_decode(token) {
            Some(after) => Some(after),
            None => return error(400, "InvalidArgument"),
        },
        None => None,
    };
    let max_keys = param("max-keys")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1000)
        .min(1000);
    let take = page_size.min(max_keys).max(1);
    let matching: Vec<(&String, &Object)> = b
        .objects
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .filter(|(k, _)| after.as_ref().is_none_or(|a| k.as_str() > a.as_str()))
        .collect();
    let truncated = matching.len() > take;
    let mut page: Vec<(String, Object)> = matching
        .into_iter()
        .take(take)
        .map(|(k, o)| (k.clone(), o.clone()))
        .collect();
    let last = page.last().map(|(k, _)| k.clone());
    if let Some(stray) = stray {
        page.push((
            stray.to_owned(),
            Object {
                bytes: b"stray".to_vec(),
                etag: etag_of(b"stray"),
                modified_s: now_s(),
                class: None,
                restored: None,
                version: None,
                sha256: None,
            },
        ));
    }
    if unsorted {
        page.reverse();
    }
    let mut out = format!(
        "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{max_keys}</MaxKeys>\
         <EncodingType>url</EncodingType><IsTruncated>{truncated}</IsTruncated>",
        s3_xml::escape(bucket),
        url_listing_encode(&prefix),
        page.len(),
    );
    if let Some(token) = &given {
        out.push_str(&format!(
            "<ContinuationToken>{}</ContinuationToken>",
            s3_xml::escape(token)
        ));
    }
    for (key, object) in &page {
        out.push_str(&format!(
            "<Contents><Key>{}</Key><LastModified>{}</LastModified><ETag>{}</ETag>\
             <Size>{}</Size><StorageClass>{}</StorageClass></Contents>",
            url_listing_encode(key),
            iso_date(object.modified_s),
            s3_xml::escape(&object.etag),
            claimed
                .get(key)
                .copied()
                .unwrap_or(object.bytes.len() as u64),
            object.class.as_deref().unwrap_or("STANDARD"),
        ));
    }
    if truncated && !without_token {
        let token = match (&given, repeat_token) {
            (Some(given), true) => given.clone(),
            _ => s3_sign::hex(last.unwrap_or_default().as_bytes()),
        };
        out.push_str(&format!(
            "<NextContinuationToken>{token}</NextContinuationToken>"
        ));
    }
    out.push_str("</ListBucketResult>");
    xml(out)
}

/// Encodes a listed name like AWS S3's `encoding-type=url`: a space becomes `+` and a literal
/// `+` becomes `%2B`.
fn url_listing_encode(text: &str) -> String {
    s3_sign::uri_encode(text, true).replace("%20", "+")
}

fn object_answer(
    object: &Object,
    head_only: bool,
    headers: &HeaderMap,
    hung: bool,
    truncated: Option<usize>,
    claimed: Option<u64>,
) -> Response {
    let archived = matches!(object.class.as_deref(), Some("GLACIER" | "DEEP_ARCHIVE"))
        && object.restored != Some(true);
    if archived && !head_only {
        return error(403, "InvalidObjectState");
    }
    if let Some(wanted) = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok())
        && wanted != object.etag
    {
        return error(412, "PreconditionFailed");
    }
    let total = object.bytes.len();
    let mut status = StatusCode::OK;
    let mut range = None;
    if !head_only && let Some(asked) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        let parsed = asked.strip_prefix("bytes=").and_then(|r| {
            let (a, b) = r.split_once('-')?;
            let a = a.parse::<usize>().ok()?;
            let b = b.parse::<usize>().ok()?.min(total.saturating_sub(1));
            (a <= b && b < total).then_some((a, b))
        });
        match parsed {
            Some(r) => {
                status = StatusCode::PARTIAL_CONTENT;
                range = Some(r);
            }
            None => return error(416, "InvalidRange"),
        }
    }
    let bytes = match range {
        Some((a, b)) => object.bytes[a..=b].to_vec(),
        None => object.bytes.clone(),
    };
    let length = claimed.unwrap_or(bytes.len() as u64);
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ETAG, object.etag.clone())
        .header(header::LAST_MODIFIED, http_date(object.modified_s))
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some((a, b)) = range {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{total}"));
    }
    if let Some(class) = &object.class {
        builder = builder.header("x-amz-storage-class", class.clone());
    }
    match object.restored {
        Some(true) => {
            builder = builder.header(
                "x-amz-restore",
                "ongoing-request=\"false\", expiry-date=\"Fri, 21 Dec 2040 00:00:00 GMT\"",
            );
        }
        Some(false) => builder = builder.header("x-amz-restore", "ongoing-request=\"true\""),
        None => {}
    }
    if let Some(version) = &object.version {
        builder = builder.header("x-amz-version-id", version.clone());
    }
    if let Some(sha256) = &object.sha256 {
        builder = builder.header("x-amz-meta-sharecraft-sha256", sha256.clone());
    }
    if head_only {
        return builder.body(Body::empty()).unwrap();
    }
    if hung {
        let half = Bytes::from(bytes[..bytes.len() / 2].to_vec());
        let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(half) })
            .chain(futures_util::stream::pending());
        return builder.body(Body::from_stream(stream)).unwrap();
    }
    if let Some(at) = truncated {
        let part = Bytes::from(bytes[..at.min(bytes.len())].to_vec());
        // The part goes out with the headers; then the answer stops short.
        let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(part) })
            .chain(futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err(std::io::Error::other("cut short"))
            }));
        return builder.body(Body::from_stream(stream)).unwrap();
    }
    builder.body(Body::from(bytes)).unwrap()
}

/// `POST ?uploads`: starts an upload and answers its id.
fn create_upload(b: &mut Bucket, key: &str, headers: &HeaderMap) -> Response {
    b.uploads_made += 1;
    let id = format!("upload-{}", b.uploads_made);
    let sha256 = headers
        .get("x-amz-meta-sharecraft-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    b.uploads.insert(
        id.clone(),
        Upload {
            key: key.to_owned(),
            sha256,
            parts: BTreeMap::new(),
        },
    );
    xml(format!(
        "<InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Bucket>fake</Bucket><Key>{}</Key><UploadId>{}</UploadId>\
         </InitiateMultipartUploadResult>",
        s3_xml::escape(key),
        s3_xml::escape(&id),
    ))
}

/// `PUT ?partNumber=&uploadId=`: stores a part (its hash was checked with the request).
fn put_part(
    b: &mut Bucket,
    key: &str,
    number: Option<&str>,
    upload_id: Option<&str>,
    bytes: Vec<u8>,
    flags: &Flags,
) -> Response {
    let Some(number) = number
        .and_then(|n| n.parse::<u16>().ok())
        .filter(|n| (1..=10_000).contains(n))
    else {
        return error(400, "InvalidArgument");
    };
    let Some(upload) = upload_id
        .and_then(|id| b.uploads.get_mut(id))
        .filter(|u| u.key == key)
    else {
        return error(404, "NoSuchUpload");
    };
    if flags.fail_part {
        return error(500, "InternalError");
    }
    let etag = etag_of(&bytes);
    upload.parts.insert(number, (bytes, etag.clone()));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag)
        .body(Body::empty())
        .unwrap()
}

/// `POST ?uploadId=`: puts the parts together under the destination's conditions.
fn complete_upload(
    b: &mut Bucket,
    key: &str,
    id: &str,
    headers: &HeaderMap,
    body: &[u8],
    flags: &Flags,
) -> Response {
    if !b.uploads.get(id).is_some_and(|u| u.key == key) {
        return error(404, "NoSuchUpload");
    }
    if flags.error_in_200 {
        return error_in_200();
    }
    let Some(root) = std::str::from_utf8(body)
        .ok()
        .and_then(|text| s3_xml::parse(text).ok())
        .filter(|root| root.name == "CompleteMultipartUpload")
    else {
        return error(400, "MalformedXML");
    };
    let mut wanted: Vec<(u16, String)> = Vec::new();
    for part in root.children.iter().filter(|c| c.name == "Part") {
        let field = |name: &str| part.children.iter().find(|c| c.name == name);
        let number = field("PartNumber").and_then(|n| n.text.trim().parse::<u16>().ok());
        let (Some(number), Some(etag)) = (number, field("ETag")) else {
            return error(400, "MalformedXML");
        };
        wanted.push((number, etag.text.trim().to_owned()));
    }
    if wanted.is_empty() {
        return error(400, "MalformedXML");
    }
    if wanted.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return error(400, "InvalidPartOrder");
    }
    let Some(upload) = b.uploads.get(id) else {
        return error(404, "NoSuchUpload");
    };
    let mut joined = Vec::new();
    for (index, (number, etag)) in wanted.iter().enumerate() {
        let Some((bytes, stored)) = upload.parts.get(number) else {
            return error(400, "InvalidPart");
        };
        if stored != etag {
            return error(400, "InvalidPart");
        }
        let last = index + 1 == wanted.len();
        if !last && (bytes.len() as u64) < flags.min_part {
            return error(400, "EntityTooSmall");
        }
        joined.extend_from_slice(bytes);
    }
    if !flags.ignore_conditions && refused_by_conditions(b.objects.get(key), headers) {
        return error(412, "PreconditionFailed");
    }
    let sha256 = upload.sha256.clone();
    let etag = format!(
        "\"{}-{}\"",
        &s3_sign::sha256_hex(&joined)[..32],
        wanted.len()
    );
    b.uploads.remove(id);
    store(
        b,
        key,
        joined,
        now_s(),
        Opts {
            etag: Some(etag.clone()),
            ..Opts::default()
        },
    );
    if let Some(object) = b.objects.get_mut(key) {
        object.sha256 = sha256;
    }
    if flags.lose_answer {
        return error(503, "SlowDown");
    }
    xml(format!(
        "<CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Bucket>fake</Bucket><Key>{}</Key><ETag>{}</ETag></CompleteMultipartUploadResult>",
        s3_xml::escape(key),
        s3_xml::escape(&etag),
    ))
}

/// `CopyObject`: a `PUT` carrying `x-amz-copy-source`.
fn copy_object(
    b: &mut Bucket,
    key: &str,
    headers: &HeaderMap,
    source: Result<Object, (u16, &'static str)>,
    flags: &Flags,
) -> Response {
    let source = match source {
        Ok(source) => source,
        Err((status, code)) => return error(status, code),
    };
    if flags.error_in_200 {
        return error_in_200();
    }
    if let Some(wanted) = headers
        .get("x-amz-copy-source-if-match")
        .and_then(|v| v.to_str().ok())
        && wanted != source.etag
    {
        return error(412, "PreconditionFailed");
    }
    if !flags.ignore_conditions && refused_by_conditions(b.objects.get(key), headers) {
        return error(412, "PreconditionFailed");
    }
    store(
        b,
        key,
        source.bytes.clone(),
        now_s(),
        Opts {
            etag: Some(source.etag.clone()),
            class: source.class.clone(),
            restored: None,
        },
    );
    let modified = match b.objects.get_mut(key) {
        Some(object) => {
            object.sha256 = source.sha256.clone();
            object.modified_s
        }
        None => now_s(),
    };
    if flags.lose_answer {
        return error(503, "SlowDown");
    }
    xml(format!(
        "<CopyObjectResult><ETag>{}</ETag><LastModified>{}</LastModified></CopyObjectResult>",
        s3_xml::escape(&source.etag),
        iso_date(modified),
    ))
}
