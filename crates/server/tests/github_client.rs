//! The GitHub API client against a fake GitHub.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::fake_github::{FakeEntry, FakeGithub};
use common::start_fake_github;
use server::github::{
    EntryKind, GithubClient, GithubError, Head, RepoRef, TarLimits, TreeEntry, for_each_in_tarball,
    git_blob_id,
};
use server::secrets::Secret;
use time::OffsetDateTime;

const TOKEN: &str = "ghp_secret123";

/// Limits no well-formed test tarball comes near.
const ROOMY: TarLimits = TarLimits {
    max_entry_bytes: 1 << 20,
    max_total_bytes: 10 << 20,
    max_entries: 1000,
};

fn octo() -> RepoRef {
    RepoRef::parse("octo/docs").unwrap()
}

async fn fake() -> FakeGithub {
    start_fake_github("octo", "docs").await
}

fn client(fake: &FakeGithub) -> GithubClient {
    GithubClient::new(&fake.origin()).unwrap()
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("README.md", b"# Docs\n".to_vec()),
        ("guide/intro.md", b"Hello\n".to_vec()),
        ("guide/deep/more.md", vec![b'x'; 3000]),
    ]
}

fn entry<'a>(entries: &'a [TreeEntry], path: &str) -> &'a TreeEntry {
    entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("no entry {path} in {entries:?}"))
}

#[tokio::test]
async fn repo_reads_the_basics() {
    let fake = fake().await;
    fake.commit("main", &files());
    let info = client(&fake).repo(&octo(), None).await.unwrap();
    assert!(!info.private);
    assert!(!info.archived);
    assert_eq!(info.default_branch, "main");
    assert_eq!(info.size_kb, fake.size_kb());
    assert_eq!(fake.count("repo"), 1);
    // The fixed headers, and no Authorization without a token.
    let headers = fake.last_headers();
    assert_eq!(headers["user-agent"], "ThineCraft-ShareCraft");
    assert_eq!(headers["x-github-api-version"], "2022-11-28");
    assert!(headers.get("authorization").is_none());
}

#[tokio::test]
async fn head_returns_commit_tree_and_etag_then_not_modified_with_that_etag() {
    let fake = fake().await;
    let first = fake.commit("main", &files());
    let github = client(&fake);
    let Head::Moved { commit, tree, etag } =
        github.head(&octo(), "main", None, None).await.unwrap()
    else {
        panic!("expected a commit");
    };
    assert_eq!(commit, first.sha);
    assert_eq!(tree, first.tree);
    let etag = etag.expect("an etag");
    assert_eq!(
        github
            .head(&octo(), "main", None, Some(&etag))
            .await
            .unwrap(),
        Head::NotModified
    );
    assert_eq!(fake.count("commits"), 2);
    assert_eq!(fake.count("commits_not_modified"), 1);

    // A new commit is seen through the old ETag.
    let second = fake.commit("main", &[("README.md", b"changed".to_vec())]);
    let moved = github
        .head(&octo(), "main", None, Some(&etag))
        .await
        .unwrap();
    assert!(matches!(moved, Head::Moved { commit, .. } if commit == second.sha));

    // A branch with a slash in its name; a deleted branch is not found.
    let feature = fake.commit("feature/x", &files());
    let moved = github.head(&octo(), "feature/x", None, None).await.unwrap();
    assert!(matches!(moved, Head::Moved { commit, .. } if commit == feature.sha));
    fake.delete_branch("feature/x");
    let gone = github.head(&octo(), "feature/x", None, None).await;
    assert_eq!(gone, Err(GithubError::NotFound));
}

#[tokio::test]
async fn tree_lists_files_dirs_symlinks_and_submodules_with_sizes() {
    let fake = fake().await;
    let pinned = "0123456789abcdef0123456789abcdef01234567";
    let commit = fake.commit_entries(
        "main",
        vec![
            FakeEntry::File("README.md".into(), b"hi".to_vec()),
            FakeEntry::File("docs/guide.md".into(), b"guide!".to_vec()),
            FakeEntry::Symlink("latest".into(), "docs/guide.md".into()),
            FakeEntry::Submodule("vendor/lib".into(), pinned.into()),
        ],
    );
    let entries = client(&fake)
        .tree(&octo(), &commit.tree, None)
        .await
        .unwrap();
    assert_eq!(entries.len(), 6, "{entries:?}");
    let readme = entry(&entries, "README.md");
    assert_eq!((readme.kind, readme.size), (EntryKind::File, 2));
    assert_eq!(readme.sha, git_blob_id(b"hi"));
    let guide = entry(&entries, "docs/guide.md");
    assert_eq!((guide.kind, guide.size), (EntryKind::File, 6));
    assert_eq!(entry(&entries, "docs").kind, EntryKind::Dir);
    assert_eq!(entry(&entries, "vendor").kind, EntryKind::Dir);
    let link = entry(&entries, "latest");
    assert_eq!(link.kind, EntryKind::Symlink);
    let module = entry(&entries, "vendor/lib");
    assert_eq!(
        (module.kind, module.sha.as_str()),
        (EntryKind::Submodule, pinned)
    );
    assert_eq!(module.size, 0);
}

#[tokio::test]
async fn a_truncated_tree_is_an_error_not_a_short_list() {
    let fake = fake().await;
    let commit = fake.commit("main", &files());
    fake.force_truncate_trees();
    let result = client(&fake).tree(&octo(), &commit.tree, None).await;
    assert_eq!(result, Err(GithubError::Truncated));
}

#[tokio::test]
async fn blob_returns_raw_bytes_and_git_blob_id_matches_the_tree_sha() {
    // Git's own ids for these contents.
    assert_eq!(git_blob_id(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_eq!(
        git_blob_id(b"hello\n"),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );

    let fake = fake().await;
    let commit = fake.commit("main", &files());
    let github = client(&fake);
    let entries = github.tree(&octo(), &commit.tree, None).await.unwrap();
    for (path, bytes) in files() {
        let found = entry(&entries, path);
        let blob = github
            .blob(&octo(), &found.sha, None, 1 << 20)
            .await
            .unwrap();
        assert_eq!(blob, bytes, "{path}");
        assert_eq!(git_blob_id(&blob), found.sha, "{path}");
    }
}

#[tokio::test]
async fn blob_over_the_limit_is_too_large_without_reading_it_all() {
    let fake = fake().await;
    let big = vec![7u8; 64 * 1024];
    let commit = fake.commit("main", &[("big.bin", big.clone())]);
    let github = client(&fake);
    let entries = github.tree(&octo(), &commit.tree, None).await.unwrap();
    let sha = entry(&entries, "big.bin").sha.clone();

    // Refused on its declared length.
    let result = github.blob(&octo(), &sha, None, 1024).await;
    assert_eq!(result, Err(GithubError::TooLarge));
    // At exactly the limit it is read.
    let exact = github.blob(&octo(), &sha, None, big.len() as u64).await;
    assert_eq!(exact.unwrap().len(), big.len());

    // A body that never ends is cut off once it passes the limit, not read to its end.
    fake.hold_open_blob_bodies();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        github.blob(&octo(), &sha, None, 1024),
    )
    .await
    .expect("the client read on past the limit");
    assert_eq!(result, Err(GithubError::TooLarge));
}

#[tokio::test]
async fn tarball_streams_to_a_file_and_for_each_in_tarball_strips_the_top_folder() {
    let fake = fake().await;
    let mut entries: Vec<FakeEntry> = files()
        .into_iter()
        .map(|(path, bytes)| FakeEntry::File(path.into(), bytes))
        .collect();
    entries.push(FakeEntry::Symlink("latest".into(), "README.md".into()));
    entries.push(FakeEntry::Submodule(
        "vendor/lib".into(),
        "0123456789abcdef0123456789abcdef01234567".into(),
    ));
    let commit = fake.commit_entries("main", entries);
    let github = client(&fake);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("repo.tar.gz");
    github
        .tarball(&octo(), &commit.sha, None, &dest, 10 << 20)
        .await
        .unwrap();
    assert_eq!(fake.count("tarball"), 1);
    assert_eq!(fake.count("codeload"), 1);

    let mut seen = BTreeMap::new();
    for_each_in_tarball(&dest, &ROOMY, &mut |path, reader, size| {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        assert_eq!(bytes.len() as u64, size, "{path}");
        seen.insert(path.to_owned(), bytes);
        Ok(())
    })
    .unwrap();
    let expected: BTreeMap<String, Vec<u8>> = files()
        .into_iter()
        .map(|(path, bytes)| (path.to_owned(), bytes))
        .collect();
    // Regular files only: the symbolic link, folders and the submodule are left out.
    assert_eq!(seen, expected);

    // A download over the limit is refused and leaves no file.
    let small = dir.path().join("small.tar.gz");
    let result = github.tarball(&octo(), &commit.sha, None, &small, 64).await;
    assert_eq!(result, Err(GithubError::TooLarge));
    assert!(!small.exists());
}

#[tokio::test]
async fn private_repo_without_token_is_not_found_and_with_token_is_found() {
    let fake = fake().await;
    fake.commit("main", &files());
    fake.require_token("ghp_good");
    let github = client(&fake);
    let missing = github.repo(&octo(), None).await.unwrap_err();
    assert_eq!(missing, GithubError::NotFound);
    assert!(
        missing
            .to_string()
            .contains("not found, or the token cannot see it"),
        "{missing}"
    );
    let good = Secret::new("ghp_good");
    let info = github.repo(&octo(), Some(&good)).await.unwrap();
    assert!(info.private);
    assert_eq!(fake.last_headers()["authorization"], "Bearer ghp_good");
}

#[tokio::test]
async fn revoked_token_is_unauthorized() {
    let fake = fake().await;
    fake.commit("main", &files());
    fake.require_token("ghp_good");
    let revoked = Secret::new("ghp_revoked");
    let result = client(&fake).repo(&octo(), Some(&revoked)).await;
    assert_eq!(result, Err(GithubError::Unauthorized));
}

#[tokio::test]
async fn rate_limited_carries_the_reset_time() {
    let fake = fake().await;
    fake.commit("main", &files());
    let github = client(&fake);
    let reset =
        OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp() + 3600)
            .unwrap();
    fake.rate_limit_next(reset);
    let result = github.head(&octo(), "main", None, None).await;
    assert_eq!(result, Err(GithubError::RateLimited { retry_at: reset }));

    // A secondary limit says how long to wait instead.
    fake.retry_after_next(120);
    let before = OffsetDateTime::now_utc();
    let Err(GithubError::RateLimited { retry_at }) = github.repo(&octo(), None).await else {
        panic!("expected a rate limit");
    };
    let wait = retry_at - before;
    assert!(
        wait >= time::Duration::seconds(119) && wait <= time::Duration::seconds(122),
        "{wait}"
    );
}

#[tokio::test]
async fn token_never_appears_in_any_error_text() {
    let fake = fake().await;
    let commit = fake.commit("main", &[("big.bin", vec![1u8; 4096])]);
    let token = Secret::new(TOKEN);
    let github = client(&fake);
    let mut errors = Vec::new();

    // NotFound: a repository the token cannot see.
    let missing = RepoRef::parse("octo/missing").unwrap();
    errors.push(github.repo(&missing, Some(&token)).await.unwrap_err());
    assert_eq!(
        fake.last_headers()["authorization"],
        format!("Bearer {TOKEN}")
    );
    // RateLimited.
    fake.rate_limit_next(OffsetDateTime::now_utc() + time::Duration::hours(1));
    errors.push(
        github
            .head(&octo(), "main", Some(&token), None)
            .await
            .unwrap_err(),
    );
    // Other: a server error whose body echoes the token.
    fake.fail_next(500);
    errors.push(github.repo(&octo(), Some(&token)).await.unwrap_err());
    // TooLarge.
    let entries = github
        .tree(&octo(), &commit.tree, Some(&token))
        .await
        .unwrap();
    let sha = entry(&entries, "big.bin").sha.clone();
    errors.push(
        github
            .blob(&octo(), &sha, Some(&token), 10)
            .await
            .unwrap_err(),
    );
    // Truncated.
    fake.force_truncate_trees();
    errors.push(
        github
            .tree(&octo(), &commit.tree, Some(&token))
            .await
            .unwrap_err(),
    );
    // Other: a redirect to a host that is not allowed.
    let elsewhere = start_fake_github("octo", "docs").await;
    fake.redirect_tarballs_to(&elsewhere.origin());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("t.tar.gz");
    errors.push(
        github
            .tarball(&octo(), &commit.sha, Some(&token), &dest, 1 << 20)
            .await
            .unwrap_err(),
    );
    // Unauthorized: the token is refused.
    fake.require_token("ghp_other");
    errors.push(github.repo(&octo(), Some(&token)).await.unwrap_err());
    // Network: nothing listening.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let nowhere = GithubClient::new(&format!("http://{closed}")).unwrap();
    errors.push(nowhere.repo(&octo(), Some(&token)).await.unwrap_err());

    let kinds: Vec<&str> = errors
        .iter()
        .map(|e| match e {
            GithubError::NotFound => "not found",
            GithubError::Unauthorized => "unauthorized",
            GithubError::RateLimited { .. } => "rate limited",
            GithubError::Truncated => "truncated",
            GithubError::TooLarge => "too large",
            GithubError::Network(_) => "network",
            GithubError::Other(_) => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "not found",
            "rate limited",
            "other",
            "too large",
            "truncated",
            "other",
            "unauthorized",
            "network"
        ]
    );
    for err in &errors {
        let text = format!("{err:?}{err}");
        assert!(!text.contains(TOKEN), "the token leaked: {text}");
        assert!(!text.contains("secret123"), "the token leaked: {text}");
    }
}

#[tokio::test]
async fn a_redirect_to_another_host_is_refused() {
    let fake = fake().await;
    let commit = fake.commit("main", &files());
    let elsewhere = start_fake_github("octo", "docs").await;
    elsewhere.commit("main", &files());
    fake.redirect_tarballs_to(&elsewhere.origin());
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("repo.tar.gz");
    let result = client(&fake)
        .tarball(&octo(), &commit.sha, None, &dest, 10 << 20)
        .await;
    let Err(GithubError::Other(message)) = result else {
        panic!("expected a refused redirect, got {result:?}");
    };
    assert!(message.contains("redirect"), "{message}");
    assert_eq!(fake.count("tarball"), 1);
    assert_eq!(elsewhere.count("codeload"), 0);
    assert!(!dest.exists());
}

#[tokio::test]
async fn repo_ref_rejects_paths_traversal_and_extra_slashes() {
    let ok = RepoRef::parse("octo-org/my.docs_v2").unwrap();
    assert_eq!(
        (ok.owner.as_str(), ok.repo.as_str()),
        ("octo-org", "my.docs_v2")
    );
    for bad in [
        "",
        "octo",
        "octo/",
        "/docs",
        "octo/docs/",
        "octo/docs/extra",
        "octo//docs",
        "../docs",
        "octo/..",
        "octo/.",
        "./docs",
        "octo/do cs",
        "octo/docs?x=1",
        "octo/docs#x",
        "oc%74o/docs",
        "octo\\docs",
        "-octo/docs",
        "octo-/docs",
        "https://github.com/octo/docs",
        " octo/docs",
    ] {
        assert!(RepoRef::parse(bad).is_err(), "{bad:?} was accepted");
    }
}

/// A gzipped tarball at a new path in `dir`, holding regular files with exactly these raw
/// names (written into the header by hand, so names `tar` itself refuses can be made).
fn raw_tarball(dir: &std::path::Path, entries: &[(&[u8], &[u8])]) -> std::path::PathBuf {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(gz);
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    let bytes = builder.into_inner().unwrap().finish().unwrap();
    let path = dir.join(format!("t{}.tar.gz", uuid::Uuid::new_v4().simple()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Reads `path` with `limits`, returning the paths given to the callback and the result.
fn read_all(path: &std::path::Path, limits: &TarLimits) -> (Vec<String>, std::io::Result<()>) {
    let mut seen = Vec::new();
    let result = for_each_in_tarball(path, limits, &mut |name, reader, _| {
        std::io::copy(reader, &mut std::io::sink())?;
        seen.push(name.to_owned());
        Ok(())
    });
    (seen, result)
}

#[test]
fn tarball_paths_must_be_plain_repository_paths() {
    let dir = tempfile::tempdir().unwrap();
    let good = raw_tarball(dir.path(), &[(b"top/a.md", b"a"), (b"top/sub/b.md", b"b")]);
    let (seen, result) = read_all(&good, &ROOMY);
    result.unwrap();
    assert_eq!(seen, ["a.md", "sub/b.md"]);

    for bad in [
        &b"top/../x"[..],
        b"top/a\\b",
        b"top/C:/x",
        b"top/x:stream",
        b"/etc/x",
        b"/top/x",
        b"top//x",
        b"top/./x",
        b"top/x/",
        b"other/x",
        b"C:/x",
        b"\\\\server\\share\\x",
    ] {
        let path = raw_tarball(
            dir.path(),
            &[
                (b"top/first.md", b"1"),
                (bad, b"bad"),
                (b"top/later.md", b"2"),
            ],
        );
        let (seen, result) = read_all(&path, &ROOMY);
        let shown = String::from_utf8_lossy(bad);
        let err = result.expect_err(&format!("{shown} was accepted"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{shown}");
        assert_eq!(seen, ["first.md"], "{shown}");
    }
    // Reading writes nothing: only the tarballs are in the folder.
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| n.ends_with(".tar.gz")), "{names:?}");
}

#[test]
fn tarball_limits_stop_expansion_before_the_callback() {
    let dir = tempfile::tempdir().unwrap();
    let check = |entries: &[(&[u8], &[u8])], limits: TarLimits, expected: &[&str]| {
        let path = raw_tarball(dir.path(), entries);
        let (seen, result) = read_all(&path, &limits);
        let err = result.expect_err("over the limit was accepted");
        assert_eq!(err.kind(), std::io::ErrorKind::FileTooLarge);
        let text = err.to_string();
        assert!(!text.contains("big") && !text.contains("top/"), "{text}");
        assert_eq!(seen, expected);
    };
    let small = vec![b's'; 60];
    let big = vec![b'b'; 1000];
    // One entry over the per-entry cap.
    check(
        &[
            (b"top/first.md", &small),
            (b"top/big.bin", &big),
            (b"top/later.md", &small),
        ],
        TarLimits {
            max_entry_bytes: 100,
            ..ROOMY
        },
        &["first.md"],
    );
    // The total passes its cap on the second entry.
    check(
        &[
            (b"top/first.md", &small),
            (b"top/second.md", &small),
            (b"top/later.md", &small),
        ],
        TarLimits {
            max_total_bytes: 100,
            ..ROOMY
        },
        &["first.md"],
    );
    // Too many entries.
    check(
        &[
            (b"top/first.md", &small),
            (b"top/second.md", &small),
            (b"top/later.md", &small),
        ],
        TarLimits {
            max_entries: 2,
            ..ROOMY
        },
        &["first.md", "second.md"],
    );
}

#[tokio::test]
async fn huge_rate_limit_waits_are_capped_without_overflow() {
    let fake = fake().await;
    fake.commit("main", &files());
    let github = client(&fake);

    fake.retry_after_next(u64::MAX);
    let before = OffsetDateTime::now_utc();
    let Err(GithubError::RateLimited { retry_at }) = github.repo(&octo(), None).await else {
        panic!("expected a rate limit");
    };
    let wait = retry_at - before;
    assert!(
        wait >= time::Duration::seconds(3599) && wait <= time::Duration::seconds(3602),
        "{wait}"
    );

    for epoch in [i64::MAX, 253_402_300_800 /* year 10000 */] {
        fake.rate_limit_next_epoch(epoch);
        let before = OffsetDateTime::now_utc();
        let Err(GithubError::RateLimited { retry_at }) = github.repo(&octo(), None).await else {
            panic!("expected a rate limit");
        };
        let wait = retry_at - before;
        assert!(
            wait >= time::Duration::hours(24) - time::Duration::seconds(1)
                && wait <= time::Duration::hours(24) + time::Duration::seconds(2),
            "{epoch}: {wait}"
        );
    }
}

#[tokio::test]
async fn a_bare_403_is_a_rate_limit_only_when_it_says_so() {
    let fake = fake().await;
    fake.commit("main", &files());
    let github = client(&fake);

    fake.forbid_next("You have exceeded a secondary rate limit. Please wait a few minutes.");
    let before = OffsetDateTime::now_utc();
    let Err(GithubError::RateLimited { retry_at }) = github.repo(&octo(), None).await else {
        panic!("expected a rate limit");
    };
    let wait = retry_at - before;
    assert!(
        wait >= time::Duration::seconds(59) && wait <= time::Duration::seconds(62),
        "{wait}"
    );

    fake.forbid_next("You have triggered an abuse detection mechanism.");
    assert!(matches!(
        github.repo(&octo(), None).await,
        Err(GithubError::RateLimited { .. })
    ));

    fake.forbid_next("Resource not accessible by personal access token");
    let err = github.repo(&octo(), None).await.unwrap_err();
    assert_eq!(err, GithubError::Unauthorized);
    let text = format!("{err:?}{err}");
    assert!(!text.contains("Resource not accessible"), "{text}");
}

/// What an extension header written by [`extension_tarball`] holds.
enum Payload<'a> {
    Bytes(&'a [u8]),
    /// This many zero bytes, written in pieces (they compress to almost nothing).
    Zeros(u64),
}

/// A gzipped tarball at a new path in `dir`: one extension header of `kind` (a GNU long name,
/// or a pax local header) carrying `payload`, then the regular file `top/after.md`.
fn extension_tarball(
    dir: &std::path::Path,
    kind: tar::EntryType,
    payload: Payload<'_>,
) -> std::path::PathBuf {
    use std::io::Write;
    let path = dir.join(format!("x{}.tar.gz", uuid::Uuid::new_v4().simple()));
    let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let mut gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
    let block = |gz: &mut dyn Write, name: &[u8], kind: tar::EntryType, size: u64| {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(size);
        header.set_cksum();
        gz.write_all(header.as_bytes()).unwrap();
    };
    let pad = |gz: &mut dyn Write, size: u64| {
        let rest = (512 - size % 512) % 512;
        gz.write_all(&vec![0u8; rest as usize]).unwrap();
    };
    let size = match payload {
        Payload::Bytes(bytes) => bytes.len() as u64,
        Payload::Zeros(n) => n,
    };
    block(&mut gz, b"././@LongLink", kind, size);
    match payload {
        Payload::Bytes(bytes) => gz.write_all(bytes).unwrap(),
        Payload::Zeros(n) => {
            let piece = vec![0u8; 1 << 20];
            let mut left = n;
            while left > 0 {
                let now = left.min(piece.len() as u64);
                gz.write_all(&piece[..now as usize]).unwrap();
                left -= now;
            }
        }
    }
    pad(&mut gz, size);
    block(&mut gz, b"top/after.md", tar::EntryType::Regular, 1);
    gz.write_all(b"x").unwrap();
    pad(&mut gz, 1);
    gz.write_all(&[0u8; 1024]).unwrap();
    gz.finish().unwrap().flush().unwrap();
    path
}

#[test]
fn a_huge_extension_header_is_refused_before_it_is_read_into_memory() {
    let dir = tempfile::tempdir().unwrap();
    let limits = TarLimits {
        max_entry_bytes: 1 << 20,
        max_total_bytes: 1 << 20,
        max_entries: 100,
    };
    for kind in [tar::EntryType::GNULongName, tar::EntryType::XHeader] {
        // 256 MiB of zeros declared in one header; a few hundred KiB once compressed.
        let path = extension_tarball(dir.path(), kind, Payload::Zeros(256 << 20));
        assert!(std::fs::metadata(&path).unwrap().len() < 2 << 20);
        let started = std::time::Instant::now();
        let (seen, result) = read_all(&path, &limits);
        let err = result.expect_err("a huge extension header was accepted");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::FileTooLarge,
            "{kind:?}: {err}"
        );
        assert_eq!(
            err.to_string(),
            "the tarball expands to more than the limit allows",
            "{kind:?}"
        );
        assert!(seen.is_empty(), "{kind:?}: {seen:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{kind:?} took {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn control_characters_in_tarball_paths_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        &b"top/a\x01b"[..],
        b"top/a\x1fb",
        b"top/a\x7fb",
        b"top/a\tb",
    ] {
        let path = raw_tarball(
            dir.path(),
            &[
                (b"top/first.md", b"1"),
                (bad, b"bad"),
                (b"top/later.md", b"2"),
            ],
        );
        let (seen, result) = read_all(&path, &ROOMY);
        let shown = String::from_utf8_lossy(bad);
        let err = result.expect_err(&format!("{shown:?} was accepted"));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{shown:?}");
        assert_eq!(seen, ["first.md"], "{shown:?}");
    }
    // NUL cannot end a short name early, but a GNU long name can carry one inside it.
    let path = extension_tarball(
        dir.path(),
        tar::EntryType::GNULongName,
        Payload::Bytes(b"top/a\0b.md\0"),
    );
    let (seen, result) = read_all(&path, &ROOMY);
    let err = result.expect_err("a NUL in a path was accepted");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(seen.is_empty(), "{seen:?}");
}
