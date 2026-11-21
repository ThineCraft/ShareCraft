//! A library connected to a GitHub branch is read-only in ShareCraft: every route that changes
//! a node's content or place refuses inside it (`409 source_read_only`), for its owners and
//! for administrators alike, while the sync engine keeps changing it. Reading, listing,
//! previewing, sharing and access changes still work, a copy out of it works, and an editor
//! opens its files read-only.
//!
//! # The route table
//!
//! [`ROUTES`] classifies every route the server registers (see
//! [`a_route_list_guard`]). A route that touches a node's content or place is `Guarded`, and has
//! at least one row in [`ROWS`], which [`every_content_route_refuses_in_a_connected_library`]
//! and [`the_same_routes_work_in_an_ordinary_library`] drive. `Seat` is the editor websocket,
//! which opens a connected file with a viewer's seat. Every other route that changes something
//! is `Allowed`, with the reason it does not change the library's files. `Read` changes nothing.
//!
//! Accepted race (ruling of 16 November 2026): a handler checks the connection, then writes; a
//! `connect_source` committed between the two lets that one write through. Connecting needs an
//! empty library, so only a write into an empty library can slip in, and Task 6's connect route
//! abandons the library's upload sessions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use collab_core::ids::{DocId, NodeId};
use common::fake_github::FakeGithub;
use common::fake_share::FakeShare;
use common::{FileClient, TestServer, TestUser, spawn_server_config, start_fake_github};
use protocol::{FileServerMsg, SeatState};
use records::{NewSource, NodeKind, Principal, SHARE_ROOTS, SourceKind, SourceMode};
use reqwest::header::CONTENT_RANGE;
use reqwest::{Method, StatusCode};
use revisions::BlobRef;
use serde_json::{Value, json};
use server::AppState;
use server::source_sync::sync_once;
use time::OffsetDateTime;
use uuid::Uuid;

const OWNER: &str = "octo-org";
const REPO: &str = "docs";
#[cfg(windows)]
const SHARE_ROOT: &str = r"C:\source-readonly";
#[cfg(not(windows))]
const SHARE_ROOT: &str = "/source-readonly";
/// The refusal's message, word for word.
const MESSAGE: &str =
    "This library is kept in step with an outside source, so it cannot be changed here.";
const SEAT: &str = "x-sharecraft-seat";

/// What a route does to a library's files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// Changes nothing.
    Read,
    /// Touches a node's content or place: refused in a connected library.
    Guarded,
    /// The editor websocket: a connected file opens with a viewer's seat.
    Seat,
    /// Changes something, but not a library's files or where they are; the reason says why.
    Allowed(&'static str),
}

use Decision::{Allowed, Guarded, Read, Seat};

/// Every route the server registers, `METHOD /path` as the router names it, with the decision
/// made for it. [`a_route_list_guard`] fails when the router registers a route that is not
/// here, or no longer registers one that is: a new route cannot be added without deciding
/// whether a connected library refuses it.
const ROUTES: &[(&str, Decision)] = &[
    (
        "POST /api/session",
        Allowed("signs in; no library is touched"),
    ),
    ("DELETE /api/session", Allowed("signs out")),
    ("GET /api/me", Read),
    ("GET /api/editors", Read),
    ("GET /api/sites", Read),
    (
        "POST /api/sites",
        Allowed("makes a new site, with new unconnected libraries"),
    ),
    ("GET /api/sites/{id}", Read),
    (
        "PATCH /api/sites/{id}",
        Allowed("the site's own name and details, not a library's files"),
    ),
    (
        "DELETE /api/sites/{id}",
        // Ruling: a lifecycle exception; the library keeps its connection through a restore.
        Allowed("site lifecycle: the whole site goes to the bin, its connection with it"),
    ),
    (
        "PUT /api/sites/{id}/quota",
        Allowed("a storage limit, not files"),
    ),
    (
        "POST /api/sites/{id}/archive",
        Allowed("site lifecycle; an archived site is read-only anyway"),
    ),
    ("POST /api/sites/{id}/reactivate", Allowed("site lifecycle")),
    ("GET /api/sites/{id}/groups", Read),
    (
        "POST /api/sites/{id}/libraries",
        Allowed("makes a new library, which is not connected"),
    ),
    (
        "POST /api/sites/{id}/libraries/github",
        Allowed("makes a new library and connects it; no existing library's files change"),
    ),
    (
        "POST /api/sites/{id}/libraries/share",
        Allowed("makes a new library and connects it; no existing library's files change"),
    ),
    ("GET /api/libraries/{id}", Read),
    ("GET /api/libraries/{id}/source", Read),
    (
        "PATCH /api/libraries/{id}/source",
        Allowed("the connection's settings; only the sync changes the library's files"),
    ),
    (
        "DELETE /api/libraries/{id}/source",
        Allowed("disconnects: the files stay as they are and the library becomes editable"),
    ),
    (
        "POST /api/libraries/{id}/source/sync",
        Allowed("wakes the sync, which is what changes a connected library's files"),
    ),
    (
        "POST /api/libraries/{id}/source/webhook",
        Allowed("a webhook secret for the connection, not files"),
    ),
    (
        "PATCH /api/libraries/{id}",
        Allowed("the library's kind and settings (file policies), not its items"),
    ),
    ("GET /api/libraries/{id}/templates", Read),
    ("POST /api/libraries/{id}/from-template", Guarded),
    ("GET /api/assets", Read),
    ("PUT /api/sites/{id}/hub", Allowed("site navigation")),
    ("DELETE /api/sites/{id}/hub", Allowed("site navigation")),
    ("PUT /api/sites/{id}/joined-hub", Allowed("site navigation")),
    ("GET /api/sites/{id}/navigation", Read),
    ("PUT /api/sites/{id}/navigation", Allowed("site navigation")),
    ("GET /api/navigation", Read),
    ("PUT /api/navigation", Allowed("organisation navigation")),
    ("GET /api/sites/{id}/look", Read),
    ("PUT /api/sites/{id}/look", Allowed("the site's look")),
    ("GET /api/sites/{id}/look/settings", Read),
    ("GET /api/sites/{id}/look/history", Read),
    (
        "POST /api/sites/{id}/look/rollback",
        Allowed("the site's look"),
    ),
    ("POST /api/sites/{id}/look/copy", Allowed("the site's look")),
    (
        "POST /api/sites/{id}/font-packages",
        Allowed("the site's look"),
    ),
    (
        "PATCH /api/sites/{id}/font-packages/{package}",
        Allowed("the site's look"),
    ),
    (
        "DELETE /api/sites/{id}/font-packages/{package}",
        Allowed("the site's look"),
    ),
    ("POST /api/sites/{id}/themes", Allowed("the site's look")),
    (
        "PATCH /api/sites/{id}/themes/{theme}",
        Allowed("the site's look"),
    ),
    (
        "DELETE /api/sites/{id}/themes/{theme}",
        Allowed("the site's look"),
    ),
    ("GET /api/brand", Read),
    ("POST /api/brand/themes", Allowed("the brand centre")),
    ("PATCH /api/brand/themes/{id}", Allowed("the brand centre")),
    ("DELETE /api/brand/themes/{id}", Allowed("the brand centre")),
    (
        "POST /api/brand/images",
        Allowed("copies a file's current version into the brand centre; reads the file"),
    ),
    ("GET /api/brand/images/{id}", Read),
    ("PATCH /api/brand/images/{id}", Allowed("the brand centre")),
    ("DELETE /api/brand/images/{id}", Allowed("the brand centre")),
    ("PUT /api/brand/managers", Allowed("the brand centre")),
    (
        "POST /api/brand/fonts",
        Allowed("copies a font file into the brand centre; reads the file"),
    ),
    ("GET /api/brand/fonts/{id}", Read),
    ("DELETE /api/brand/fonts/{id}", Allowed("the brand centre")),
    ("POST /api/brand/font-packages", Allowed("the brand centre")),
    (
        "PATCH /api/brand/font-packages/{id}",
        Allowed("the brand centre"),
    ),
    (
        "DELETE /api/brand/font-packages/{id}",
        Allowed("the brand centre"),
    ),
    ("PUT /api/brand/policy", Allowed("the brand centre")),
    ("PUT /api/brand/default-icon", Allowed("the brand centre")),
    ("GET /api/hubs", Read),
    ("GET /api/hubs/{id}/sites", Read),
    (
        "POST /api/sites/{id}/template",
        Allowed("reads the site's shape into a saved template; changes no library"),
    ),
    ("GET /api/templates", Read),
    (
        "POST /api/templates/import",
        Allowed("the template catalogue"),
    ),
    (
        "DELETE /api/templates/{id}",
        Allowed("the template catalogue"),
    ),
    ("GET /api/templates/{id}/package", Read),
    ("GET /api/sites/{id}/bin", Read),
    // Ruling: a security and lifecycle exception, purging is always possible.
    (
        "DELETE /api/bin/{id}",
        Allowed(
            "acts only on recycle-bin entries, which are no longer the library's live \
             content; restoring one into the library is what is refused",
        ),
    ),
    ("POST /api/bin/{id}/restore", Guarded),
    ("POST /api/bulk", Guarded),
    ("GET /api/roles", Read),
    ("POST /api/roles", Allowed("role definitions")),
    ("PATCH /api/roles/{id}", Allowed("role definitions")),
    ("DELETE /api/roles/{id}", Allowed("role definitions")),
    ("GET /api/groups", Read),
    ("POST /api/groups", Allowed("groups: who, not what")),
    ("GET /api/groups/{id}", Read),
    ("DELETE /api/groups/{id}", Allowed("groups: who, not what")),
    (
        "POST /api/groups/{id}/members",
        Allowed("groups: who, not what"),
    ),
    (
        "PUT /api/groups/{id}/members/{user_id}",
        Allowed("groups: who, not what"),
    ),
    (
        "DELETE /api/groups/{id}/members/{user_id}",
        Allowed("groups: who, not what"),
    ),
    ("GET /api/nodes/{id}", Read),
    ("PATCH /api/nodes/{id}", Guarded),
    ("DELETE /api/nodes/{id}", Guarded),
    ("GET /api/nodes/{id}/children", Read),
    ("POST /api/nodes/{id}/children", Guarded),
    ("GET /api/nodes/{id}/access", Read),
    (
        "PUT /api/nodes/{id}/access",
        Allowed("access: changes who sees the files, not the files"),
    ),
    (
        "DELETE /api/nodes/{id}/access/{kind}/{principal_id}",
        Allowed("access: changes who sees the files, not the files"),
    ),
    (
        "POST /api/nodes/{id}/inheritance",
        Allowed("access: changes who sees the files, not the files"),
    ),
    ("POST /api/nodes/{id}/copy", Guarded),
    ("GET /api/nodes/{id}/links", Read),
    (
        "POST /api/nodes/{id}/links",
        Allowed("a sharing link: who sees the files, not the files"),
    ),
    (
        "DELETE /api/links/{id}",
        Allowed("a sharing link: who sees the files, not the files"),
    ),
    (
        "POST /api/nodes/{id}/requests",
        Allowed("a request for access: who, not what"),
    ),
    ("GET /api/requests", Read),
    ("GET /api/requests/mine", Read),
    (
        "POST /api/requests/{id}/approve",
        Allowed("grants access: who, not what"),
    ),
    (
        "POST /api/requests/{id}/decline",
        Allowed("a request for access"),
    ),
    ("GET /api/settings/sharing", Read),
    (
        "PUT /api/settings/sharing",
        Allowed("organisation settings"),
    ),
    ("GET /api/settings/regional", Read),
    (
        "PUT /api/settings/regional",
        Allowed("organisation settings"),
    ),
    ("GET /api/settings/files", Read),
    (
        "PUT /api/settings/files",
        Allowed("organisation file policies"),
    ),
    ("GET /api/settings/scanner", Read),
    (
        "PUT /api/settings/scanner",
        Allowed("organisation settings"),
    ),
    ("GET /api/settings/shares", Read),
    ("PUT /api/settings/shares", Allowed("organisation settings")),
    ("GET /api/nodes/{id}/inheritance/preview", Read),
    ("GET /api/explorer/{root}", Read),
    ("GET /api/explorer/{root}/counts", Read),
    ("GET /api/explorer/{root}/export.csv", Read),
    ("POST /api/docs", Guarded),
    ("POST /api/docs/upload", Guarded),
    ("POST /api/uploads", Guarded),
    ("GET /api/uploads/{id}", Read),
    ("PUT /api/uploads/{id}", Guarded),
    (
        "DELETE /api/uploads/{id}",
        Allowed("abandons an unfinished upload; nothing reaches the library"),
    ),
    ("POST /api/uploads/{id}/finish", Guarded),
    ("GET /api/docs/{id}", Read),
    ("GET /api/docs/{id}/content", Read),
    ("GET /api/docs/{id}/live-base", Read),
    ("PUT /api/docs/{id}/content", Guarded),
    ("PUT /api/docs/{id}/live-checkpoint", Guarded),
    ("POST /api/docs/{id}/live-checkpoint/hash", Guarded),
    ("GET /api/docs/{id}/preview", Read),
    ("GET /api/docs/{id}/thumbnail", Read),
    ("GET /api/docs/{id}/access", Read),
    (
        "PUT /api/docs/{id}/access",
        Allowed("access: changes who sees the file, not the file"),
    ),
    ("GET /api/docs/{id}/events", Read),
    (
        "DELETE /api/docs/{id}/access/{kind}/{principal_id}",
        Allowed("access: changes who sees the file, not the file"),
    ),
    ("GET /api/admin/users", Read),
    ("POST /api/admin/users", Allowed("accounts")),
    ("POST /api/admin/users/{id}/disable", Allowed("accounts")),
    ("POST /api/admin/users/{id}/enable", Allowed("accounts")),
    ("POST /api/admin/looks/apply", Allowed("sites' looks")),
    ("GET /api/admin/sites", Read),
    // Ruling: a lifecycle exception; the purge removes the connection, its items and secrets.
    (
        "DELETE /api/admin/sites/{id}",
        Allowed("site lifecycle: purges a deleted site, its connection with it"),
    ),
    // Ruling: a lifecycle exception, as deleting the site is.
    (
        "POST /api/admin/sites/{id}/restore",
        Allowed("site lifecycle"),
    ),
    (
        "POST /api/admin/scan-existing",
        Allowed("queues content for the malware scanner; files stay as they are"),
    ),
    ("GET /api/admin/quarantine", Read),
    (
        "POST /api/admin/quarantine/{sha256}/release",
        Allowed("lets held content through: its scan state, not the files"),
    ),
    // Ruling: a security exception; the purge removes the file's item record too.
    (
        "POST /api/admin/quarantine/{sha256}/delete",
        Allowed(
            "malware response: an administrator must be able to purge infected content \
             wherever it is; the next sync records the loss",
        ),
    ),
    ("GET /api/l/{token}", Read),
    (
        "POST /api/l/{token}/redeem",
        Allowed("opens a sharing link: joins its hidden group, who not what"),
    ),
    ("GET /api/l/{token}/children/{node}", Read),
    ("GET /api/l/{token}/content/{node}", Read),
    ("GET /api/l/{token}/text/{node}", Read),
    ("GET /api/l/{token}/preview/{node}", Read),
    ("GET /healthz", Read),
    ("GET /ws/docs/{doc_id}", Seat),
    ("GET /l/{token}", Read),
    (
        "POST /hooks/github/{library}",
        Allowed("only wakes the sync worker, which keeps the library in step with its branch"),
    ),
];

/// The routers nested or merged into the server's, besides the routes above, as the source
/// writes them (whitespace removed). A new one fails [`a_route_list_guard`] until its routes are
/// parsed and classified too.
const MOUNTS: &[&str] = &[
    // lib.rs: the static files of `web/`, read only.
    "fallback_service(ServeDir::new(&state.config().web_dir))",
    "nest(\"/api\",api::router())",
    "merge(editors::router(state.clone()))",
    "fallback_service(files)",
    // api/mod.rs: the link routes, parsed from links.rs.
    "nest(\"/l\",crate::links::api_router())",
    // editors.rs: an installed editor's static files, read only.
    "nest_service(\"/editors\",files)",
    // api/mod.rs: unknown paths answer 404.
    "fallback(||async{ApiError::NotFound})",
    // links.rs: unknown link paths answer 404.
    "fallback(||async{ApiError::LinkNotFound})",
];

/// Calls on a router that register something. Any other call whose name mentions one of
/// [`REGISTERING_WORDS`] fails [`registered`]: it may register routes this parser cannot see.
const REGISTRATIONS: &[&str] = &[
    "route",
    "route_service",
    "nest",
    "nest_service",
    "merge",
    "fallback",
    "fallback_service",
];
const REGISTERING_WORDS: &[&str] = &["route", "nest", "merge", "fallback", "service"];

/// The routes and mounts registered between `start` and `end` in the source file `file` (below
/// `crates/server/src`), each route as `METHOD <prefix><path>` (`SERVICE <prefix><path>` for a
/// `route_service`). Fails on any registration it cannot read: a route without a literal path
/// or without a method it knows, a method router built otherwise (`any`, `on`, a service), or a
/// registering call it does not know.
fn registered(file: &str, start: &str, end: &str, prefix: &str) -> (Vec<String>, Vec<String>) {
    let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("{start} in {file}"));
    let to = from
        + text[from..]
            .find(end)
            .unwrap_or_else(|| panic!("{end} in {file}"));
    let body = &text[from..to];
    let mut routes = Vec::new();
    let mut mounts = Vec::new();
    let mut at = 0;
    while let Some((name, open)) = next_call(body, at) {
        if !REGISTRATIONS.contains(&name.as_str()) {
            assert!(
                !REGISTERING_WORDS.iter().any(|w| name.contains(w)),
                "{file}: cannot classify the routes of `.{name}(`"
            );
            at = open + 1;
            continue;
        }
        let close = matching(body, open);
        let args = &body[open + 1..close];
        if name == "route" || name == "route_service" {
            let path = literal_path(args)
                .unwrap_or_else(|| panic!("{file}: a route without a literal path: {args}"));
            let rest = &args[args.find('"').unwrap_or(0) + path.len() + 2..];
            if name == "route_service" {
                routes.push(format!("SERVICE {prefix}{path}"));
            } else {
                let found = methods(rest);
                assert!(
                    !found.is_empty(),
                    "{file}: no method found for {path}: {rest}"
                );
                routes.extend(found.into_iter().map(|m| format!("{m} {prefix}{path}")));
            }
        } else {
            let squeezed: String = args.chars().filter(|c| !c.is_whitespace()).collect();
            mounts.push(format!("{name}({squeezed})"));
        }
        at = close;
    }
    (routes, mounts)
}

/// The string literal that starts `args`, after whitespace.
fn literal_path(args: &str) -> Option<&str> {
    let trimmed = args.trim_start();
    let inner = trimmed.strip_prefix('"')?;
    let len = inner.find('"')?;
    Some(&inner[..len])
}

/// The next method call (`.name(`) at or after `at`: its name and where its `(` is.
fn next_call(body: &str, at: usize) -> Option<(String, usize)> {
    let bytes = body.as_bytes();
    let mut i = at;
    while let Some(dot) = body[i..].find('.') {
        let start = i + dot + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        if end > start && end < bytes.len() && bytes[end] == b'(' {
            return Some((body[start..end].to_owned(), end));
        }
        i = start;
    }
    None
}

/// Whether `text` calls `word` (`name(`) as a whole name, not as the end of a longer one.
fn called(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| {
        !text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// Where the `)` that closes the `(` at `open` is.
fn matching(body: &str, open: usize) -> usize {
    let mut depth = 0;
    for (i, c) in body[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return open + i;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parentheses in the router source")
}

/// The HTTP methods a route's method router names (`get(…).post(…)`), in order.
fn methods(text: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    for (word, method) in [
        ("get(", "GET"),
        ("post(", "POST"),
        ("put(", "PUT"),
        ("patch(", "PATCH"),
        ("delete(", "DELETE"),
    ] {
        let mut from = 0;
        while let Some(i) = text[from..].find(word) {
            let at = from + i;
            let before = text[..at].chars().next_back();
            if !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == ':') {
                found.push((at, method));
            }
            from = at + word.len();
        }
    }
    // Method routers built any other way would hide their methods from this parser.
    for word in [
        "any(",
        "on(",
        "on_service(",
        "get_service(",
        "post_service(",
    ] {
        assert!(
            !called(text, word),
            "a method router this parser cannot read: {text}"
        );
    }
    assert!(
        !text.contains("MethodRouter") && !text.contains("method_routing"),
        "a method router this parser cannot read: {text}"
    );
    found.sort_unstable();
    found.into_iter().map(|(_, method)| method).collect()
}

/// Parses the routers' source (`lib.rs`, `api/mod.rs`, `links.rs`, `editors.rs`) for every
/// route registration and compares the list with [`ROUTES`], and the nested and merged routers
/// with [`MOUNTS`]. When this fails, a route was added (or removed): decide whether it touches
/// a node's content or place, guard it in its handler if it does (and add a row to [`ROWS`]),
/// and record the decision in [`ROUTES`].
#[test]
fn a_route_list_guard() {
    let mut routes = Vec::new();
    let mut mounts = Vec::new();
    for (file, start, end, prefix) in [
        ("lib.rs", "pub fn router(", "\n}", ""),
        ("api/mod.rs", "pub(crate) fn router()", "\n}", "/api"),
        ("links.rs", "pub(crate) fn api_router()", "\n}", "/api/l"),
        (
            "editors.rs",
            "pub(crate) fn router(state: AppState)",
            "\n}",
            "/editors",
        ),
    ] {
        let (r, m) = registered(file, start, end, prefix);
        routes.extend(r);
        mounts.extend(m);
    }
    let registered: BTreeSet<&str> = routes.iter().map(String::as_str).collect();
    assert_eq!(
        registered.len(),
        routes.len(),
        "a route is registered twice"
    );
    let recorded: BTreeSet<&str> = ROUTES.iter().map(|(route, _)| *route).collect();
    assert_eq!(recorded.len(), ROUTES.len(), "a route is recorded twice");
    let unclassified: Vec<&&str> = registered.difference(&recorded).collect();
    let gone: Vec<&&str> = recorded.difference(&registered).collect();
    assert!(
        unclassified.is_empty() && gone.is_empty(),
        "routes not classified in ROUTES: {unclassified:?}; recorded but not registered: {gone:?}"
    );
    assert_eq!(routes.len(), 157, "the number of registered routes changed");
    let mut mounts_sorted = mounts.clone();
    mounts_sorted.sort();
    let mut expected: Vec<String> = MOUNTS.iter().map(|m| (*m).to_owned()).collect();
    expected.sort();
    assert_eq!(mounts_sorted, expected, "a router was nested or merged");

    // Every guarded route is driven by at least one row, and every row names a guarded route.
    for (route, decision) in ROUTES {
        if *decision == Guarded {
            assert!(
                ROWS.iter().any(|row| row.route == *route),
                "{route} has no row"
            );
        }
        // A changing route is never left as a read.
        if *decision == Read {
            assert!(route.starts_with("GET "), "{route} changes something");
        }
    }
    for row in ROWS {
        assert!(
            ROUTES.contains(&(row.route, Guarded)),
            "{} is not a guarded route",
            row.route
        );
    }
}

/// One request a row makes. Each acts on its own item of the fixture (see [`Fixture`]), so the
/// rows can run one after the other in an ordinary library, where they succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    CreateFolder,
    CreateDoc,
    Upload,
    UploadReplacing,
    StartNewFile,
    StartNewVersion,
    Chunk,
    Finish,
    Rename,
    MoveWithin,
    MoveIn,
    MoveOut,
    CopyIn,
    CopyWithin,
    Delete,
    Restore,
    BulkMove,
    BulkMoveIn,
    BulkMoveOut,
    BulkCopyIn,
    BulkDelete,
    BulkRestore,
    FromTemplate,
    Save,
    LiveCheckpoint,
    LiveHash,
    RenameFolder,
    RenameLibrary,
    DeleteFolder,
    DeleteLibrary,
}

/// A row: the request, the route it goes to, and its status in an ordinary library.
struct Row {
    act: Act,
    route: &'static str,
    ok: u16,
}

const fn row(act: Act, route: &'static str, ok: u16) -> Row {
    Row { act, route, ok }
}

/// The table: one row per way a guarded route can change a library. Destructive rows (the
/// folder and the library themselves) come last.
const ROWS: &[Row] = &[
    row(Act::CreateFolder, "POST /api/nodes/{id}/children", 201),
    row(Act::CreateDoc, "POST /api/docs", 201),
    row(Act::Upload, "POST /api/docs/upload", 201),
    row(Act::UploadReplacing, "POST /api/docs/upload", 200),
    row(Act::StartNewFile, "POST /api/uploads", 201),
    row(Act::StartNewVersion, "POST /api/uploads", 201),
    row(Act::Chunk, "PUT /api/uploads/{id}", 200),
    row(Act::Finish, "POST /api/uploads/{id}/finish", 201),
    row(Act::Rename, "PATCH /api/nodes/{id}", 200),
    row(Act::MoveWithin, "PATCH /api/nodes/{id}", 200),
    row(Act::MoveIn, "PATCH /api/nodes/{id}", 200),
    row(Act::MoveOut, "PATCH /api/nodes/{id}", 200),
    row(Act::CopyIn, "POST /api/nodes/{id}/copy", 201),
    row(Act::CopyWithin, "POST /api/nodes/{id}/copy", 201),
    row(Act::Delete, "DELETE /api/nodes/{id}", 204),
    row(Act::Restore, "POST /api/bin/{id}/restore", 200),
    row(Act::BulkMove, "POST /api/bulk", 200),
    row(Act::BulkMoveIn, "POST /api/bulk", 200),
    row(Act::BulkMoveOut, "POST /api/bulk", 200),
    row(Act::BulkCopyIn, "POST /api/bulk", 200),
    row(Act::BulkDelete, "POST /api/bulk", 200),
    row(Act::BulkRestore, "POST /api/bulk", 200),
    row(
        Act::FromTemplate,
        "POST /api/libraries/{id}/from-template",
        201,
    ),
    row(Act::Save, "PUT /api/docs/{id}/content", 200),
    // In an ordinary library these reach the live room's ticket check: the file has no live
    // room here, so no ticket can be held (403). A connected library refuses them first.
    row(
        Act::LiveCheckpoint,
        "PUT /api/docs/{id}/live-checkpoint",
        403,
    ),
    row(
        Act::LiveHash,
        "POST /api/docs/{id}/live-checkpoint/hash",
        403,
    ),
    row(Act::RenameFolder, "PATCH /api/nodes/{id}", 200),
    row(Act::RenameLibrary, "PATCH /api/nodes/{id}", 200),
    row(Act::DeleteFolder, "DELETE /api/nodes/{id}", 204),
    row(Act::DeleteLibrary, "DELETE /api/nodes/{id}", 204),
];

/// The files of the fixture's `docs` folder, each for its own row.
const DOCS_FILES: &[&str] = &[
    "rename-me.txt",
    "move-me.txt",
    "move-out.txt",
    "delete-me.txt",
    "copy-me.txt",
    "version.txt",
    "replace-me.txt",
    "bulk-move.txt",
    "bulk-move-out.txt",
    "bulk-delete.txt",
    "sheet.xlsx",
    "read-me.txt",
];
/// Files of `docs` that are in the recycle bin once the fixture is made.
const BINNED: &[&str] = &["restore-me.txt", "bulk-restore.txt"];
/// The files of the fixture's `doomed` folder, which a row deletes, and of `other`.
const DOOMED_FILES: &[&str] = &["inside.txt"];
const OTHER_FILES: &[&str] = &["kept.txt"];
/// The files of the folder outside the library.
const OUTSIDE_FILES: &[&str] = &[
    "move-in.txt",
    "copy-in.txt",
    "bulk-move-in.txt",
    "bulk-copy-in.txt",
];

fn bytes_of(name: &str) -> Vec<u8> {
    format!("contents of {name}\n").into_bytes()
}

fn sha_hex(bytes: &[u8]) -> String {
    BlobRef::of(bytes).hex()
}

/// A server, a fake GitHub, an administrator, Olive (owner of the site), Bob (no access) and
/// a template library with a template in the site.
struct Env {
    server: TestServer,
    fake: FakeGithub,
    share: FakeShare,
    state: AppState,
    admin: TestUser,
    olive: TestUser,
    bob: TestUser,
    site: String,
    /// The harness's own library, where the folders outside the fixtures' libraries go.
    elsewhere: String,
    template: String,
}

/// A library and its items, made by [`Env::fixture`]; ids as text.
struct Fixture {
    library: String,
    docs: String,
    other: String,
    doomed: String,
    /// Outside the library.
    outside: String,
    /// By name: the files of `docs`, `other`, `doomed` and the outside folder, and the binned
    /// ones.
    items: HashMap<String, String>,
    /// Per user: an upload session with every byte received (to finish) and an empty one (to
    /// send a chunk to), both for new files in the library.
    sessions: HashMap<String, (String, String)>,
}

impl Fixture {
    fn item(&self, name: &str) -> &str {
        self.items
            .get(name)
            .unwrap_or_else(|| panic!("{name} is not in the fixture"))
    }
}

impl Env {
    async fn new() -> Env {
        let fake = start_fake_github(OWNER, REPO).await;
        let origin = fake.origin();
        let server = spawn_server_config(|c| c.github_api = origin).await;
        let admin = server.admin("Ada Admin").await;
        let olive = server.user("Olive Owner").await;
        let bob = server.user("Bob Nobody").await;
        let elsewhere = common::Site::library(&server).await;
        let state = server.app_state().await;
        let share = FakeShare::new(SHARE_ROOT);
        state.shares().set_fs(Arc::new(share.clone()));
        let db = server.records_db();
        db.set_setting(admin.id, SHARE_ROOTS, json!([SHARE_ROOT]))
            .await
            .unwrap();
        let site = db.node(elsewhere).await.unwrap().unwrap().site;
        db.set_node_access(admin.id, site, Principal::User(olive.id), Some("owner"))
            .await
            .unwrap();
        let mut env = Env {
            server,
            fake,
            share,
            state,
            admin,
            olive,
            bob,
            site: site.0.to_string(),
            elsewhere: elsewhere.0.to_string(),
            template: String::new(),
        };
        let templates = env.new_library("Templates", "template").await;
        env.template = env.upload(&templates, "Budget.xlsx").await;
        env
    }

    async fn call(
        &self,
        user: &TestUser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = self
            .server
            .http(Some(user))
            .request(method, self.server.url(path));
        if let Some(body) = body {
            request = request.json(&body);
        }
        answer(request.send().await.unwrap()).await
    }

    async fn new_library(&self, name: &str, kind: &str) -> String {
        let (status, body) = self
            .call(
                &self.admin,
                Method::POST,
                &format!("/api/sites/{}/libraries", self.site),
                Some(json!({ "name": name, "kind": kind })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    async fn folder(&self, parent: &str, name: &str) -> String {
        let (status, body) = self
            .call(
                &self.admin,
                Method::POST,
                &format!("/api/nodes/{parent}/children"),
                Some(json!({ "kind": "folder", "name": name })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// Uploads `name` (with [`bytes_of`] it) into `parent` as the administrator; its id.
    async fn upload(&self, parent: &str, name: &str) -> String {
        let response = self
            .server
            .http(Some(&self.admin))
            .post(self.server.url("/api/docs/upload"))
            .query(&[("parent", parent), ("name", name)])
            .body(bytes_of(name))
            .send()
            .await
            .unwrap();
        let (status, body) = answer(response).await;
        assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
        body["id"].as_str().unwrap().to_owned()
    }

    /// The live children of `parent`, by name, as the administrator sees them.
    async fn children(&self, parent: &str) -> HashMap<String, String> {
        let (status, page) = self
            .call(
                &self.admin,
                Method::GET,
                &format!("/api/nodes/{parent}/children"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["name"].as_str().unwrap().to_owned(),
                    item["id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// Every live item at or below `node`: `(path, id)`, sorted.
    async fn tree(&self, node: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut todo = vec![(String::new(), node.to_owned())];
        while let Some((path, id)) = todo.pop() {
            for (name, child) in self.children(&id).await {
                let child_path = format!("{path}/{name}");
                let kind = self
                    .server
                    .records_db()
                    .node(NodeId(child.parse().unwrap()))
                    .await
                    .unwrap()
                    .unwrap()
                    .kind;
                if kind == NodeKind::Folder {
                    todo.push((child_path.clone(), child.clone()));
                }
                out.push((child_path, child));
            }
        }
        out.sort();
        out
    }

    /// An upload session started by `user` for a new file named `name` in `parent`, of two
    /// bytes; with `full`, both bytes sent.
    async fn session(&self, user: &TestUser, parent: &str, name: &str, full: bool) -> String {
        let (status, body) = self
            .call(
                user,
                Method::POST,
                "/api/uploads",
                Some(json!({
                    "parent": parent,
                    "name": name,
                    "size": 2,
                    "sha256": sha_hex(b"ab"),
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_owned();
        if full {
            let (status, body) = self.chunk(user, &id).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        id
    }

    async fn chunk(&self, user: &TestUser, session: &str) -> (StatusCode, Value) {
        let response = self
            .server
            .http(Some(user))
            .put(self.server.url(&format!("/api/uploads/{session}")))
            .header(CONTENT_RANGE, "bytes 0-1/2")
            .body(b"ab".to_vec())
            .send()
            .await
            .unwrap();
        answer(response).await
    }

    async fn sync(&self, library: &str) {
        let library = NodeId(library.parse().unwrap());
        sync_once(&self.state, library, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .expect("the library changed");
    }

    /// A library with a `docs` folder of [`DOCS_FILES`] (and [`BINNED`] in the bin), a
    /// `doomed` folder of [`DOOMED_FILES`] and an `other` folder of [`OTHER_FILES`]; a folder
    /// of [`OUTSIDE_FILES`] outside it; and upload sessions for each of `users`. With
    /// `connected`, the library is connected to the fake's branch and its items come from the
    /// sync; otherwise the administrator makes them.
    async fn fixture(&self, connected: bool, users: &[&TestUser]) -> Fixture {
        self.fixture_for(connected.then_some(SourceKind::Github), users)
            .await
    }

    async fn share_fixture(&self, users: &[&TestUser]) -> Fixture {
        self.fixture_for(Some(SourceKind::Share), users).await
    }

    async fn fixture_for(&self, source: Option<SourceKind>, users: &[&TestUser]) -> Fixture {
        let suffix = Uuid::new_v4().simple().to_string();
        let library = self
            .new_library(&format!("Library {suffix}"), "document")
            .await;
        let outside = self
            .folder(&self.elsewhere, &format!("Outside {suffix}"))
            .await;
        let mut items = HashMap::new();
        for name in OUTSIDE_FILES {
            items.insert((*name).to_owned(), self.upload(&outside, name).await);
        }
        // Started while the library is empty, so the connection is possible after.
        let mut sessions = HashMap::new();
        for user in users {
            let finish = self
                .session(user, &library, &format!("finish-{}.txt", user.id.0), true)
                .await;
            let chunk = self
                .session(user, &library, &format!("chunk-{}.txt", user.id.0), false)
                .await;
            sessions.insert(user.email.clone(), (finish, chunk));
        }
        let (docs, other, doomed);
        if let Some(source) = source {
            self.connect(&library, source).await;
            let mut tree: Vec<(String, Vec<u8>)> = Vec::new();
            for name in DOCS_FILES.iter().chain(BINNED) {
                tree.push((format!("docs/{name}"), bytes_of(name)));
            }
            for name in DOOMED_FILES {
                tree.push((format!("doomed/{name}"), bytes_of(name)));
            }
            for name in OTHER_FILES {
                tree.push((format!("other/{name}"), bytes_of(name)));
            }
            let files: Vec<(&str, Vec<u8>)> =
                tree.iter().map(|(p, b)| (p.as_str(), b.clone())).collect();
            match source {
                SourceKind::Github => {
                    self.fake.commit("main", &files);
                }
                SourceKind::Share => {
                    for (path, bytes) in &files {
                        self.share.put(path, bytes);
                    }
                }
            }
            self.sync(&library).await;
            let top = self.children(&library).await;
            docs = top["docs"].clone();
            other = top["other"].clone();
            doomed = top["doomed"].clone();
            items.extend(self.children(&docs).await);
            items.extend(self.children(&other).await);
            items.extend(self.children(&doomed).await);
            // The binned files leave the branch: the sync deletes them to the bin.
            let kept: Vec<(&str, Vec<u8>)> = files
                .into_iter()
                .filter(|(path, _)| !BINNED.iter().any(|b| path.ends_with(b)))
                .collect();
            match source {
                SourceKind::Github => {
                    self.fake.commit("main", &kept);
                    self.sync(&library).await;
                }
                SourceKind::Share => {
                    for name in BINNED {
                        self.share.remove(&format!("docs/{name}"));
                    }
                    let library_id = NodeId(library.parse().unwrap());
                    let missing_at = OffsetDateTime::now_utc();
                    sync_once(&self.state, library_id, missing_at)
                        .await
                        .unwrap()
                        .expect("the share changed");
                    sync_once(
                        &self.state,
                        library_id,
                        missing_at + time::Duration::seconds(301),
                    )
                    .await
                    .unwrap()
                    .expect("the missing share files were recycled");
                }
            }
            let live = self.children(&docs).await;
            for name in BINNED {
                assert!(!live.contains_key(*name), "{name} is still live");
            }
        } else {
            docs = self.folder(&library, "docs").await;
            other = self.folder(&library, "other").await;
            doomed = self.folder(&library, "doomed").await;
            for name in DOCS_FILES.iter().chain(BINNED) {
                items.insert((*name).to_owned(), self.upload(&docs, name).await);
            }
            for name in DOOMED_FILES {
                items.insert((*name).to_owned(), self.upload(&doomed, name).await);
            }
            for name in OTHER_FILES {
                items.insert((*name).to_owned(), self.upload(&other, name).await);
            }
            for name in BINNED {
                let (status, body) = self
                    .call(
                        &self.admin,
                        Method::DELETE,
                        &format!("/api/nodes/{}", items[*name]),
                        None,
                    )
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            }
        }
        for name in DOCS_FILES.iter().chain(BINNED).chain(OTHER_FILES) {
            assert!(items.contains_key(*name), "{name} is missing");
        }
        Fixture {
            library,
            docs,
            other,
            doomed,
            outside,
            items,
            sessions,
        }
    }

    /// Connects the empty `library` to the fake's `octo-org/docs` branch `main` (the records
    /// API: the HTTP route comes with Task 6).
    async fn connect(&self, library: &str, kind: SourceKind) {
        self.server
            .records_db()
            .connect_source(&NewSource {
                library: NodeId(library.parse().unwrap()),
                kind,
                owner: if kind == SourceKind::Github {
                    OWNER.to_owned()
                } else {
                    String::new()
                },
                repo: if kind == SourceKind::Github {
                    REPO.to_owned()
                } else {
                    String::new()
                },
                branch: if kind == SourceKind::Github {
                    "main".to_owned()
                } else {
                    String::new()
                },
                folder: String::new(),
                location: (kind == SourceKind::Share).then(|| SHARE_ROOT.to_owned()),
                mode: SourceMode::ReadOnly,
                token: None,
                poll_seconds: 300,
                connected_by: self.admin.id,
            })
            .await
            .unwrap();
    }

    /// The current revision of the document `doc`.
    async fn revision(&self, doc: &str) -> Option<String> {
        let (status, body) = self
            .call(&self.admin, Method::GET, &format!("/api/docs/{doc}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["revision"].as_str().map(str::to_owned)
    }

    /// Does `act` on `fx` as `user`.
    async fn perform(&self, fx: &Fixture, user: &TestUser, act: Act) -> (StatusCode, Value) {
        let (method, path, body) = match act {
            Act::CreateFolder => (
                Method::POST,
                format!("/api/nodes/{}/children", fx.docs),
                json!({ "kind": "folder", "name": "New folder" }),
            ),
            Act::CreateDoc => (
                Method::POST,
                "/api/docs".to_owned(),
                json!({ "parent": fx.docs, "name": "Notes" }),
            ),
            Act::Upload | Act::UploadReplacing => {
                let (name, mut query) = if act == Act::Upload {
                    ("uploaded.txt", vec![])
                } else {
                    ("replace-me.txt", vec![("on_conflict", "replace")])
                };
                query.push(("parent", fx.docs.as_str()));
                query.push(("name", name));
                let response = self
                    .server
                    .http(Some(user))
                    .post(self.server.url("/api/docs/upload"))
                    .query(&query)
                    .body(b"new bytes".to_vec())
                    .send()
                    .await
                    .unwrap();
                return answer(response).await;
            }
            Act::StartNewFile => (
                Method::POST,
                "/api/uploads".to_owned(),
                json!({
                    "parent": fx.docs,
                    "name": "started.txt",
                    "size": 2,
                    "sha256": sha_hex(b"ab"),
                }),
            ),
            Act::StartNewVersion => {
                let doc = fx.item("version.txt");
                (
                    Method::POST,
                    "/api/uploads".to_owned(),
                    json!({
                        "doc": doc,
                        "size": 2,
                        "sha256": sha_hex(b"ab"),
                        "base": self.revision(doc).await,
                    }),
                )
            }
            Act::Chunk => return self.chunk(user, &fx.sessions[&user.email].1).await,
            Act::Finish => (
                Method::POST,
                format!("/api/uploads/{}/finish", fx.sessions[&user.email].0),
                Value::Null,
            ),
            Act::Rename => (
                Method::PATCH,
                format!("/api/nodes/{}", fx.item("rename-me.txt")),
                json!({ "name": "renamed.txt" }),
            ),
            Act::MoveWithin => (
                Method::PATCH,
                format!("/api/nodes/{}", fx.item("move-me.txt")),
                json!({ "parent": fx.other }),
            ),
            Act::MoveIn => (
                Method::PATCH,
                format!("/api/nodes/{}", fx.item("move-in.txt")),
                json!({ "parent": fx.docs }),
            ),
            Act::MoveOut => (
                Method::PATCH,
                format!("/api/nodes/{}", fx.item("move-out.txt")),
                json!({ "parent": fx.outside }),
            ),
            Act::CopyIn => (
                Method::POST,
                format!("/api/nodes/{}/copy", fx.item("copy-in.txt")),
                json!({ "parent": fx.docs }),
            ),
            Act::CopyWithin => (
                Method::POST,
                format!("/api/nodes/{}/copy", fx.item("copy-me.txt")),
                json!({ "parent": fx.other }),
            ),
            Act::Delete => (
                Method::DELETE,
                format!("/api/nodes/{}", fx.item("delete-me.txt")),
                Value::Null,
            ),
            Act::Restore => (
                Method::POST,
                format!("/api/bin/{}/restore", fx.item("restore-me.txt")),
                Value::Null,
            ),
            Act::BulkMove => bulk("move", fx.item("bulk-move.txt"), Some(&fx.other)),
            Act::BulkMoveIn => bulk("move", fx.item("bulk-move-in.txt"), Some(&fx.docs)),
            Act::BulkMoveOut => bulk("move", fx.item("bulk-move-out.txt"), Some(&fx.outside)),
            Act::BulkCopyIn => bulk("copy", fx.item("bulk-copy-in.txt"), Some(&fx.docs)),
            Act::BulkDelete => bulk("delete", fx.item("bulk-delete.txt"), None),
            Act::BulkRestore => bulk("restore", fx.item("bulk-restore.txt"), None),
            Act::FromTemplate => (
                Method::POST,
                format!("/api/libraries/{}/from-template", fx.library),
                json!({ "template": self.template, "parent": fx.docs }),
            ),
            Act::Save => return self.save(fx, user).await,
            Act::LiveCheckpoint => (
                Method::PUT,
                format!(
                    "/api/docs/{}/live-checkpoint?ticket={}&at=0",
                    fx.item("sheet.xlsx"),
                    Uuid::new_v4()
                ),
                Value::Null,
            ),
            Act::LiveHash => (
                Method::POST,
                format!("/api/docs/{}/live-checkpoint/hash", fx.item("sheet.xlsx")),
                json!({
                    "ticket": Uuid::new_v4(),
                    "at": 0,
                    "sha256": "ab".repeat(32),
                    "session_sha256": "cd".repeat(32),
                }),
            ),
            Act::RenameFolder => (
                Method::PATCH,
                format!("/api/nodes/{}", fx.docs),
                json!({ "name": "papers" }),
            ),
            Act::RenameLibrary => (
                Method::PATCH,
                format!("/api/nodes/{}", fx.library),
                json!({ "name": format!("Renamed {}", Uuid::new_v4().simple()) }),
            ),
            Act::DeleteFolder => (
                Method::DELETE,
                format!("/api/nodes/{}", fx.doomed),
                Value::Null,
            ),
            Act::DeleteLibrary => (
                Method::DELETE,
                format!("/api/nodes/{}", fx.library),
                Value::Null,
            ),
        };
        let body = (!body.is_null()).then_some(body);
        self.call(user, method, &path, body).await
    }

    /// A save of `sheet.xlsx` through `PUT /api/docs/{id}/content`, by `user` from an editor
    /// they opened: with the seat's token when they got it, and with a made-up one otherwise.
    async fn save(&self, fx: &Fixture, user: &TestUser) -> (StatusCode, Value) {
        let doc = fx.item("sheet.xlsx");
        let mut editor =
            FileClient::connect_as(&self.server, DocId(doc.parse().unwrap()), user).await;
        let token = editor.token().unwrap_or_else(|| "no-seat".to_owned());
        let base = self.revision(doc).await;
        let mut request = self
            .server
            .http(Some(user))
            .put(self.server.url(&format!("/api/docs/{doc}/content")))
            .header(SEAT, token)
            .body(b"new sheet".to_vec());
        if let Some(base) = &base {
            request = request.query(&[("base", base.as_str())]);
        }
        let answered = answer(request.send().await.unwrap()).await;
        editor.close().await;
        answered
    }
}

fn bulk(op: &str, item: &str, parent: Option<&String>) -> (Method, String, Value) {
    let mut body = json!({ "op": op, "items": [item] });
    if let Some(parent) = parent {
        body["parent"] = json!(parent);
    }
    (Method::POST, "/api/bulk".to_owned(), body)
}

async fn answer(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, body)
}

fn is_bulk(act: Act) -> bool {
    matches!(
        act,
        Act::BulkMove
            | Act::BulkMoveIn
            | Act::BulkMoveOut
            | Act::BulkCopyIn
            | Act::BulkDelete
            | Act::BulkRestore
    )
}

/// Fails unless `answer` is the refusal (for a bulk request, its one item's).
fn assert_refused(act: Act, who: &str, (status, body): &(StatusCode, Value)) {
    if is_bulk(act) {
        assert_eq!(*status, StatusCode::OK, "{act:?} as {who}: {body}");
        assert_eq!(body[0]["ok"], false, "{act:?} as {who}: {body}");
        assert_eq!(
            body[0]["error"], "source_read_only",
            "{act:?} as {who}: {body}"
        );
    } else {
        assert_eq!(*status, StatusCode::CONFLICT, "{act:?} as {who}: {body}");
        assert_eq!(
            body["error"], "source_read_only",
            "{act:?} as {who}: {body}"
        );
        assert_eq!(body["message"], MESSAGE, "{act:?} as {who}: {body}");
    }
}

/// Every row of [`ROWS`], in a connected library, as its owner and as an administrator:
/// `409 source_read_only` with the message (one item refused for a bulk request), and the
/// library's items, their versions and the recycle bin unchanged after all of them. The sync
/// still changes the library afterwards.
#[tokio::test]
async fn every_content_route_refuses_in_a_connected_library() {
    let env = Env::new().await;
    let fx = env.fixture(true, &[&env.olive, &env.admin]).await;
    let before = env.tree(&fx.library).await;
    let revisions_before: Vec<Option<String>> = {
        let mut out = Vec::new();
        for (_, id) in &before {
            out.push(
                env.server
                    .records_db()
                    .document(DocId(id.parse().unwrap()))
                    .await
                    .unwrap()
                    .and_then(|d| d.revision),
            );
        }
        out
    };
    let outside_before = env.tree(&fx.outside).await;
    let library_id = NodeId(fx.library.parse().unwrap());
    let library_before = env
        .server
        .records_db()
        .node(library_id)
        .await
        .unwrap()
        .unwrap();

    for (user, who) in [(&env.olive, "owner"), (&env.admin, "administrator")] {
        for row in ROWS {
            let answered = env.perform(&fx, user, row.act).await;
            assert_refused(row.act, who, &answered);
        }
    }

    assert_eq!(env.tree(&fx.library).await, before, "the library changed");
    // RenameLibrary and DeleteLibrary left the library itself as it was.
    let library_after = env
        .server
        .records_db()
        .node(library_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        library_after.name, library_before.name,
        "the library was renamed"
    );
    assert!(!library_after.deleted, "the library was deleted");
    assert_eq!(library_after.parent, library_before.parent);
    let mut revisions_after = Vec::new();
    for (_, id) in &before {
        revisions_after.push(
            env.server
                .records_db()
                .document(DocId(id.parse().unwrap()))
                .await
                .unwrap()
                .and_then(|d| d.revision),
        );
    }
    assert_eq!(
        revisions_after, revisions_before,
        "a file has a new version"
    );
    assert_eq!(
        env.tree(&fx.outside).await,
        outside_before,
        "the outside folder changed"
    );
    for name in BINNED {
        let node = env
            .server
            .records_db()
            .node(NodeId(fx.item(name).parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        assert!(node.deleted, "{name} was restored");
    }

    // The sync engine, which calls the same tree functions directly, still changes it.
    env.fake.commit(
        "main",
        &[
            ("docs/read-me.txt", b"changed on the branch".to_vec()),
            ("added.txt", b"new on the branch".to_vec()),
        ],
    );
    env.sync(&fx.library).await;
    let top = env.children(&fx.library).await;
    assert!(top.contains_key("added.txt"), "{top:?}");
    assert!(!top.contains_key("other"), "{top:?}");
}

/// The same complete guarded-route table uses the same refusal for a file-share library. The
/// fixture selection is the only difference, so a route added to the table is exercised for
/// both connected-source kinds without a second copy of the table.
#[tokio::test]
async fn every_write_route_is_refused_in_a_share_library() {
    let env = Env::new().await;
    let fx = env.share_fixture(&[&env.olive, &env.admin]).await;
    for (user, who) in [
        (&env.olive, "share owner"),
        (&env.admin, "share administrator"),
    ] {
        for row in ROWS {
            let answered = env.perform(&fx, user, row.act).await;
            assert_refused(row.act, who, &answered);
        }
    }
}

/// The same rows in ordinary libraries, as an owner and as an administrator (each with a
/// library of their own, since the rows change it): each succeeds.
#[tokio::test]
async fn the_same_routes_work_in_an_ordinary_library() {
    let env = Env::new().await;
    for (user, who) in [(&env.olive, "owner"), (&env.admin, "administrator")] {
        let fx = env.fixture(false, &[user]).await;
        for row in ROWS {
            let (status, body) = env.perform(&fx, user, row.act).await;
            assert_eq!(status.as_u16(), row.ok, "{:?} as {who}: {body}", row.act);
            if is_bulk(row.act) {
                assert_eq!(body[0]["ok"], true, "{:?} as {who}: {body}", row.act);
            }
        }
    }
}

/// A file or folder copied out of a connected library is an ordinary item where it lands: it
/// can be renamed and changed there. The original stays.
#[tokio::test]
async fn copying_out_of_a_connected_library_works() {
    let env = Env::new().await;
    let fx = env.fixture(true, &[]).await;
    let mut copies = Vec::new();
    for (user, item) in [
        (&env.olive, fx.item("read-me.txt")),
        (&env.admin, fx.other.as_str()),
    ] {
        let (status, body) = env
            .call(
                user,
                Method::POST,
                &format!("/api/nodes/{item}/copy"),
                Some(json!({ "parent": fx.outside })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let copy = body["node"]["id"].as_str().unwrap().to_owned();
        assert_eq!(body["node"]["parent"], fx.outside.as_str());
        let (status, body) = env
            .call(
                user,
                Method::PATCH,
                &format!("/api/nodes/{copy}"),
                Some(json!({ "name": format!("copy {}.txt", Uuid::new_v4().simple()) })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        copies.push(copy);
    }
    // The copied folder came with its file.
    assert!(env.children(&copies[1]).await.contains_key("kept.txt"));
    let read_me = &copies[0];
    let response = env
        .server
        .http(Some(&env.olive))
        .get(env.server.url(&format!("/api/docs/{read_me}/content")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().to_vec(),
        bytes_of("read-me.txt")
    );
    // The bulk route copies out too.
    let (status, body) = env
        .call(
            &env.olive,
            Method::POST,
            "/api/bulk",
            Some(json!({ "op": "copy", "items": [fx.item("kept.txt")], "parent": fx.outside })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["ok"], true, "{body}");
    // The originals are still in the library.
    assert!(env.children(&fx.docs).await.contains_key("read-me.txt"));
    assert!(env.children(&fx.other).await.contains_key("kept.txt"));
}

/// Moving an item out of a connected library takes it out of the mirror: refused, alone or in
/// bulk, for the owner and an administrator, and the item stays.
#[tokio::test]
async fn moving_out_is_refused() {
    let env = Env::new().await;
    let fx = env.fixture(true, &[]).await;
    for user in [&env.olive, &env.admin] {
        for item in [fx.item("move-out.txt"), fx.other.as_str()] {
            let answered = env
                .call(
                    user,
                    Method::PATCH,
                    &format!("/api/nodes/{item}"),
                    Some(json!({ "parent": fx.outside })),
                )
                .await;
            assert_refused(Act::MoveOut, &user.email, &answered);
            let answered = env
                .call(
                    user,
                    Method::POST,
                    "/api/bulk",
                    Some(json!({ "op": "move", "items": [item], "parent": fx.outside })),
                )
                .await;
            assert_refused(Act::BulkMoveOut, &user.email, &answered);
        }
    }
    assert!(env.children(&fx.docs).await.contains_key("move-out.txt"));
    assert!(env.children(&fx.library).await.contains_key("other"));
    assert!(
        env.children(&fx.outside)
            .await
            .keys()
            .all(|n| OUTSIDE_FILES.contains(&n.as_str()))
    );
}

/// In a connected library, what reads and what changes access still works: the node, its
/// children, the document, its content and preview, the library's settings, the bin; sharing
/// and unsharing, breaking and restoring inheritance, links, requests for access and their
/// approval.
#[tokio::test]
async fn reading_listing_previewing_and_sharing_still_work() {
    let env = Env::new().await;
    let fx = env.fixture(true, &[]).await;
    let file = fx.item("read-me.txt");
    let olive = &env.olive;
    for path in [
        format!("/api/nodes/{}", fx.library),
        format!("/api/nodes/{}/children", fx.library),
        format!("/api/nodes/{}/children", fx.docs),
        format!("/api/nodes/{file}"),
        format!("/api/docs/{file}"),
        format!("/api/docs/{file}/preview"),
        format!("/api/nodes/{file}/access"),
        format!("/api/libraries/{}", fx.library),
        format!("/api/sites/{}/bin", env.site),
        format!("/api/explorer/{}", fx.library),
    ] {
        let (status, body) = env.call(olive, Method::GET, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let response = env
        .server
        .http(Some(olive))
        .get(env.server.url(&format!("/api/docs/{file}/content")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().to_vec(),
        bytes_of("read-me.txt")
    );

    // Sharing: Bob gets a role on the file, can read it, and loses it again.
    let (status, body) = env
        .call(
            olive,
            Method::PUT,
            &format!("/api/nodes/{file}/access"),
            Some(json!({ "email": env.bob.email, "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = env
        .call(&env.bob, Method::GET, &format!("/api/docs/{file}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(
            olive,
            Method::DELETE,
            &format!("/api/nodes/{file}/access/user/{}", env.bob.id.0),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = env
        .call(&env.bob, Method::GET, &format!("/api/docs/{file}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Inheritance.
    for inherit in [false, true] {
        let (status, body) = env
            .call(
                olive,
                Method::POST,
                &format!("/api/nodes/{}/inheritance", fx.docs),
                Some(json!({ "inherit": inherit })),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }

    // A sharing link, made and revoked.
    let (status, link) = env
        .call(
            olive,
            Method::POST,
            &format!("/api/nodes/{file}/links"),
            Some(json!({ "kind": "signed_in", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{link}");
    let (status, body) = env
        .call(
            olive,
            Method::DELETE,
            &format!("/api/links/{}", link["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Bob asks for access and Olive approves it.
    let (status, body) = env
        .call(
            &env.bob,
            Method::POST,
            &format!("/api/nodes/{file}/requests"),
            Some(json!({ "message": "please" })),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, open) = env.call(olive, Method::GET, "/api/requests", None).await;
    assert_eq!(status, StatusCode::OK, "{open}");
    let request = open
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["node"]["id"] == file)
        .expect("Bob's request is open")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = env
        .call(
            olive,
            Method::POST,
            &format!("/api/requests/{request}/approve"),
            Some(json!({ "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = env
        .call(&env.bob, Method::GET, &format!("/api/docs/{file}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);

    // The library's settings are not its files.
    let (status, body) = env
        .call(
            olive,
            Method::PATCH,
            &format!("/api/libraries/{}", fx.library),
            Some(json!({ "description": "Mirrors the docs repository." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Someone who sees the library but not a folder that keeps its own access learns nothing
    // about that folder's items from a refused change either: 404, as elsewhere.
    let carol = env.server.user("Carol Partial").await;
    let db = env.server.records_db();
    db.set_node_access(
        env.admin.id,
        NodeId(fx.library.parse().unwrap()),
        Principal::User(carol.id),
        Some("editor"),
    )
    .await
    .unwrap();
    let (status, body) = env
        .call(
            &env.admin,
            Method::POST,
            &format!("/api/nodes/{}/inheritance", fx.other),
            Some(json!({ "inherit": false })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    db.set_node_access(
        env.admin.id,
        NodeId(fx.other.parse().unwrap()),
        Principal::User(carol.id),
        None,
    )
    .await
    .unwrap();
    let hidden = fx.item("kept.txt");
    let (status, _) = env
        .call(&carol, Method::GET, &format!("/api/nodes/{hidden}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "Carol cannot see kept.txt");
    for (method, path, body) in [
        (Method::DELETE, format!("/api/nodes/{hidden}"), None),
        (
            Method::PATCH,
            format!("/api/nodes/{hidden}"),
            Some(json!({ "name": "x.txt" })),
        ),
        (
            Method::POST,
            format!("/api/nodes/{}/children", fx.other),
            Some(json!({ "kind": "folder", "name": "x" })),
        ),
    ] {
        let (status, answered) = env.call(&carol, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {answered}");
    }
    // What she can see (a file that still inherits from the library) is refused in words.
    let answered = env
        .call(
            &carol,
            Method::DELETE,
            &format!("/api/nodes/{}", fx.item("version.txt")),
            None,
        )
        .await;
    assert_refused(Act::Delete, "Carol", &answered);

    // Someone who cannot see the library learns nothing from a refused change.
    let (status, body) = env
        .call(
            &env.bob,
            Method::DELETE,
            &format!("/api/nodes/{}", fx.item("kept.txt")),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// An editor opened on a file of a connected library joins with a viewer's seat: `can_edit`
/// false and no token, even for its owner and an administrator, and a save with any token is
/// refused. An access change while it is open (which reads access afresh) keeps it read-only.
#[tokio::test]
async fn an_editor_opens_a_connected_file_read_only() {
    let env = Env::new().await;
    let fx = env.fixture(true, &[]).await;
    let sheet = DocId(fx.item("sheet.xlsx").parse().unwrap());
    let mut editors = Vec::new();
    for user in [&env.olive, &env.admin] {
        let editor = FileClient::connect_as(&env.server, sheet, user).await;
        assert!(!editor.can_edit, "{}", user.email);
        assert!(editor.token().is_none(), "{} got the seat", user.email);
        assert!(
            !matches!(editor.seat, Some(SeatState::Yours { .. })),
            "{:?}",
            editor.seat
        );
        editors.push(editor);
    }
    let base = env.revision(fx.item("sheet.xlsx")).await;
    for user in [&env.olive, &env.admin] {
        let mut request = env
            .server
            .http(Some(user))
            .put(env.server.url(&format!("/api/docs/{}/content", sheet.0)))
            .header(SEAT, "no-seat")
            .body(b"changed".to_vec());
        if let Some(base) = &base {
            request = request.query(&[("base", base.as_str())]);
        }
        let answered = answer(request.send().await.unwrap()).await;
        assert_refused(Act::Save, &user.email, &answered);
    }

    // Bob joins as a viewer, then is made an editor of the file while connected: the room is
    // told his new access, still without editing.
    env.server
        .records_db()
        .set_node_access(
            env.admin.id,
            sheet.into(),
            Principal::User(env.bob.id),
            Some("viewer"),
        )
        .await
        .unwrap();
    let mut bob = FileClient::connect_as(&env.server, sheet, &env.bob).await;
    assert!(!bob.can_edit);
    let (status, body) = env
        .call(
            &env.olive,
            Method::PUT,
            &format!("/api/docs/{}/access", sheet.0),
            Some(json!({ "email": env.bob.email, "role": "editor" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    bob.pump_until(|c| {
        c.received
            .iter()
            .any(|m| matches!(m, FileServerMsg::Access { .. }))
    })
    .await;
    assert!(!bob.can_edit, "an access change gave editing back");
    assert!(bob.token().is_none());
    for mut editor in editors {
        assert!(editor.token().is_none());
        editor.close().await;
    }
    bob.close().await;
}
