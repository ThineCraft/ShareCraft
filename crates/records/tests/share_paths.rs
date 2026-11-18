#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::user;
use records::test_support::TestDb;
use records::{
    AccessError, DEFAULT_SOURCE_MAX_FILES, PathRefusal, SHARE_CACHE_MAX_BYTES, SHARE_LIST_SECONDS,
    SHARE_ROOTS, SOURCE_MAX_FILES, SharePath, approved_root, hidden_by_default, reserved_name,
    valid_root, windows_safe_name,
};
use serde_json::json;

#[test]
fn parses_unc_drive_and_posix_and_displays_them_normalised() {
    let unc = SharePath::parse(r"\\server\share\one\two").unwrap();
    let drive = SharePath::parse(r"c:\one\two").unwrap();
    let posix = SharePath::parse("/mnt/one/two").unwrap();

    assert_eq!(unc.display(), r"\\server\share\one\two");
    assert_eq!(drive.display(), r"C:\one\two");
    assert_eq!(posix.display(), "/mnt/one/two");
    assert_eq!(unc.to_path_buf().to_string_lossy(), unc.display());
    assert_eq!(drive.to_path_buf().to_string_lossy(), drive.display());
    assert_eq!(posix.to_path_buf().to_string_lossy(), posix.display());
}

#[test]
fn forward_slashes_and_trailing_separators_are_normalised() {
    assert_eq!(
        SharePath::parse("//server/share/one\\two///")
            .unwrap()
            .display(),
        r"\\server\share\one\two"
    );
    assert_eq!(
        SharePath::parse("C:/one\\two//").unwrap().display(),
        r"C:\one\two"
    );
    assert_eq!(
        SharePath::parse("/mnt/one\\two//").unwrap().display(),
        "/mnt/one/two"
    );
}

#[test]
fn refuses_relative_empty_and_device_prefixes() {
    assert_eq!(SharePath::parse(""), Err(PathRefusal::Empty));
    assert_eq!(
        SharePath::parse("folder/file"),
        Err(PathRefusal::NotAbsolute)
    );
    assert_eq!(SharePath::parse("C:file"), Err(PathRefusal::NotAbsolute));
    for path in [r"\\?\C:\x", r"\\.\pipe\x", "//?/UNC/s/x", "//./pipe/x"] {
        assert_eq!(SharePath::parse(path), Err(PathRefusal::DevicePrefix));
    }
    assert_eq!(
        SharePath::parse(r"\\server\\share\file"),
        Err(PathRefusal::EmptySegment)
    );
}

#[test]
fn refuses_traversal_anywhere() {
    for path in [r"\\s\sh\a\..\b", r"C:\a\.\b", "/a/../b"] {
        assert_eq!(SharePath::parse(path), Err(PathRefusal::Traversal));
    }
}

#[test]
fn refuses_device_names_in_any_segment_and_case() {
    for path in [r"\\s\sh\con", r"C:\a\NUL.txt", r"C:\a\lpt1"] {
        assert_eq!(SharePath::parse(path), Err(PathRefusal::DeviceName));
    }
}

#[test]
fn refuses_streams_and_trailing_dots_and_spaces() {
    assert_eq!(
        SharePath::parse(r"C:\a\file.txt:secret"),
        Err(PathRefusal::Stream)
    );
    for path in [r"C:\a\file.", r"C:\a\file "] {
        assert_eq!(SharePath::parse(path), Err(PathRefusal::TrailingDotOrSpace));
    }
    assert_eq!(
        SharePath::parse(r"C:\a\bad*name"),
        Err(PathRefusal::BadCharacter)
    );
    let too_long = format!("C:\\{}", "a".repeat(1_000));
    assert_eq!(SharePath::parse(&too_long), Err(PathRefusal::TooLong));
}

#[test]
fn within_is_by_whole_segments_and_ignores_case_for_windows_forms() {
    let unc_root = SharePath::parse(r"\\Server\Share\A").unwrap();
    assert!(
        SharePath::parse(r"\\server\share\a")
            .unwrap()
            .is_within(&unc_root)
    );
    assert!(
        SharePath::parse(r"\\SERVER\SHARE\A\b")
            .unwrap()
            .is_within(&unc_root)
    );
    assert!(
        !SharePath::parse(r"\\server\share\ab")
            .unwrap()
            .is_within(&unc_root)
    );

    let drive_root = SharePath::parse(r"C:\Folder").unwrap();
    assert!(
        SharePath::parse(r"c:\folder\Child")
            .unwrap()
            .is_within(&drive_root)
    );
    assert!(
        !SharePath::parse(r"D:\folder\Child")
            .unwrap()
            .is_within(&drive_root)
    );
}

#[test]
fn drive_containment_uses_single_code_point_windows_case_mapping() {
    let dotted_capital = SharePath::parse("C:\\\u{0130}").unwrap();
    let expanded_lower = SharePath::parse("C:\\i\u{0307}").unwrap();
    assert!(!dotted_capital.is_within(&expanded_lower));
    assert!(!expanded_lower.is_within(&dotted_capital));

    for candidate in ["C:\\\u{03C2}\\file", "C:\\\u{03C3}\\file"] {
        assert!(
            SharePath::parse(candidate)
                .unwrap()
                .is_within(&SharePath::parse("C:\\\u{03A3}").unwrap())
        );
    }
    for root in ["C:\\\u{03C2}", "C:\\\u{03C3}"] {
        assert!(
            SharePath::parse("C:\\\u{03A3}\\file")
                .unwrap()
                .is_within(&SharePath::parse(root).unwrap())
        );
    }
}

#[test]
fn unc_containment_uses_single_code_point_windows_case_mapping() {
    let dotted_capital = SharePath::parse("\\\\server\\share\\\u{0130}").unwrap();
    let expanded_lower = SharePath::parse("\\\\server\\share\\i\u{0307}").unwrap();
    assert!(!dotted_capital.is_within(&expanded_lower));
    assert!(!expanded_lower.is_within(&dotted_capital));

    for candidate in [
        "\\\\server\\share\\\u{03C2}\\file",
        "\\\\server\\share\\\u{03C3}\\file",
    ] {
        assert!(
            SharePath::parse(candidate)
                .unwrap()
                .is_within(&SharePath::parse("\\\\server\\share\\\u{03A3}").unwrap())
        );
    }
    for root in ["\\\\server\\share\\\u{03C2}", "\\\\server\\share\\\u{03C3}"] {
        assert!(
            SharePath::parse("\\\\server\\share\\\u{03A3}\\file")
                .unwrap()
                .is_within(&SharePath::parse(root).unwrap())
        );
    }
}

#[test]
fn windows_case_mapping_still_matches_only_whole_segments() {
    let drive_root = SharePath::parse("C:\\\u{03A3}").unwrap();
    assert!(
        !SharePath::parse("C:\\\u{03C3}uffix")
            .unwrap()
            .is_within(&drive_root)
    );

    let unc_root = SharePath::parse("\\\\server\\share\\\u{03A3}").unwrap();
    assert!(
        !SharePath::parse("\\\\server\\share\\\u{03C2}uffix")
            .unwrap()
            .is_within(&unc_root)
    );
}

#[test]
fn posix_within_is_case_sensitive() {
    let root = SharePath::parse("/mnt/Share").unwrap();
    assert!(SharePath::parse("/mnt/Share/a").unwrap().is_within(&root));
    assert!(!SharePath::parse("/mnt/share/a").unwrap().is_within(&root));
}

#[test]
fn join_refuses_a_relative_path_that_climbs_or_is_absolute() {
    let root = SharePath::parse(r"C:\root").unwrap();
    for relative in ["../x", "a/../../x", "/x", r"a\..\x", "C:x", r"C:\x"] {
        assert!(root.join(relative).is_err(), "accepted {relative:?}");
    }
    assert_eq!(root.join("a/b").unwrap().display(), r"C:\root\a\b");
}

#[test]
fn an_admin_share_is_approved_only_below_a_root_on_that_share() {
    let root = SharePath::parse(r"\\server\C$\approved").unwrap();
    let roots = [root];
    let below = SharePath::parse(r"\\SERVER\c$\approved\folder").unwrap();
    let beside = SharePath::parse(r"\\server\C$\other").unwrap();
    let bare = SharePath::parse(r"\\server\C$").unwrap();

    assert!(below.is_admin_share());
    assert_eq!(approved_root(&below, &roots), roots.first());
    assert_eq!(approved_root(&beside, &roots), None);
    assert_eq!(approved_root(&bare, &roots), None);
}

#[test]
fn a_whole_drive_or_bare_admin_share_is_not_a_valid_root() {
    assert_eq!(
        valid_root(&SharePath::parse(r"C:\").unwrap()),
        Err(PathRefusal::WholeDrive)
    );
    assert_eq!(
        valid_root(&SharePath::parse(r"\\server\C$").unwrap()),
        Err(PathRefusal::WholeDrive)
    );
    assert!(valid_root(&SharePath::parse(r"\\server\share").unwrap()).is_ok());
    assert!(valid_root(&SharePath::parse(r"\\server\C$\folder").unwrap()).is_ok());
}

#[test]
fn nothing_is_approved_when_there_are_no_roots() {
    assert_eq!(
        approved_root(&SharePath::parse(r"C:\folder").unwrap(), &[]),
        None
    );
}

#[test]
fn windows_safe_name_and_hidden_by_default_cases() {
    for name in [
        "CON",
        "nul.txt",
        "Lpt9.log",
        "trailing.",
        "trailing ",
        "bad:name",
        "a/b",
    ] {
        assert!(!windows_safe_name(name), "accepted {name:?}");
    }
    assert!(windows_safe_name("quarterly report.txt"));

    for name in [
        "Thumbs.db",
        "THUMBS.DB",
        "desktop.ini",
        ".DS_Store",
        "~$report.docx",
    ] {
        assert!(hidden_by_default(name), "did not hide {name:?}");
    }
    assert!(!hidden_by_default("report.docx"));
}

#[test]
fn reserved_names_are_recognised_and_refused_in_paths() {
    for name in [
        ".sharecraft-bin",
        ".SHARECRAFT-BIN",
        ".sharecraft-tmp-upload",
    ] {
        assert!(reserved_name(name));
        assert_eq!(
            SharePath::parse(&format!(r"C:\root\{name}")),
            Err(PathRefusal::BadCharacter)
        );
    }
    assert!(!reserved_name(".sharecraft-temp"));
    let root = SharePath::parse(r"C:\root").unwrap();
    assert_eq!(
        root.join(".sharecraft-tmp-copy"),
        Err(PathRefusal::BadCharacter)
    );
}

#[tokio::test]
async fn share_roots_setting_refuses_bad_roots_and_non_administrators() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let member = user(&test, "Member", false).await;

    assert_eq!(
        test.db
            .set_setting(member.id, SHARE_ROOTS, json!([r"C:\approved"]))
            .await,
        Err(AccessError::Forbidden)
    );
    for bad in [
        json!(r"C:\approved"),
        json!([r"C:\"]),
        json!([r"\\server\C$"]),
        json!(["relative"]),
        json!([1]),
        json!((0..21).map(|n| format!(r"C:\root{n}")).collect::<Vec<_>>()),
    ] {
        assert_eq!(
            test.db.set_setting(admin.id, SHARE_ROOTS, bad).await,
            Err(AccessError::InvalidSetting)
        );
    }
}

#[tokio::test]
async fn share_cache_and_listing_settings_enforce_exact_bounds_and_types() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;

    for bad in [json!(268_435_455), json!("268435456"), json!(-1)] {
        assert_eq!(
            test.db
                .set_setting(admin.id, SHARE_CACHE_MAX_BYTES, bad)
                .await,
            Err(AccessError::InvalidSetting)
        );
    }
    test.db
        .set_setting(admin.id, SHARE_CACHE_MAX_BYTES, json!(268_435_456))
        .await
        .unwrap();

    for bad in [json!(29), json!(3_601), json!("30")] {
        assert_eq!(
            test.db.set_setting(admin.id, SHARE_LIST_SECONDS, bad).await,
            Err(AccessError::InvalidSetting)
        );
    }
    for valid in [30, 3_600] {
        test.db
            .set_setting(admin.id, SHARE_LIST_SECONDS, json!(valid))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn share_settings_have_defaults_and_read_back() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let defaults = test.db.share_settings().await.unwrap();
    assert!(defaults.roots.is_empty());
    assert_eq!(defaults.cache_max_bytes, 10 * 1024 * 1024 * 1024);
    assert_eq!(defaults.list_seconds, 600);
    assert_eq!(defaults.max_files, DEFAULT_SOURCE_MAX_FILES as usize);

    test.db
        .set_setting(
            admin.id,
            SHARE_ROOTS,
            json!([r"C:\approved", r"\\server\share\team"]),
        )
        .await
        .unwrap();
    test.db
        .set_setting(
            admin.id,
            SHARE_CACHE_MAX_BYTES,
            json!(512 * 1024 * 1024_u64),
        )
        .await
        .unwrap();
    test.db
        .set_setting(admin.id, SHARE_LIST_SECONDS, json!(120))
        .await
        .unwrap();
    test.db
        .set_setting(admin.id, SOURCE_MAX_FILES, json!(1_234))
        .await
        .unwrap();

    let settings = test.db.share_settings().await.unwrap();
    assert_eq!(
        settings
            .roots
            .iter()
            .map(SharePath::display)
            .collect::<Vec<_>>(),
        [r"C:\approved", r"\\server\share\team"]
    );
    assert_eq!(settings.cache_max_bytes, 512 * 1024 * 1024);
    assert_eq!(settings.list_seconds, 120);
    assert_eq!(settings.max_files, 1_234);

    sqlx::query("update settings set value = $1 where key = $2")
        .bind(json!([r"C:\approved", "relative", r"C:\also-approved"]))
        .bind(SHARE_ROOTS)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let settings = test.db.share_settings().await.unwrap();
    assert_eq!(
        settings
            .roots
            .iter()
            .map(SharePath::display)
            .collect::<Vec<_>>(),
        [r"C:\approved", r"C:\also-approved"]
    );
}
