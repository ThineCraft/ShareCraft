//! Every change to a bucket: the safe write, the copy written beside a changed file, the folder
//! marker and the write probe. This is the only module that calls `S3::put` or `S3::delete`.
//!
//! A write never overwrites what it has not seen. It starts with a `HEAD`; on a store that
//! honours conditions the `PUT` carries `If-Match` or `If-None-Match`, so a change made between
//! the `HEAD` and the `PUT` is refused by the store and becomes a conflict. Every `PUT` records
//! the content's SHA-256 in object metadata, so a write whose answer was lost is recognised as
//! ours by the next attempt's `HEAD` rather than taken for a conflict. Nothing is retried here:
//! the caller's back-off retries, and every retry starts with a `HEAD`.

use records::{Tri, object_tag, reserved_name};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::s3::{Cond, ObjectHead, PutBody, S3, S3Error, S3Target};
use crate::s3_sign;
use crate::s3_source::{MAX_RELATIVE_CHARS, nameable_segment};
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
) -> Result<(String, Seen), WriteError> {
    check_size(s3, body)?;
    for name in names {
        let path = format!("{folder}{name}");
        let key = object_key(t, &path).ok_or(WriteError::Failed)?;
        match put_checked(s3, t, &key, body, Cond::IfAbsent).await {
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
) -> Result<(String, Seen), WriteError> {
    object_key(t, path).ok_or(WriteError::Failed)?;
    let (folder, name) = split_path(path);
    let first = sibling_name(name, person, at);
    put_first_free(s3, t, folder, &candidates(&first), body).await
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
) -> Result<(String, Seen), WriteError> {
    object_key(t, path).ok_or(WriteError::Failed)?;
    let (folder, name) = split_path(path);
    let mut names = vec![];
    names.extend((2..=LAST_NUMBER).map(|n| numbered_name(name, n)));
    put_first_free(s3, t, folder, &names, body).await
}

/// The body of a zero-byte object.
fn empty_body() -> PutBody {
    PutBody {
        path: None,
        size: 0,
        sha256: Sha256::digest(b"").into(),
    }
}

/// Makes the folder at `path`: a zero-byte object at `<key>/`, `If-None-Match`. A marker that is
/// already there is success. Nothing is deleted.
///
/// # Errors
/// [`WriteError`]; `Failed` for a path that is not a good folder name.
pub async fn make_folder(s3: &S3, t: &S3Target, path: &str) -> Result<(), WriteError> {
    let key = object_key(t, path).ok_or(WriteError::Failed)?;
    let key = format!("{key}/");
    if key.len() > MAX_KEY_BYTES {
        return Err(WriteError::Failed);
    }
    match put_checked(s3, t, &key, &empty_body(), Cond::IfAbsent).await {
        Ok(_) | Err(S3Error::Precondition) => Ok(()),
        Err(error) => Err(write_error(error)),
    }
}

/// Whether a refusal answers a condition: `Ok(true)` refused, `Ok(false)` accepted or not
/// understood (a store that rejects the header does not honour it), else why it failed.
fn refused(result: Result<ObjectHead, S3Error>) -> Result<bool, &'static str> {
    match result {
        Ok(_) => Ok(false),
        Err(S3Error::Precondition | S3Error::NotFound) => Ok(true),
        Err(S3Error::Unsupported | S3Error::Other | S3Error::Malformed | S3Error::Archived) => {
            Ok(false)
        }
        Err(error) => Err(reason(error)),
    }
}

/// Finds out whether the key may write to the library's prefix, and whether the store honours
/// conditional writes, with four requests on one key of ours: a `PUT` (`If-None-Match`), the same
/// again (refused when conditions work), a `PUT` with an `If-Match` that cannot match (refused
/// when conditions work) and a `DELETE`. Nothing that was there is touched.
///
/// The key is `<prefix>.sharecraft-tmp-probe-<uuid>`, which listings hide. If the `DELETE` fails
/// the answer is still what the probe found.
pub async fn probe_writes(s3: &S3, t: &S3Target) -> Probe {
    // Not built by `object_key`, which refuses names ShareCraft keeps for itself.
    let key = format!("{}.sharecraft-tmp-probe-{}", t.prefix(), Uuid::new_v4());
    let body = empty_body();
    match put_checked(s3, t, &key, &body, Cond::IfAbsent).await {
        Ok(_) => {}
        Err(S3Error::Denied) => return Probe::ReadOnly,
        Err(error) => return Probe::Unreachable(reason(error)),
    }
    let second = refused(put_checked(s3, t, &key, &body, Cond::IfAbsent).await);
    let third = match second {
        Ok(_) => refused(put_checked(s3, t, &key, &body, Cond::IfMatch("\"0\"")).await),
        Err(why) => Err(why),
    };
    // The key is ours whatever the answers were.
    // If this fails the finding stands, and listings hide the key.
    let _ = s3.delete(t, &key).await;
    match (second, third) {
        (Ok(true), Ok(true)) => Probe::ReadWrite {
            conditional: Tri::Yes,
        },
        (Ok(_), Ok(_)) => Probe::ReadWrite {
            conditional: Tri::No,
        },
        (Err(why), _) | (_, Err(why)) => Probe::Unreachable(why),
    }
}
