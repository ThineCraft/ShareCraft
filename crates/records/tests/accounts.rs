#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use collab_core::ids::UserId;
use records::test_support::TestDb;
use records::{AccountError, NewUser, SESSION_LIFETIME, SessionToken, SignInError, User};
use sqlx::Row;
use time::OffsetDateTime;

const PASSWORD: &str = "correct horse battery";

fn new_user<'a>(email: &'a str, name: &'a str, is_admin: bool) -> NewUser<'a> {
    NewUser {
        email,
        name,
        password: PASSWORD,
        is_admin,
    }
}

async fn admin(test: &TestDb) -> User {
    test.db
        .create_user(None, new_user("root@x.test", "Root", true))
        .await
        .unwrap()
}

#[tokio::test]
async fn created_user_can_sign_in() {
    let test = TestDb::create().await;
    let before = OffsetDateTime::now_utc();
    let ann = test
        .db
        .create_user(None, new_user("  ann@x.test ", " Ann ", false))
        .await
        .unwrap();
    assert_eq!(ann.email, "ann@x.test");
    assert_eq!(ann.name, "Ann");
    assert!(!ann.is_admin && !ann.disabled);
    assert_eq!(test.db.user(ann.id).await.unwrap(), Some(ann.clone()));
    assert_eq!(
        test.db.user_by_email("ANN@x.test").await.unwrap(),
        Some(ann.clone())
    );

    let (session, token) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    assert_eq!(session.user, ann);
    assert!(session.expires_at > before + SESSION_LIFETIME - time::Duration::minutes(1));
    assert!(session.expires_at < OffsetDateTime::now_utc() + SESSION_LIFETIME);
    let found = test.db.session(&token).await.unwrap().unwrap();
    assert_eq!(found.id, session.id);
    assert_eq!(found.user, ann);
}

#[tokio::test]
async fn email_is_unique_ignoring_case() {
    let test = TestDb::create().await;
    test.db
        .create_user(None, new_user("Ann@x.test", "Ann", false))
        .await
        .unwrap();
    let again = test
        .db
        .create_user(None, new_user("ann@X.test", "Other", false))
        .await;
    assert!(matches!(again, Err(AccountError::EmailTaken)));
    assert_eq!(test.db.users().await.unwrap().len(), 1);
}

#[tokio::test]
async fn hostile_names_and_emails_are_refused() {
    let test = TestDb::create().await;
    let long_name = "n".repeat(61);
    for name in ["", "<b>", "a\u{0}b", ";:,", long_name.as_str()] {
        let result = test
            .db
            .create_user(None, new_user("ok@x.test", name, false))
            .await;
        assert!(
            matches!(result, Err(AccountError::InvalidName)),
            "name {name:?}: {result:?}"
        );
    }
    let long_email = format!("{}@x.test", "e".repeat(300));
    for email in [
        "a",
        "a@",
        "a b@x.test",
        "@x.test",
        "a@b@c",
        long_email.as_str(),
    ] {
        let result = test
            .db
            .create_user(None, new_user(email, "Ok", false))
            .await;
        assert!(
            matches!(result, Err(AccountError::InvalidEmail)),
            "email {email:?}: {result:?}"
        );
    }
    assert!(test.db.users().await.unwrap().is_empty());
}

#[tokio::test]
async fn short_password_is_weak() {
    let test = TestDb::create().await;
    let long = "p".repeat(257);
    for password in ["", "123456789", long.as_str()] {
        let result = test
            .db
            .create_user(
                None,
                NewUser {
                    email: "a@x.test",
                    name: "A",
                    password,
                    is_admin: false,
                },
            )
            .await;
        assert!(matches!(result, Err(AccountError::WeakPassword)));
    }
}

#[tokio::test]
async fn wrong_password_unknown_email_and_disabled_account_are_all_invalid() {
    let test = TestDb::create().await;
    let root = admin(&test).await;
    let ann = test
        .db
        .create_user(Some(root.id), new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let wrong = test.db.sign_in("ann@x.test", "not the password").await;
    assert!(matches!(wrong, Err(SignInError::Invalid)));
    let unknown = test.db.sign_in("nobody@x.test", PASSWORD).await;
    assert!(matches!(unknown, Err(SignInError::Invalid)));
    test.db.set_disabled(root.id, ann.id, true).await.unwrap();
    let disabled = test.db.sign_in("ann@x.test", PASSWORD).await;
    assert!(matches!(disabled, Err(SignInError::Invalid)));
}

#[tokio::test]
async fn stored_hash_is_argon2id_and_token_is_not_stored() {
    let test = TestDb::create().await;
    let ann = test
        .db
        .create_user(None, new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let pool = test.db.test_pool();
    let hash: String = sqlx::query("select password_hash from users where id = $1")
        .bind(ann.id.0)
        .fetch_one(pool)
        .await
        .unwrap()
        .get(0);
    assert!(hash.starts_with("$argon2id$"));
    assert!(!hash.contains(PASSWORD));

    let (_, token) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    let cookie = token.as_cookie_value();
    assert_eq!(cookie.len(), 64);
    assert!(cookie.bytes().all(|b| b.is_ascii_hexdigit()));
    let row =
        sqlx::query("select s::text as whole, encode(token_hash, 'hex') as hex from sessions s")
            .fetch_one(pool)
            .await
            .unwrap();
    let whole: String = row.get("whole");
    let hex: String = row.get("hex");
    assert!(!whole.contains(&cookie));
    assert_ne!(hex, cookie);
    assert_eq!(hex.len(), 64);
    // The token round-trips through its cookie form and does not show itself.
    let parsed = SessionToken::parse(&cookie).unwrap();
    assert_eq!(parsed.as_cookie_value(), cookie);
    assert!(!format!("{token:?}").contains(&cookie));
    assert!(SessionToken::parse("abc").is_none());
    assert!(SessionToken::parse(&"z".repeat(64)).is_none());
}

#[tokio::test]
async fn ended_and_expired_sessions_are_not_found() {
    let test = TestDb::create().await;
    test.db
        .create_user(None, new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let (session, token) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    assert!(test.db.session(&token).await.unwrap().is_some());
    test.db.end_session(session.id).await.unwrap();
    assert!(test.db.session(&token).await.unwrap().is_none());
    // Ending twice is harmless.
    test.db.end_session(session.id).await.unwrap();

    let (session, token) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    sqlx::query("update sessions set expires_at = now() - interval '1 second' where id = $1")
        .bind(session.id.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    assert!(test.db.session(&token).await.unwrap().is_none());

    let unknown = SessionToken::parse(&"ab".repeat(32)).unwrap();
    assert!(test.db.session(&unknown).await.unwrap().is_none());
}

#[tokio::test]
async fn session_by_id_follows_the_same_rules_as_by_token() {
    let test = TestDb::create().await;
    let root = admin(&test).await;
    let ann = test
        .db
        .create_user(Some(root.id), new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let (session, token) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    let found = test.db.session_by_id(session.id).await.unwrap().unwrap();
    assert_eq!(found, test.db.session(&token).await.unwrap().unwrap());
    assert_eq!(found.user, ann);

    // Ended.
    test.db.end_session(session.id).await.unwrap();
    assert!(test.db.session_by_id(session.id).await.unwrap().is_none());

    // Expired.
    let (session, _) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    sqlx::query("update sessions set expires_at = now() - interval '1 second' where id = $1")
        .bind(session.id.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();
    assert!(test.db.session_by_id(session.id).await.unwrap().is_none());

    // The account disabled.
    let (session, _) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    assert!(test.db.session_by_id(session.id).await.unwrap().is_some());
    test.db.set_disabled(root.id, ann.id, true).await.unwrap();
    assert!(test.db.session_by_id(session.id).await.unwrap().is_none());

    // Unknown.
    let unknown = collab_core::ids::SessionId(uuid::Uuid::new_v4());
    assert!(test.db.session_by_id(unknown).await.unwrap().is_none());
}

#[tokio::test]
async fn disabling_ends_sessions_and_blocks_sign_in() {
    let test = TestDb::create().await;
    let root = admin(&test).await;
    let ann = test
        .db
        .create_user(Some(root.id), new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let (_, first) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    let (_, second) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    test.db.set_disabled(root.id, ann.id, true).await.unwrap();
    assert!(test.db.user(ann.id).await.unwrap().unwrap().disabled);
    assert!(test.db.session(&first).await.unwrap().is_none());
    assert!(test.db.session(&second).await.unwrap().is_none());
    assert!(test.db.sign_in("ann@x.test", PASSWORD).await.is_err());

    test.db.set_disabled(root.id, ann.id, false).await.unwrap();
    // Sessions that were ended stay ended, but signing in works again.
    assert!(test.db.session(&first).await.unwrap().is_none());
    assert!(test.db.sign_in("ann@x.test", PASSWORD).await.is_ok());

    let missing = test
        .db
        .set_disabled(root.id, UserId(uuid::Uuid::new_v4()), true)
        .await;
    assert!(matches!(missing, Err(AccountError::NotFound)));
}

#[tokio::test]
async fn last_admin_cannot_be_disabled() {
    let test = TestDb::create().await;
    let root = admin(&test).await;
    let result = test.db.set_disabled(root.id, root.id, true).await;
    assert!(matches!(result, Err(AccountError::LastAdmin)));
    assert!(!test.db.user(root.id).await.unwrap().unwrap().disabled);

    let second = test
        .db
        .create_user(Some(root.id), new_user("two@x.test", "Two", true))
        .await
        .unwrap();
    test.db
        .set_disabled(second.id, root.id, true)
        .await
        .unwrap();
    // A disabled administrator cannot act, and the remaining one cannot be disabled.
    let result = test.db.set_disabled(second.id, second.id, true).await;
    assert!(matches!(result, Err(AccountError::LastAdmin)));
    let result = test.db.set_disabled(root.id, second.id, false).await;
    assert!(matches!(result, Err(AccountError::Forbidden)));
}

#[tokio::test]
async fn non_admin_cannot_create_or_disable() {
    let test = TestDb::create().await;
    let root = admin(&test).await;
    let ann = test
        .db
        .create_user(Some(root.id), new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let result = test
        .db
        .create_user(Some(ann.id), new_user("bob@x.test", "Bob", false))
        .await;
    assert!(matches!(result, Err(AccountError::Forbidden)));
    let result = test.db.set_disabled(ann.id, root.id, true).await;
    assert!(matches!(result, Err(AccountError::Forbidden)));
    let stranger = UserId(uuid::Uuid::new_v4());
    let result = test
        .db
        .create_user(Some(stranger), new_user("bob@x.test", "Bob", false))
        .await;
    assert!(matches!(result, Err(AccountError::Forbidden)));
    assert_eq!(test.db.users().await.unwrap().len(), 2);
}

#[tokio::test]
async fn account_changes_are_recorded() {
    let test = TestDb::create().await;
    let root = admin(&test).await;
    let ann = test
        .db
        .create_user(Some(root.id), new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    assert!(
        test.db
            .sign_in("ann@x.test", "wrong password!")
            .await
            .is_err()
    );
    assert!(
        test.db
            .sign_in("nobody@x.test", "wrong password!")
            .await
            .is_err()
    );
    let (session, _) = test.db.sign_in("ann@x.test", PASSWORD).await.unwrap();
    test.db.end_session(session.id).await.unwrap();
    test.db.set_disabled(root.id, ann.id, true).await.unwrap();
    test.db.set_disabled(root.id, ann.id, false).await.unwrap();

    let events = test.db.events_about(ann.id).await.unwrap();
    let seen: Vec<_> = events
        .iter()
        .map(|e| (e.kind.as_str(), e.actor, e.subject))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("user.created", Some(root.id), Some(ann.id)),
            ("sign_in.failed", None, Some(ann.id)),
            ("session.started", Some(ann.id), Some(ann.id)),
            ("session.ended", Some(ann.id), Some(ann.id)),
            ("user.disabled", Some(root.id), Some(ann.id)),
            ("user.enabled", Some(root.id), Some(ann.id)),
        ]
    );
    assert!(
        events
            .windows(2)
            .all(|w| w[0].id < w[1].id && w[0].at <= w[1].at)
    );
    assert!(events.iter().all(|e| e.document.is_none()));

    let about_root = test.db.events_about(root.id).await.unwrap();
    assert_eq!(about_root.len(), 1);
    assert_eq!(about_root[0].kind, "user.created");
    assert_eq!(about_root[0].actor, None);
    // Nothing secret reaches the log.
    let all = sqlx::query("select coalesce(string_agg(detail::text, ' '), '') from events")
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
        .get::<String, _>(0);
    assert!(!all.contains(PASSWORD) && !all.contains("argon2"));
}

#[tokio::test]
async fn ten_character_password_is_accepted() {
    let test = TestDb::create().await;
    let password = "0123456789";
    test.db
        .create_user(
            None,
            NewUser {
                email: "a@x.test",
                name: "A",
                password,
                is_admin: false,
            },
        )
        .await
        .unwrap();
    assert!(test.db.sign_in("a@x.test", password).await.is_ok());
}

#[tokio::test]
async fn unknown_email_and_overlong_password_sign_in_fail_cleanly() {
    let test = TestDb::create().await;
    let ann = test
        .db
        .create_user(None, new_user("ann@x.test", "Ann", false))
        .await
        .unwrap();
    let count = || async {
        sqlx::query_scalar::<_, i64>("select count(*) from events")
            .fetch_one(test.db.test_pool())
            .await
            .unwrap()
    };
    let before = count().await;
    let unknown = test.db.sign_in("nobody@x.test", PASSWORD).await;
    assert!(matches!(unknown, Err(SignInError::Invalid)));
    assert_eq!(count().await, before, "an unknown email writes no event");

    let long = "p".repeat(100_000);
    let result = test.db.sign_in("ann@x.test", &long).await;
    assert!(matches!(result, Err(SignInError::Invalid)));
    let result = test.db.sign_in("nobody@x.test", &long).await;
    assert!(matches!(result, Err(SignInError::Invalid)));
    let failures = test.db.events_about(ann.id).await.unwrap();
    assert_eq!(
        failures
            .iter()
            .filter(|e| e.kind == "sign_in.failed")
            .count(),
        1
    );
}

#[tokio::test]
async fn simultaneous_disables_of_different_admins_do_not_deadlock() {
    let test = TestDb::create().await;
    let pool = test.db.test_pool();
    let mut ids = Vec::new();
    for email in ["one@x.test", "two@x.test"] {
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            "insert into users (id, email, name, password_hash, is_admin) \
             values ($1, $2, 'Admin', 'unused', true)",
        )
        .bind(id)
        .bind(email)
        .execute(pool)
        .await
        .unwrap();
        ids.push(UserId(id));
    }
    let (one, two) = (ids[0], ids[1]);
    for round in 0..10 {
        sqlx::query("update users set disabled_at = null")
            .execute(pool)
            .await
            .unwrap();
        let first = test.db.clone();
        let second = test.db.clone();
        let (a, b) = tokio::join!(
            tokio::spawn(async move { first.set_disabled(one, two, true).await }),
            tokio::spawn(async move { second.set_disabled(two, one, true).await }),
        );
        for result in [a.unwrap(), b.unwrap()] {
            assert!(
                matches!(
                    result,
                    Ok(()) | Err(AccountError::LastAdmin | AccountError::Forbidden)
                ),
                "round {round}: {result:?}"
            );
        }
        let enabled: i64 =
            sqlx::query_scalar("select count(*) from users where is_admin and disabled_at is null")
                .fetch_one(pool)
                .await
                .unwrap();
        assert!(
            enabled >= 1,
            "round {round}: no enabled administrator is left"
        );
    }
}
