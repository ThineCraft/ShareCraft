//! Every change to a bucket: the safe write, the copy written beside a changed file, the folder
//! marker, the write probe, and the moves inside a bucket (move, copy, hold in the holding
//! folder, unhold, delete held, sweep). This is the only module that calls `S3::put`,
//! `S3::copy` or `S3::delete`.
//!
//! A write never overwrites what it has not seen. It starts with a `HEAD`; on a store that
//! honours conditions the `PUT` carries `If-Match` or `If-None-Match`, so a change made between
//! the `HEAD` and the `PUT` is refused by the store and becomes a conflict. Every `PUT` records
//! the content's SHA-256 in object metadata, so a write whose answer was lost is recognised as
//! ours by the next attempt's `HEAD` rather than taken for a conflict. Nothing is retried here:
//! the caller's back-off retries, and every retry starts with a `HEAD`.

use std::collections::{BTreeMap, HashMap, HashSet};

use collab_core::ids::NodeId;
use records::{Tri, object_tag, reserved_name};
use sha2::{Digest, Sha256};
use time::{Date, Month, OffsetDateTime};
use uuid::Uuid;

use crate::s3::{COPY_MAX, Cond, ObjectHead, PutBody, S3, S3Error, S3Target};
use crate::s3_sign;
use crate::s3_source::{MAX_RELATIVE_CHARS, nameable_segment};
use crate::s3_xml::ObjectEntry;
use crate::source_write::{Base, Probe, Seen, WriteError, Written, numbered_name, sibling_name};

/// The longest key S3 accepts, in bytes.
const MAX_KEY_BYTES: usize = 1024;
/// How many names a copy tries: the plain one, then `(2)` to `(20)`.
const LAST_NUMBER: u32 = 20;

/// The key for the library-relative `path`: the library's prefix and the path.
///
/// `None` for an empty path, a leading `/`, an empty, `.` or `..` segment, a control character,
/// a segment the listing would not show, a name ShareCraft keeps for itself, a path longer than
/// the listing shows, or a key over 1,024 bytes. Every write builds its key through this.
#[must_use]
pub fn object_key(t: &S3Target, path: &str) -> Option<String> {
    if path.is_empty() || path.chars().count() > MAX_RELATIVE_CHARS {
        return None;
    }
    if path
        .split('/')
        .any(|segment| !nameable_segment(segment) || reserved_name(segment))
    {
        return None;
    }
    let key = format!("{}{path}", t.prefix());
    (key.len() <= MAX_KEY_BYTES).then_some(key)
}

/// How a client error reads to the caller.
fn write_error(error: S3Error) -> WriteError {
    match error {
        S3Error::Denied | S3Error::BadKeys => WriteError::Denied,
        S3Error::NoBucket => WriteError::Gone,
        S3Error::Throttled
        | S3Error::Unreachable
        | S3Error::TimedOut
        | S3Error::AddressRefused
        | S3Error::ClockSkew
        | S3Error::WrongRegion => WriteError::Offline,
        S3Error::Unsupported => WriteError::TooLarge,
        S3Error::NotFound
        | S3Error::Precondition
        | S3Error::Archived
        | S3Error::Malformed
        | S3Error::Other => WriteError::Failed,
    }
}

/// The word `connect_s3` uses for why a bucket could not be asked.
fn reason(error: S3Error) -> &'static str {
    match error {
        S3Error::BadKeys => "keys",
        S3Error::NoBucket => "bucket",
        S3Error::WrongRegion => "region",
        S3Error::ClockSkew => "clock",
        S3Error::AddressRefused => "address",
        _ => "offline",
    }
}

/// The tag of an object, as the listing computes it.
fn tag_of(head: &ObjectHead) -> String {
    object_tag(head.size, head.modified_s, &head.etag, false)
}

/// What the store holds, as the next listing will see it.
fn seen_of(head: &ObjectHead) -> Seen {
    Seen {
        tag: tag_of(head),
        size: head.size,
        modified: OffsetDateTime::from_unix_timestamp(head.modified_s)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH),
    }
}

/// Whether the object was written by us with this content.
fn is_ours(head: &ObjectHead, body: &PutBody) -> bool {
    head.sha256.as_deref() == Some(s3_sign::hex(&body.sha256).as_str())
}

/// Refuses a body one `PUT` cannot send, before any request.
fn check_size(s3: &S3, body: &PutBody) -> Result<(), WriteError> {
    if body.size > s3.single_put_max() {
        Err(WriteError::TooLarge)
    } else {
        Ok(())
    }
}

/// Puts `body` at `path`, replacing what the caller saw (`base.tag`) or creating a file that was
/// not there (`base.tag` of `None`), and never overwriting anything else.
///
/// With `conditional` of `Tri::Yes` the `PUT` is conditional, so a change after the `HEAD` is
/// refused by the store. With `Tri::No` or `Tri::Unknown` (treated alike) the `PUT` is
/// unconditional after the same `HEAD` check: a small stated race, which the caller reports.
///
/// # Errors
/// [`WriteError`] when the store refused, could not be reached, or the body is too large. A
/// `PUT` that succeeded but whose following `HEAD` failed is an error too: the retry's first
/// `HEAD` finds our hash and finishes without a second `PUT`.
pub async fn write_object(
    s3: &S3,
    t: &S3Target,
    path: &str,
    body: &PutBody,
    base: &Base,
    conditional: Tri,
) -> Result<Written, WriteError> {
    let key = object_key(t, path).ok_or(WriteError::Failed)?;
    check_size(s3, body)?;
    let conditions = conditional == Tri::Yes;
    let head = match s3.head(t, &key).await {
        Ok(head) => Some(head),
        Err(S3Error::NotFound) => None,
        Err(error) => return Err(write_error(error)),
    };
    let put = match head {
        None => match base.tag {
            None => put_checked(s3, t, &key, body, cond_if_absent(conditions)).await,
            Some(_) => return Ok(Written::Conflict),
        },
        Some(head) => {
            if base.tag.as_deref() == Some(tag_of(&head).as_str()) {
                if conditions {
                    put_checked(s3, t, &key, body, Cond::IfMatch(&head.etag)).await
                } else {
                    put_checked(s3, t, &key, body, Cond::None).await
                }
            } else if is_ours(&head, body) {
                return Ok(Written::Done(seen_of(&head)));
            } else {
                return Ok(Written::Conflict);
            }
        }
    };
    match put {
        Ok(head) => Ok(Written::Done(seen_of(&head))),
        Err(S3Error::Precondition) => {
            // Refused: the object is not what we saw. Ours only if our own hash is there.
            match s3.head(t, &key).await {
                Ok(head) if is_ours(&head, body) => Ok(Written::Done(seen_of(&head))),
                Ok(_) | Err(S3Error::NotFound) => Ok(Written::Conflict),
                Err(error) => Err(write_error(error)),
            }
        }
        Err(error) => Err(write_error(error)),
    }
}

/// The condition for a file that must not exist: the store's, or none on a store without them.
fn cond_if_absent(conditions: bool) -> Cond<'static> {
    if conditions {
        Cond::IfAbsent
    } else {
        Cond::None
    }
}

/// The one place `S3::put` is called for content.
async fn put_checked(
    s3: &S3,
    t: &S3Target,
    key: &str,
    body: &PutBody,
    cond: Cond<'_>,
) -> Result<ObjectHead, S3Error> {
    s3.put(t, key, body, cond).await
}

/// `path` split into its folder (with the closing `/`, or empty) and its name.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(at) => (
            path.get(..=at).unwrap_or(""),
            path.get(at + 1..).unwrap_or(path),
        ),
        None => ("", path),
    }
}

/// Puts `body` at the first of `names` (in the folder of `path`) that is free, each `If-None-
/// Match`. A taken name whose object carries our hash is our own earlier attempt and is done.
async fn put_first_free(
    s3: &S3,
    t: &S3Target,
    folder: &str,
    names: &[String],
    body: &PutBody,
    conditional: Tri,
) -> Result<(String, Seen), WriteError> {
    check_size(s3, body)?;
    let conditions = conditional == Tri::Yes;
    for name in names {
        let path = format!("{folder}{name}");
        let key = object_key(t, &path).ok_or(WriteError::Failed)?;
        if !conditions {
            // No conditions to lean on: a name that is there is never written to.
            match s3.head(t, &key).await {
                Ok(head) if is_ours(&head, body) => return Ok((path, seen_of(&head))),
                Ok(_) => continue,
                Err(S3Error::NotFound) => {}
                Err(error) => return Err(write_error(error)),
            }
        }
        match put_checked(s3, t, &key, body, cond_if_absent(conditions)).await {
            Ok(head) => return Ok((path, seen_of(&head))),
            Err(S3Error::Precondition) => match s3.head(t, &key).await {
                Ok(head) if is_ours(&head, body) => return Ok((path, seen_of(&head))),
                Ok(_) | Err(S3Error::NotFound) => {}
                Err(error) => return Err(write_error(error)),
            },
            Err(error) => return Err(write_error(error)),
        }
    }
    Err(WriteError::Failed)
}

/// The names to try for `first`: itself, then `(2)` to `(20)`.
fn candidates(first: &str) -> Vec<String> {
    let mut names = vec![first.to_owned()];
    names.extend((2..=LAST_NUMBER).map(|n| numbered_name(first, n)));
    names
}

/// Writes the edit as a new file beside `path`, named for the person and the time (see
/// [`sibling_name`]), numbered if that name is taken. Returns the new path and what was stored.
///
/// # Errors
/// [`WriteError`]; `Failed` when all twenty names are taken.
pub async fn write_beside(
    s3: &S3,
    t: &S3Target,
    path: &str,
    body: &PutBody,
    person: &str,
    at: OffsetDateTime,
    conditional: Tri,
) -> Result<(String, Seen), WriteError> {
    object_key(t, path).ok_or(WriteError::Failed)?;
    let (folder, name) = split_path(path);
    let first = sibling_name(name, person, at);
    put_first_free(s3, t, folder, &candidates(&first), body, conditional).await
}

/// Writes a new file at `path`, or at `name (2).ext` and on if that key appeared meanwhile.
///
/// # Errors
/// [`WriteError`]; `Failed` when all twenty names are taken.
pub async fn write_numbered(
    s3: &S3,
    t: &S3Target,
    path: &str,
    body: &PutBody,
    conditional: Tri,
) -> Result<(String, Seen), WriteError> {
    object_key(t, path).ok_or(WriteError::Failed)?;
    let (folder, name) = split_path(path);
    let mut names = vec![];
    names.extend((2..=LAST_NUMBER).map(|n| numbered_name(name, n)));
    put_first_free(s3, t, folder, &names, body, conditional).await
}

/// The body of a zero-byte object.
fn empty_body() -> PutBody {
    PutBody {
        path: None,
        size: 0,
        sha256: Sha256::digest(b"").into(),
    }
}

/// Makes the folder at `path`: a zero-byte object at `<key>/`. A marker that is already there is
/// success. Nothing is deleted or overwritten.
///
/// With `conditional` of `Tri::Yes` the `PUT` is `If-None-Match`. With `Tri::No` or
/// `Tri::Unknown` (treated alike) the marker key is asked for first (`HEAD`): one that is there
/// is success with no `PUT`, else the `PUT` is unconditional: the same small stated race as
/// [`write_object`].
///
/// # Errors
/// [`WriteError`]; `Failed` for a path that is not a good folder name.
pub async fn make_folder(
    s3: &S3,
    t: &S3Target,
    path: &str,
    conditional: Tri,
) -> Result<(), WriteError> {
    let key = object_key(t, path).ok_or(WriteError::Failed)?;
    let key = format!("{key}/");
    if key.len() > MAX_KEY_BYTES {
        return Err(WriteError::Failed);
    }
    let conditions = conditional == Tri::Yes;
    if !conditions {
        match s3.head(t, &key).await {
            Ok(_) => return Ok(()),
            Err(S3Error::NotFound) => {}
            Err(error) => return Err(write_error(error)),
        }
    }
    match put_checked(s3, t, &key, &empty_body(), cond_if_absent(conditions)).await {
        Ok(_) | Err(S3Error::Precondition) => Ok(()),
        Err(error) => Err(write_error(error)),
    }
}

/// Whether a refusal answers a condition: `Ok(true)` refused, `Ok(false)` accepted or not
/// understood (a store that rejects the header does not honour it), `Err(None)` when the key may
/// not write at all, else why it failed.
fn refused(result: Result<ObjectHead, S3Error>) -> Result<bool, Option<&'static str>> {
    match result {
        Ok(_) => Ok(false),
        Err(S3Error::Precondition | S3Error::NotFound) => Ok(true),
        Err(S3Error::Unsupported | S3Error::Other | S3Error::Malformed | S3Error::Archived) => {
            Ok(false)
        }
        Err(S3Error::Denied) => Err(None),
        Err(error) => Err(Some(reason(error))),
    }
}

/// Finds out whether the key may write to the library's prefix, and whether the store honours
/// conditional writes, with four requests on one key of ours: a `PUT` (`If-None-Match`), the same
/// again (refused when conditions work), a `PUT` with an `If-Match` that cannot match (refused
/// when conditions work) and a `DELETE`. Nothing that was there is touched.
///
/// The key is `<prefix>.sharecraft-tmp-probe-<uuid>`, which listings hide. The `DELETE` is sent
/// after any `PUT` that did not answer `Denied`, even one that failed, since it may have landed.
/// The second value is the probe object's name relative to the library prefix when its removal
/// failed, else `None`; the answer is still what the probe found.
pub async fn probe_writes(s3: &S3, t: &S3Target) -> (Probe, Option<String>) {
    // Not built by `object_key`, which refuses names ShareCraft keeps for itself.
    let name = format!(".sharecraft-tmp-probe-{}", Uuid::new_v4());
    let key = format!("{}{name}", t.prefix());
    let body = empty_body();
    let first = put_checked(s3, t, &key, &body, Cond::IfAbsent).await;
    let finding = match first {
        Err(S3Error::Denied) => return (Probe::ReadOnly, None),
        Err(error) => Err(Some(reason(error))),
        Ok(_) => {
            let second = refused(put_checked(s3, t, &key, &body, Cond::IfAbsent).await);
            match second {
                Ok(second) => {
                    let third =
                        refused(put_checked(s3, t, &key, &body, Cond::IfMatch("\"0\"")).await);
                    third.map(|third| second && third)
                }
                Err(why) => Err(why),
            }
        }
    };
    // The key is ours whatever the answers were.
    let left = match s3.delete(t, &key).await {
        Ok(()) => None,
        Err(_) => Some(name),
    };
    let probe = match finding {
        Ok(true) => Probe::ReadWrite {
            conditional: Tri::Yes,
        },
        Ok(false) => Probe::ReadWrite {
            conditional: Tri::No,
        },
        Err(None) => Probe::ReadOnly,
        Err(Some(why)) => Probe::Unreachable(why),
    };
    (probe, left)
}

// ---- Bucket moves: move, copy, hold, unhold, delete held, sweep (slice 2) ----

/// The most objects one move, copy, hold or unhold touches. Folder markers count.
pub const MOVE_MAX_OBJECTS: u64 = 1_000;
/// The holding folder's name, at the top of the library's prefix.
const HOLDING: &str = ".sharecraft-bin";
/// Keys asked for per listing page.
const PAGE_KEYS: u16 = 1000;
/// Listing pages one step reads at most: a guard on a store that never ends a listing.
const MAX_PAGES: u32 = 10_000;

/// What a move, copy, hold or unhold did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    /// How many objects were copied.
    pub objects: u64,
    /// How many originals could not be deleted after every copy was made.
    pub left_behind: u64,
    /// Each destination's library-relative path and what the store holds there.
    pub seen: Vec<(String, Seen)>,
}

/// One object of a step: where it is, where it goes, and the tag and size it is copied on.
struct Step {
    from: String,
    to: String,
    etag: String,
    size: u64,
}

/// The key of a library-relative path: a file's through [`object_key`]; a folder's (`path`
/// ending `/`, or given without it) is the key of its name followed by `/`.
fn item_key(t: &S3Target, path: &str, folder: bool) -> Option<String> {
    if !folder {
        return object_key(t, path);
    }
    let name = path.strip_suffix('/').unwrap_or(path);
    let key = format!("{}/", object_key(t, name)?);
    (key.len() <= MAX_KEY_BYTES).then_some(key)
}

/// A `YYYY-MM-DD` date, strictly: four, two and two ASCII digits.
fn parse_day(text: &str) -> Option<Date> {
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if parts.next().is_some() || !digits(year, 4) || !digits(month, 2) || !digits(day, 2) {
        return None;
    }
    let month = Month::try_from(month.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(year.parse().ok()?, month, day.parse().ok()?).ok()
}

/// A bin entry id, in the hyphenated lower-case form [`holding_root`] writes.
fn parse_entry(text: &str) -> Option<NodeId> {
    let id = Uuid::try_parse(text).ok()?;
    (id.hyphenated().to_string() == text).then_some(NodeId(id))
}

/// Whether `root` is a holding root: `.sharecraft-bin/<YYYY-MM-DD>/<entry>/`.
fn is_root(root: &str) -> bool {
    let Some(rest) = root
        .strip_prefix(HOLDING)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.strip_suffix('/'))
    else {
        return false;
    };
    match rest.split_once('/') {
        Some((date, entry)) => parse_day(date).is_some() && parse_entry(entry).is_some(),
        None => false,
    }
}

/// The key of `path` held under `root`. `None` when the root is not a holding root, the path
/// is not a good library path, or the key is over 1,024 bytes. Built by hand, since
/// [`object_key`] refuses the holding folder's name.
fn holding_key(t: &S3Target, root: &str, path: &str, folder: bool) -> Option<String> {
    if !is_root(root) {
        return None;
    }
    let key = item_key(t, path, folder)?;
    let inside = key.strip_prefix(t.prefix())?;
    let key = format!("{}{root}{inside}", t.prefix());
    (key.len() <= MAX_KEY_BYTES).then_some(key)
}

/// The holding root for a bin entry deleted on `date` (UTC):
/// `.sharecraft-bin/<YYYY-MM-DD>/<entry>/`, library-relative.
#[must_use]
pub fn holding_root(date: Date, entry: NodeId) -> String {
    format!(
        "{HOLDING}/{:04}-{:02}-{:02}/{}/",
        date.year(),
        u8::from(date.month()),
        date.day(),
        entry.0.hyphenated()
    )
}

/// Whether the library-relative `path` is inside the holding folder (where a tracking row of an
/// item deleted into it is kept while its bin entry lasts).
#[must_use]
pub fn is_held_path(path: &str) -> bool {
    path.strip_prefix(HOLDING)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Aborts the multipart uploads a crash left open for more than a day before `now`. Answers how
/// many it aborted. Nothing yet: multipart uploads come with a later step of this work.
pub async fn abort_stale(_state: &crate::AppState, _now: OffsetDateTime) -> usize {
    0
}

/// Every object whose key starts with `prefix`, all pages. Keys a store names outside the
/// prefix are dropped, so nothing outside it is ever moved or deleted.
async fn list_all(s3: &S3, t: &S3Target, prefix: &str) -> Result<Vec<ObjectEntry>, WriteError> {
    let mut objects: BTreeMap<String, ObjectEntry> = BTreeMap::new();
    let mut tokens: HashSet<String> = HashSet::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = s3
            .list_page(t, prefix, token.as_deref(), PAGE_KEYS)
            .await
            .map_err(write_error)?;
        for object in page.objects {
            if object.key.starts_with(prefix) {
                objects.insert(object.key.clone(), object);
            }
        }
        match page.next {
            None => return Ok(objects.into_values().collect()),
            Some(next) => {
                if !tokens.insert(next.clone()) {
                    return Err(WriteError::Failed);
                }
                token = Some(next);
            }
        }
    }
    Err(WriteError::Failed)
}

/// Whether any object's key starts with `prefix`.
async fn any_under(s3: &S3, t: &S3Target, prefix: &str) -> Result<bool, WriteError> {
    let page = s3
        .list_page(t, prefix, None, 1)
        .await
        .map_err(write_error)?;
    Ok(page.objects.iter().any(|o| o.key.starts_with(prefix)))
}

/// Whether `key` is there.
async fn exists(s3: &S3, t: &S3Target, key: &str) -> Result<bool, WriteError> {
    match s3.head(t, key).await {
        Ok(_) => Ok(true),
        Err(S3Error::NotFound) => Ok(false),
        Err(error) => Err(write_error(error)),
    }
}

/// The library-relative path of a key in the library.
fn relative<'a>(t: &S3Target, key: &'a str) -> Option<&'a str> {
    key.strip_prefix(t.prefix())
}

/// Copies every object of the item at `from_key` to `to_key` and, with `delete`, then deletes
/// the originals. `known` (when given) is what the tree recorded, by library-relative path;
/// `None` skips the tag checks, for the holding folder that nothing outside writes.
///
/// Everything that can refuse the step is asked before the first write: the count, the sizes,
/// the recorded tags and the destination names. No original is deleted before every copy has
/// succeeded.
async fn relocate(
    s3: &S3,
    t: &S3Target,
    from_key: &str,
    to_key: &str,
    folder: bool,
    known: Option<&[(String, String)]>,
    delete: bool,
) -> Result<Moved, WriteError> {
    if from_key == to_key || (folder && to_key.starts_with(from_key)) {
        return Err(WriteError::Failed);
    }
    let mut steps: Vec<Step> = Vec::new();
    if folder {
        let objects = list_all(s3, t, from_key).await?;
        let count = u64::try_from(objects.len()).unwrap_or(u64::MAX);
        if count > MOVE_MAX_OBJECTS {
            return Err(WriteError::TooMany(count));
        }
        if objects.iter().any(|o| o.size > COPY_MAX) {
            return Err(WriteError::TooLargeToMove);
        }
        for object in objects {
            let rest = object
                .key
                .strip_prefix(from_key)
                .ok_or(WriteError::Failed)?;
            let to = format!("{to_key}{rest}");
            if to.len() > MAX_KEY_BYTES {
                return Err(WriteError::Failed);
            }
            steps.push(Step {
                from: object.key,
                to,
                etag: object.etag,
                size: object.size,
            });
        }
        if let Some(known) = known {
            check_folder(s3, t, from_key, &mut steps, known).await?;
        }
    } else {
        let head = match s3.head(t, from_key).await {
            Ok(head) => head,
            Err(S3Error::NotFound) => return Err(WriteError::Changed),
            Err(error) => return Err(write_error(error)),
        };
        if head.size > COPY_MAX {
            return Err(WriteError::TooLargeToMove);
        }
        if let Some(known) = known {
            let path = relative(t, from_key).ok_or(WriteError::Failed)?;
            // A file the tree has no tag for was never seen as it is now: not moved blind.
            match known.iter().find(|(p, _)| p == path) {
                Some((_, tag)) if *tag == tag_of(&head) => {}
                _ => return Err(WriteError::Changed),
            }
        }
        steps.push(Step {
            from: from_key.to_owned(),
            to: to_key.to_owned(),
            etag: head.etag,
            size: head.size,
        });
    }
    // Destination names. For a folder, anything under its prefix: an existing folder, marked
    // or not, is never merged into.
    let taken = if folder {
        any_under(s3, t, to_key).await?
    } else {
        exists(s3, t, to_key).await?
    };
    if taken {
        return Err(WriteError::NameTaken);
    }
    let seen = copy_all(s3, t, &steps).await?;
    let mut left_behind = 0;
    if delete {
        for step in &steps {
            if s3.delete(t, &step.from).await.is_err() {
                left_behind += 1;
            }
        }
    }
    Ok(Moved {
        objects: u64::try_from(steps.len()).unwrap_or(u64::MAX),
        left_behind,
        seen,
    })
}

/// Checks a folder's objects against what the tree recorded: a key the tree does not know, a
/// known file the listing does not find, or a known file whose `HEAD` has another tag is a
/// change. Each checked file's step takes the tag and size its `HEAD` answered.
async fn check_folder(
    s3: &S3,
    t: &S3Target,
    from_key: &str,
    steps: &mut [Step],
    known: &[(String, String)],
) -> Result<(), WriteError> {
    let folder = relative(t, from_key).ok_or(WriteError::Failed)?;
    let recorded: HashMap<&str, &str> = known
        .iter()
        .filter(|(path, _)| path.starts_with(folder))
        .map(|(path, tag)| (path.as_str(), tag.as_str()))
        .collect();
    let listed: HashSet<&str> = steps
        .iter()
        .filter_map(|step| relative(t, &step.from))
        .collect();
    // A known file the listing no longer finds was deleted or renamed outside. A known folder
    // may have no marker.
    if recorded
        .keys()
        .any(|path| !path.ends_with('/') && !listed.contains(path))
    {
        return Err(WriteError::Changed);
    }
    for step in steps.iter_mut() {
        let path = relative(t, &step.from).ok_or(WriteError::Failed)?;
        let Some(tag) = recorded.get(path) else {
            return Err(WriteError::Changed);
        };
        if path.ends_with('/') {
            // A folder marker holds nothing to change.
            continue;
        }
        let head = match s3.head(t, &step.from).await {
            Ok(head) => head,
            Err(S3Error::NotFound) => return Err(WriteError::Changed),
            Err(error) => return Err(write_error(error)),
        };
        if *tag != tag_of(&head) {
            return Err(WriteError::Changed);
        }
        if head.size > COPY_MAX {
            return Err(WriteError::TooLargeToMove);
        }
        step.etag = head.etag;
        step.size = head.size;
    }
    Ok(())
}

/// Copies every step, each on its source's tag and only to a free key. On the first failure
/// the copies already made, and only those, are deleted, and the failure is answered: a
/// refused copy is `NameTaken` when its destination is now there, else `Changed`.
async fn copy_all(
    s3: &S3,
    t: &S3Target,
    steps: &[Step],
) -> Result<Vec<(String, Seen)>, WriteError> {
    let mut made: Vec<&str> = Vec::new();
    let mut seen = Vec::new();
    for step in steps {
        let copied = s3
            .copy(
                t,
                &step.from,
                &step.etag,
                step.size,
                &step.to,
                Cond::IfAbsent,
            )
            .await;
        match copied {
            Ok(head) => {
                made.push(&step.to);
                let path = relative(t, &step.to).unwrap_or(&step.to).to_owned();
                seen.push((path, seen_of(&head)));
            }
            Err(error) => {
                let answer = match error {
                    S3Error::Precondition => match exists(s3, t, &step.to).await {
                        Ok(true) => WriteError::NameTaken,
                        Ok(false) => WriteError::Changed,
                        Err(other) => other,
                    },
                    S3Error::Unsupported => WriteError::TooLargeToMove,
                    other => write_error(other),
                };
                for key in made {
                    // A copy that cannot be removed is a duplicate, never a loss.
                    let _ = s3.delete(t, key).await;
                }
                return Err(answer);
            }
        }
    }
    Ok(seen)
}

/// Moves the item at `from` to `to` (library-relative; a folder's paths end with `/`): every
/// object is copied, then every original deleted. `known` is what the tree recorded for the
/// item and everything inside it: library-relative paths and tags, folders ending `/`.
///
/// # Errors
/// `TooMany`, `TooLargeToMove`, `Changed` or `NameTaken` before any write; a failed copy's
/// error after its copies are removed; `Failed` for a path that is not a good library path or
/// a folder moved into itself.
pub async fn move_item(
    s3: &S3,
    t: &S3Target,
    from: &str,
    to: &str,
    folder: bool,
    known: &[(String, String)],
) -> Result<Moved, WriteError> {
    let from_key = item_key(t, from, folder).ok_or(WriteError::Failed)?;
    let to_key = item_key(t, to, folder).ok_or(WriteError::Failed)?;
    relocate(s3, t, &from_key, &to_key, folder, Some(known), true).await
}

/// Copies the item at `from` to `to`: [`move_item`] without the deletes.
///
/// # Errors
/// As [`move_item`].
pub async fn copy_item(
    s3: &S3,
    t: &S3Target,
    from: &str,
    to: &str,
    folder: bool,
    known: &[(String, String)],
) -> Result<Moved, WriteError> {
    let from_key = item_key(t, from, folder).ok_or(WriteError::Failed)?;
    let to_key = item_key(t, to, folder).ok_or(WriteError::Failed)?;
    relocate(s3, t, &from_key, &to_key, folder, Some(known), false).await
}

/// Moves the item at `from` into the holding folder, at `root` followed by `from`.
///
/// # Errors
/// As [`move_item`]; `Failed` for a root that is not a holding root or a holding key over
/// 1,024 bytes.
pub async fn hold(
    s3: &S3,
    t: &S3Target,
    from: &str,
    folder: bool,
    root: &str,
    known: &[(String, String)],
) -> Result<Moved, WriteError> {
    let from_key = item_key(t, from, folder).ok_or(WriteError::Failed)?;
    let to_key = holding_key(t, root, from, folder).ok_or(WriteError::Failed)?;
    relocate(s3, t, &from_key, &to_key, folder, Some(known), true).await
}

/// Moves the item held at `root` followed by `held` back to `to`. Tags are not checked, since
/// nothing outside writes the holding folder; a taken name is `NameTaken`, so the caller can
/// pick a numbered name and try again.
///
/// # Errors
/// As [`hold`].
pub async fn unhold(
    s3: &S3,
    t: &S3Target,
    root: &str,
    held: &str,
    to: &str,
    folder: bool,
) -> Result<Moved, WriteError> {
    let from_key = holding_key(t, root, held, folder).ok_or(WriteError::Failed)?;
    let to_key = item_key(t, to, folder).ok_or(WriteError::Failed)?;
    relocate(s3, t, &from_key, &to_key, folder, None, true).await
}

/// Deletes every object under the holding root `root`. Answers how many were deleted.
///
/// # Errors
/// `Failed` for a root that is not a holding root; a listing's error; after every key was
/// tried, the first failed delete's error.
pub async fn delete_held(s3: &S3, t: &S3Target, root: &str) -> Result<u64, WriteError> {
    if !is_root(root) {
        return Err(WriteError::Failed);
    }
    let prefix = format!("{}{root}", t.prefix());
    let keys: Vec<String> = list_all(s3, t, &prefix)
        .await?
        .into_iter()
        .map(|o| o.key)
        .collect();
    delete_keys(s3, t, &keys).await
}

/// Deletes the copies a step made (the destinations of `seen`, library-relative), best effort: a
/// copy that cannot be removed is a duplicate the next listing shows, never a loss. Answers how
/// many could not be removed.
pub async fn remove_copies(s3: &S3, t: &S3Target, seen: &[(String, Seen)]) -> u64 {
    let mut failed = 0;
    for (path, _) in seen {
        let Some(key) = item_key(t, path, path.ends_with('/')) else {
            failed += 1;
            continue;
        };
        if s3.delete(t, &key).await.is_err() {
            failed += 1;
        }
    }
    failed
}

/// Deletes `keys`, carrying on past failures. Answers the count, or the first failure's error.
async fn delete_keys(s3: &S3, t: &S3Target, keys: &[String]) -> Result<u64, WriteError> {
    let mut deleted = 0;
    let mut first_error = None;
    for key in keys {
        match s3.delete(t, key).await {
            Ok(()) => deleted += 1,
            Err(error) => {
                first_error.get_or_insert(write_error(error));
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(deleted),
    }
}

/// Whether a key under the holding folder (`rest`, after `.sharecraft-bin/`) is one the sweep
/// deletes: inside an entry folder whose date is before `older_than` and whose id is not kept.
fn swept(rest: &str, older_than: Date, keep: &HashSet<NodeId>) -> bool {
    let mut parts = rest.splitn(3, '/');
    let (Some(date), Some(entry), Some(inside)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !inside.is_empty()
        && parse_day(date).is_some_and(|date| date < older_than)
        && parse_entry(entry).is_some_and(|entry| !keep.contains(&entry))
}

/// Deletes the held objects nothing names any more: every key under the holding folder whose
/// date folder is before `older_than` and whose entry folder is a node id not in `keep`. Any
/// other key, including one whose date or id does not parse, is left alone. Answers how many
/// were deleted.
///
/// # Errors
/// A listing's error; after every key was tried, the first failed delete's error.
pub async fn sweep_holding(
    s3: &S3,
    t: &S3Target,
    older_than: Date,
    keep: &HashSet<NodeId>,
) -> Result<u64, WriteError> {
    let prefix = format!("{}{HOLDING}/", t.prefix());
    let keys: Vec<String> = list_all(s3, t, &prefix)
        .await?
        .into_iter()
        .map(|o| o.key)
        .filter(|key| {
            key.strip_prefix(prefix.as_str())
                .is_some_and(|rest| swept(rest, older_than, keep))
        })
        .collect();
    delete_keys(s3, t, &keys).await
}
