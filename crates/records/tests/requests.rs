#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{NodeId, UserId};
use common::{Role, Tree, delete, gate, new_id, put, role_of, tree, user, wait_blocked};
use records::test_support::TestDb;
use records::{
    AccessError, LinkKind, NodeKind, Perms, Principal, REQUESTS_PER_DAY, RequestDecision,
    RequestError,
};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

async fn role(test: &TestDb, user: UserId, node: NodeId) -> Option<Role> {
    role_of(test.db.effective_access(user, node).await.unwrap())
}

async fn folder(test: &TestDb, parent: NodeId, site: NodeId, name: &str) -> NodeId {
    put(test, new_id(), NodeKind::Folder, Some(parent), site, name).await
}

async fn scalar(test: &TestDb, query: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(query.to_owned()))
        .bind(id)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
}

/// How many requests `user` has, open or decided.
async fn requests_of(test: &TestDb, user: UserId) -> i64 {
    scalar(
        test,
        "select count(*) from access_requests where requester = $1",
        user.0,
    )
    .await
}

async fn attempts_of(test: &TestDb, user: UserId) -> i64 {
    scalar(
        test,
        "select count(*) from request_attempts where requester = $1",
        user.0,
    )
    .await
}

/// How many events of `kind` name the request `id` in their detail.
async fn request_events(test: &TestDb, kind: &str, id: Uuid) -> i64 {
    sqlx::query_scalar("select count(*) from events where kind = $1 and detail->>'request' = $2")
        .bind(kind)
        .bind(id.to_string())
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
}

async fn ask(test: &TestDb, who: UserId, node: NodeId) {
    test.db.request_access(who, node, None).await.unwrap();
}

/// The id of the one open request `manager` sees.
async fn only_open(test: &TestDb, manager: UserId) -> Uuid {
    let open = test.db.open_requests(manager).await.unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    open[0].id
}

/// The decision recorded on request `id`, if any.
async fn decision(test: &TestDb, id: Uuid) -> Option<String> {
    sqlx::query_scalar("select decision from access_requests where id = $1")
        .bind(id)
        .fetch_one(test.db.test_pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn requests_never_reveal_existence() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t: Tree = tree(&test, ann.id).await;
    let gone = folder(&test, t.library, t.site, "Gone").await;
    delete(&test, gone).await;
    let shared = folder(&test, t.library, t.site, "Shared").await;
    test.db
        .set_node_access(ann.id, shared, Principal::User(bob.id), Some("viewer"))
        .await
        .unwrap();
    let missing = new_id();

    // An item that exists, one that never did, a deleted one and one already open to Bob: the
    // same answer for each, with and without a message.
    for node in [t.a, missing, gone, shared] {
        assert_eq!(test.db.request_access(bob.id, node, None).await, Ok(()));
        assert_eq!(
            test.db.request_access(bob.id, node, Some("please")).await,
            Ok(())
        );
    }
    // Every submission counted alike; nothing Bob can see tells them apart.
    assert_eq!(attempts_of(&test, bob.id).await, 8);
    assert!(test.db.my_requests(bob.id).await.unwrap().is_empty());
    // Only the real, closed item got a request, which its manager sees.
    assert_eq!(requests_of(&test, bob.id).await, 1);
    let open = test.db.open_requests(ann.id).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].node.id, t.a);

    // The limit counts every kind of submission the same and answers the same for each.
    for i in 8..REQUESTS_PER_DAY {
        let node = [t.a, missing, gone, shared][usize::try_from(i).unwrap() % 4];
        assert_eq!(test.db.request_access(bob.id, node, None).await, Ok(()));
    }
    for node in [t.a, missing, gone, shared] {
        assert_eq!(
            test.db.request_access(bob.id, node, None).await,
            Err(RequestError::TooMany)
        );
    }
    assert_eq!(attempts_of(&test, bob.id).await, REQUESTS_PER_DAY);

    // A message that is too long is refused before anything, whatever the item.
    let long = "x".repeat(501);
    for node in [t.a, missing] {
        assert_eq!(
            test.db.request_access(bob.id, node, Some(&long)).await,
            Err(RequestError::MessageTooLong)
        );
    }
}

#[tokio::test]
async fn one_open_request_per_item() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    test.db
        .request_access(bob.id, t.a, Some("first"))
        .await
        .unwrap();
    test.db
        .request_access(bob.id, t.a, Some("  second  "))
        .await
        .unwrap();
    let open = test.db.open_requests(ann.id).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].message.as_deref(), Some("second"));
    assert_eq!(open[0].requester.id, bob.id);
    assert_eq!(
        request_events(&test, "request.created", open[0].id).await,
        1
    );

    // Once decided, a new request may be made.
    test.db
        .decline_request(ann.id, open[0].id, None)
        .await
        .unwrap();
    ask(&test, bob.id, t.a).await;
    let again = only_open(&test, ann.id).await;
    assert_ne!(again, open[0].id);
    assert_eq!(requests_of(&test, bob.id).await, 2);
}

#[tokio::test]
async fn rate_limit_holds_under_concurrency() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let nodes = [t.a, t.b, new_id(), t.library];
    let mut tasks = Vec::new();
    for i in 0..25 {
        let db = test.db.clone();
        let node = nodes[i % nodes.len()];
        tasks.push(tokio::spawn(async move {
            db.request_access(bob.id, node, None).await
        }));
    }
    let mut taken = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => taken += 1,
            Err(RequestError::TooMany) => refused += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((taken, refused), (20, 5));
    assert_eq!(attempts_of(&test, bob.id).await, 20);
    // One open request per item, whatever the order.
    assert_eq!(requests_of(&test, bob.id).await, 3);

    // A day later the sweeper forgets them, and Bob may ask again.
    let later = OffsetDateTime::now_utc() + Duration::days(1) + Duration::seconds(5);
    assert_eq!(test.db.forget_old_attempts(later).await.unwrap(), 20);
    assert_eq!(
        test.db
            .forget_old_attempts(OffsetDateTime::now_utc())
            .await
            .unwrap(),
        0
    );
    ask(&test, bob.id, t.a).await;
}

#[tokio::test]
async fn managers_see_requests_on_their_items_only() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let carl = user(&test, "Carl", false).await;
    let dave = user(&test, "Dave", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    let other = common::site(&test, "Other", Some(carl.id)).await;
    let theirs = folder(&test, other, other, "Theirs").await;
    // Dave views the first site, without managing it.
    test.db
        .set_node_access(ann.id, t.site, Principal::User(dave.id), Some("viewer"))
        .await
        .unwrap();
    ask(&test, bob.id, t.b).await;
    ask(&test, bob.id, theirs).await;

    let ann_sees = test.db.open_requests(ann.id).await.unwrap();
    assert_eq!(ann_sees.len(), 1);
    assert_eq!(ann_sees[0].node.id, t.b);
    assert_eq!(ann_sees[0].node.name, "B");
    let path: Vec<NodeId> = ann_sees[0].path.iter().map(|c| c.id).collect();
    assert_eq!(path, vec![t.site, t.library, t.a]);
    assert_eq!(ann_sees[0].requester.name, "Bob");
    assert_eq!(test.db.open_request_count(ann.id).await.unwrap(), 1);

    let carl_sees = test.db.open_requests(carl.id).await.unwrap();
    assert_eq!(carl_sees.len(), 1);
    assert_eq!(carl_sees[0].node.id, theirs);
    assert!(test.db.open_requests(dave.id).await.unwrap().is_empty());
    assert_eq!(test.db.open_request_count(dave.id).await.unwrap(), 0);
    assert!(test.db.open_requests(bob.id).await.unwrap().is_empty());

    // An administrator manages everything, so sees both, newest first.
    let all = test.db.open_requests(admin.id).await.unwrap();
    let nodes: Vec<NodeId> = all.iter().map(|r| r.node.id).collect();
    assert_eq!(nodes, vec![theirs, t.b]);
    assert_eq!(test.db.open_request_count(admin.id).await.unwrap(), 2);

    // A folder below B with its own permissions, not managed by Ann, keeps its requests from
    // her once she has no manage_access there.
    let private = folder(&test, t.b, t.site, "Private").await;
    test.db.break_inheritance(ann.id, private).await.unwrap();
    test.db
        .set_node_access(ann.id, private, Principal::User(carl.id), Some("owner"))
        .await
        .unwrap();
    common::without_groups(&test, private).await;
    test.db
        .set_node_access(carl.id, private, Principal::User(ann.id), None)
        .await
        .unwrap();
    ask(&test, bob.id, private).await;
    assert_eq!(test.db.open_request_count(ann.id).await.unwrap(), 1);
    let carl_now = test.db.open_requests(carl.id).await.unwrap();
    assert_eq!(carl_now.len(), 2);

    // A disabled requester's open requests are shown to nobody.
    test.db.set_disabled(admin.id, bob.id, true).await.unwrap();
    assert!(test.db.open_requests(ann.id).await.unwrap().is_empty());
    assert_eq!(test.db.open_request_count(admin.id).await.unwrap(), 0);
}

#[tokio::test]
async fn approve_gives_the_role_directly() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    ask(&test, bob.id, t.b).await;
    let id = only_open(&test, ann.id).await;
    test.db.approve_request(ann.id, id, "editor").await.unwrap();

    assert_eq!(role(&test, bob.id, t.b).await, Some(Role::Editor));
    assert_eq!(role(&test, bob.id, t.file).await, Some(Role::Editor));
    assert_eq!(role(&test, bob.id, t.a).await, None);
    // Directly on the item, which now has its own permissions.
    assert!(test.db.node(t.b).await.unwrap().unwrap().unique_scope);
    let direct: Option<String> = sqlx::query_scalar(
        "select role_id from assignments \
         where node_id = $1 and principal_kind = 'user' and principal_id = $2",
    )
    .bind(t.b.0)
    .bind(bob.id.0)
    .fetch_optional(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(direct.as_deref(), Some("editor"));
    assert_eq!(decision(&test, id).await.as_deref(), Some("approved"));
    assert_eq!(request_events(&test, "request.approved", id).await, 1);
    assert!(test.db.open_requests(ann.id).await.unwrap().is_empty());

    let mine = test.db.my_requests(bob.id).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].decision, RequestDecision::Approved);
    assert_eq!(mine[0].role.as_ref().map(|r| r.id.as_str()), Some("editor"));
    assert_eq!(mine[0].node.as_ref().map(|c| c.id), Some(t.b));
}

#[tokio::test]
async fn approve_never_downgrades() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    ask(&test, bob.id, t.a).await;
    let id = only_open(&test, ann.id).await;
    // Meanwhile Bob was given Editor there.
    test.db
        .set_node_access(ann.id, t.a, Principal::User(bob.id), Some("editor"))
        .await
        .unwrap();
    assert_eq!(
        test.db.approve_request(ann.id, id, "viewer").await,
        Err(AccessError::AlreadyHas)
    );
    assert_eq!(
        test.db.approve_request(ann.id, id, "editor").await,
        Err(AccessError::AlreadyHas)
    );
    assert_eq!(role(&test, bob.id, t.a).await, Some(Role::Editor));
    // The request stays open for the manager to decline.
    assert_eq!(decision(&test, id).await, None);

    // A role that adds a permission but lacks one his assignment gives would replace it:
    // refused too.
    let adder = test
        .db
        .create_role(
            admin.id,
            "Site keeper",
            Perms::ADD.union(Perms::MANAGE_SITE),
        )
        .await
        .unwrap();
    assert_eq!(
        test.db.approve_request(ann.id, id, &adder.id).await,
        Err(AccessError::AlreadyHas)
    );
    assert_eq!(role(&test, bob.id, t.a).await, Some(Role::Editor));
    // A role covering everything he holds and more is an upgrade.
    test.db.approve_request(ann.id, id, "owner").await.unwrap();
    assert_eq!(role(&test, bob.id, t.a).await, Some(Role::Owner));
}

#[tokio::test]
async fn approve_beyond_own_access_refused() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let carl = user(&test, "Carl", false).await;
    let dave = user(&test, "Dave", false).await;
    let eve = user(&test, "Eve", false).await;
    let t = tree(&test, ann.id).await;
    let gatekeeper = test
        .db
        .create_role(admin.id, "Gatekeeper", Perms::MANAGE_ACCESS)
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.a, Principal::User(carl.id), Some(&gatekeeper.id))
        .await
        .unwrap();
    test.db
        .set_node_access(ann.id, t.a, Principal::User(dave.id), Some("viewer"))
        .await
        .unwrap();
    ask(&test, bob.id, t.a).await;
    let id = only_open(&test, carl.id).await;

    assert_eq!(
        test.db.approve_request(carl.id, id, "editor").await,
        Err(AccessError::BeyondYourAccess)
    );
    assert_eq!(
        test.db.approve_request(dave.id, id, "viewer").await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db.approve_request(eve.id, id, "viewer").await,
        Err(AccessError::NotFound)
    );
    assert_eq!(
        test.db.decline_request(eve.id, id, None).await,
        Err(AccessError::NotFound)
    );
    assert_eq!(
        test.db.approve_request(carl.id, id, "nonesuch").await,
        Err(AccessError::NoSuchRole)
    );
    assert_eq!(
        test.db
            .approve_request(carl.id, Uuid::new_v4(), "viewer")
            .await,
        Err(AccessError::NotFound)
    );
    assert_eq!(role(&test, bob.id, t.a).await, None);
    assert_eq!(decision(&test, id).await, None);
    test.db
        .approve_request(carl.id, id, "viewer")
        .await
        .unwrap();
    assert_eq!(role(&test, bob.id, t.a).await, Some(Role::Viewer));
}

#[tokio::test]
async fn my_requests_lists_only_decided() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    ask(&test, bob.id, t.a).await;
    ask(&test, bob.id, t.b).await;
    assert!(test.db.my_requests(bob.id).await.unwrap().is_empty());

    let open = test.db.open_requests(ann.id).await.unwrap();
    let on_a = open.iter().find(|r| r.node.id == t.a).unwrap().id;
    let on_b = open.iter().find(|r| r.node.id == t.b).unwrap().id;
    test.db.decline_request(ann.id, on_a, None).await.unwrap();
    let mine = test.db.my_requests(bob.id).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, on_a);
    assert_eq!(mine[0].decision, RequestDecision::Declined);
    // Bob cannot see A, so it is not named.
    assert_eq!(mine[0].node, None);
    assert!(mine[0].path.is_empty());

    // Once B is approved Bob sees it, and A by its name as limited access above B.
    test.db
        .approve_request(ann.id, on_b, "viewer")
        .await
        .unwrap();
    let mine = test.db.my_requests(bob.id).await.unwrap();
    assert_eq!(mine.len(), 2);
    assert_eq!(mine[0].id, on_b);
    let named = mine[0].node.as_ref().unwrap();
    assert_eq!((named.id, named.name.as_str()), (t.b, "B"));
    let path: Vec<NodeId> = mine[0].path.iter().map(|c| c.id).collect();
    assert_eq!(path, vec![t.site, t.library, t.a]);
    assert_eq!(mine[1].node.as_ref().map(|c| c.id), Some(t.a));
    assert_eq!(mine[1].node.as_ref().unwrap().role, None);
}

#[tokio::test]
async fn decline_reply_reaches_requester() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    ask(&test, bob.id, t.a).await;
    let id = only_open(&test, ann.id).await;
    let long = "y".repeat(501);
    assert_eq!(
        test.db.decline_request(ann.id, id, Some(&long)).await,
        Err(AccessError::MessageTooLong)
    );
    test.db
        .decline_request(ann.id, id, Some(" Ask Carl instead. "))
        .await
        .unwrap();
    let mine = test.db.my_requests(bob.id).await.unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].decision, RequestDecision::Declined);
    assert_eq!(mine[0].reply.as_deref(), Some("Ask Carl instead."));
    assert_eq!(mine[0].role, None);
    assert_eq!(request_events(&test, "request.declined", id).await, 1);
    assert_eq!(role(&test, bob.id, t.a).await, None);
}

#[tokio::test]
async fn decided_twice_refused() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    ask(&test, bob.id, t.a).await;
    let id = only_open(&test, ann.id).await;
    test.db.approve_request(ann.id, id, "viewer").await.unwrap();
    assert_eq!(
        test.db.approve_request(ann.id, id, "editor").await,
        Err(AccessError::AlreadyDecided)
    );
    assert_eq!(
        test.db.decline_request(ann.id, id, None).await,
        Err(AccessError::AlreadyDecided)
    );
    assert_eq!(decision(&test, id).await.as_deref(), Some("approved"));
    assert_eq!(role(&test, bob.id, t.a).await, Some(Role::Viewer));

    ask(&test, bob.id, t.b).await;
    // Bob views A and so B: nothing was recorded.
    assert_eq!(requests_of(&test, bob.id).await, 1);
    let carl = user(&test, "Carl", false).await;
    ask(&test, carl.id, t.b).await;
    let declined = only_open(&test, ann.id).await;
    test.db
        .decline_request(ann.id, declined, None)
        .await
        .unwrap();
    assert_eq!(
        test.db
            .decline_request(ann.id, declined, Some("again"))
            .await,
        Err(AccessError::AlreadyDecided)
    );
    assert_eq!(
        test.db.approve_request(ann.id, declined, "viewer").await,
        Err(AccessError::AlreadyDecided)
    );
    assert_eq!(request_events(&test, "request.declined", declined).await, 1);
}

#[tokio::test]
async fn concurrent_approve_and_decline_take_turns() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, ann.id).await;
    ask(&test, bob.id, t.b).await;
    let id = only_open(&test, ann.id).await;

    // Both start while the item's row is held, so both are waiting when it is let go.
    let held = gate(&test, t.b).await;
    let first = test.db.clone();
    let approve = tokio::spawn(async move { first.approve_request(ann.id, id, "editor").await });
    let second = test.db.clone();
    let decline = tokio::spawn(async move { second.decline_request(ann.id, id, Some("no")).await });
    wait_blocked(&test, held.pid, 2).await;
    held.open().await;
    let results = [approve.await.unwrap(), decline.await.unwrap()];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(
        results.contains(&Err(AccessError::AlreadyDecided)),
        "{results:?}"
    );
    let decisions = request_events(&test, "request.approved", id).await
        + request_events(&test, "request.declined", id).await;
    assert_eq!(decisions, 1);
    let approved = results[0].is_ok();
    assert_eq!(
        decision(&test, id).await.as_deref(),
        Some(if approved { "approved" } else { "declined" })
    );
    assert_eq!(
        role(&test, bob.id, t.b).await,
        approved.then_some(Role::Editor)
    );
}

#[tokio::test]
async fn deleted_item_closes_its_requests() {
    let test = TestDb::create().await;
    let ann = user(&test, "Ann", false).await;
    let bob = user(&test, "Bob", false).await;
    let carl = user(&test, "Carl", false).await;
    let t = tree(&test, ann.id).await;
    let kept = folder(&test, t.library, t.site, "Kept").await;
    ask(&test, bob.id, t.b).await;
    ask(&test, carl.id, t.a).await;
    ask(&test, bob.id, kept).await;
    let open = test.db.open_requests(ann.id).await.unwrap();
    assert_eq!(open.len(), 3);
    let on_b = open.iter().find(|r| r.node.id == t.b).unwrap().id;

    // Deleting A closes the requests on A and below it, and only those.
    delete(&test, t.a).await;
    let closed: i64 = sqlx::query_scalar(
        "select count(*) from access_requests where decision = 'closed' and decided_at is not null",
    )
    .fetch_one(test.db.test_pool())
    .await
    .unwrap();
    assert_eq!(closed, 2);
    let left = test.db.open_requests(ann.id).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].node.id, kept);
    // A closed request is not the requester's to see: it would say the item existed.
    assert!(test.db.my_requests(bob.id).await.unwrap().is_empty());
    assert!(test.db.my_requests(carl.id).await.unwrap().is_empty());
    assert_eq!(
        test.db.approve_request(ann.id, on_b, "viewer").await,
        Err(AccessError::NotFound)
    );
    // Asking again records nothing.
    ask(&test, bob.id, t.b).await;
    assert_eq!(requests_of(&test, bob.id).await, 2);
}

#[tokio::test]
async fn dead_links_do_not_keep_a_role() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let ann = user(&test, "Ann", false).await;
    let t = tree(&test, ann.id).await;
    let reviewer = test
        .db
        .create_role(admin.id, "Reviewer", Perms::EDIT)
        .await
        .unwrap();
    let (on_a, _) = test
        .db
        .create_link(ann.id, t.a, LinkKind::SignedIn, &reviewer.id, None)
        .await
        .unwrap();
    let (on_b, _) = test
        .db
        .create_link(ann.id, t.b, LinkKind::SignedIn, &reviewer.id, None)
        .await
        .unwrap();
    // A link left live under a deleted item, as one made while an ancestor was being deleted
    // could be.
    sqlx::query("update nodes set deleted_at = now(), deleted_with = id where id = $1")
        .bind(t.library.0)
        .execute(test.db.test_pool())
        .await
        .unwrap();

    // It no longer stops the role from becoming a managing one...
    test.db
        .change_role(admin.id, &reviewer.id, None, Some(Perms::EDIT))
        .await
        .unwrap();
    let manages = Perms::EDIT.union(Perms::MANAGE_ACCESS);
    let changed = test
        .db
        .change_role(admin.id, &reviewer.id, None, Some(manages))
        .await
        .unwrap();
    assert!(changed.perms.contains(Perms::MANAGE_ACCESS));
    test.db
        .change_role(admin.id, &reviewer.id, None, Some(Perms::EDIT))
        .await
        .unwrap();
    // ...nor from being deleted: the links are revoked with it.
    test.db.delete_role(admin.id, &reviewer.id).await.unwrap();
    for link in [on_a.id, on_b.id] {
        let now = test.db.link(link).await.unwrap().unwrap();
        let (by, _, reason) = now.revoked.unwrap();
        assert_eq!(reason, "item deleted");
        assert_eq!(by.map(|u| u.id), Some(admin.id));
        assert_eq!(now.role, None);
    }

    // A live link on a live item still keeps its role.
    let other = common::site(&test, "Other", Some(ann.id)).await;
    let checker = test
        .db
        .create_role(admin.id, "Checker", Perms::EDIT)
        .await
        .unwrap();
    test.db
        .create_link(ann.id, other, LinkKind::SignedIn, &checker.id, None)
        .await
        .unwrap();
    assert_eq!(
        test.db.delete_role(admin.id, &checker.id).await,
        Err(AccessError::RoleInUse)
    );
    assert_eq!(
        test.db
            .change_role(admin.id, &checker.id, None, Some(manages))
            .await
            .map(|_| ()),
        Err(AccessError::LinkRoleTooHigh)
    );
}
