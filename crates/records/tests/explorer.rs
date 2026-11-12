#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use collab_core::ids::{GroupId, NodeId, UserId};
use common::{Tree, new_id, put, tree, user};
use records::test_support::{TestDb, break_counts_on, breaks_on};
use records::{
    AccessError, AssignmentView, Break, Crumb, ExplorerFilter, NodeKind, Perms, Principal,
    PrincipalView, RoleRef, SiteGroup, UserRef, breaks_csv,
};
use uuid::Uuid;

/// Every break `viewer` sees from `root` with `filter`, all pages of two; checked to be what
/// [`records::Db::all_breaks`] reads at once (as the CSV export does), across every page
/// boundary.
async fn all_breaks(
    test: &TestDb,
    viewer: UserId,
    root: NodeId,
    filter: &ExplorerFilter,
) -> Option<Vec<Break>> {
    let at_once = test.db.all_breaks(viewer, root, filter).await.unwrap();
    let paged = paged_breaks(test, viewer, root, filter).await;
    assert_eq!(paged, at_once, "pages of two and the whole list differ");
    paged
}

/// Every break `viewer` sees from `root` with `filter`, all pages of two.
async fn paged_breaks(
    test: &TestDb,
    viewer: UserId,
    root: NodeId,
    filter: &ExplorerFilter,
) -> Option<Vec<Break>> {
    let mut found = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, next) = test
            .db
            .breaks(viewer, root, filter, after.as_deref(), 2)
            .await
            .unwrap()?;
        assert!(page.len() <= 2);
        found.extend(page);
        match next {
            Some(next) => after = Some(next),
            None => return Some(found),
        }
    }
}

fn ids(breaks: &[Break]) -> Vec<NodeId> {
    breaks.iter().map(|b| b.node.id).collect()
}

fn names(crumbs: &[Crumb]) -> Vec<&str> {
    crumbs.iter().map(|c| c.name.as_str()).collect()
}

fn everything() -> ExplorerFilter {
    ExplorerFilter::default()
}

fn editor_perms() -> Perms {
    Perms::ADD.union(Perms::EDIT).union(Perms::DELETE)
}

/// A folder named `name` under `parent` in `site`.
async fn folder(test: &TestDb, parent: NodeId, site: NodeId, name: &str) -> NodeId {
    put(test, new_id(), NodeKind::Folder, Some(parent), site, name).await
}

async fn site_group(test: &TestDb, site: NodeId, purpose: SiteGroup) -> GroupId {
    let id: Uuid = sqlx::query_scalar("select id from groups where site_id = $1 and purpose = $2")
        .bind(site.0)
        .bind(purpose.as_str())
        .fetch_one(test.db.test_pool())
        .await
        .unwrap();
    GroupId(id)
}

async fn give(test: &TestDb, actor: UserId, node: NodeId, principal: Principal, role: &str) {
    test.db
        .set_node_access(actor, node, principal, Some(role))
        .await
        .unwrap();
}

async fn break_at(test: &TestDb, actor: UserId, node: NodeId) {
    assert!(test.db.break_inheritance(actor, node).await.unwrap());
}

#[tokio::test]
async fn finds_every_nested_break() {
    let test = TestDb::create().await;
    let owner = user(&test, "Owner", false).await;
    let t = tree(&test, owner.id).await;
    let quiet = folder(&test, t.library, t.site, "Quiet").await;
    for node in [t.library, t.a, t.b, t.file] {
        break_at(&test, owner.id, node).await;
    }

    let found = all_breaks(&test, owner.id, t.site, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.site, t.library, t.a, t.b, t.file]);
    assert!(!ids(&found).contains(&quiet));
    let kinds: Vec<NodeKind> = found.iter().map(|b| b.kind).collect();
    assert_eq!(
        kinds,
        [
            NodeKind::Site,
            NodeKind::Library,
            NodeKind::Folder,
            NodeKind::Folder,
            NodeKind::Document
        ]
    );
    let below: Vec<i64> = found.iter().map(|b| b.breaks_below).collect();
    assert_eq!(below, [4, 3, 2, 1, 0]);
    let file = &found[4];
    assert_eq!(names(&file.path), ["Site", "Documents", "A", "B"]);
    assert_eq!(file.node.name, "Notes");
    assert_eq!(file.inherited_from.as_ref().map(|c| c.id), Some(t.b));
    assert_eq!(found[0].inherited_from, None);
    assert!(found[0].path.is_empty());
    // Each break was copied from the one above: the owner and the site's three groups.
    for b in &found {
        assert_eq!(b.assignments.len(), 4, "{b:?}");
        assert!(b.node.role.as_ref().unwrap().can_manage_access());
    }

    // Started lower, the explorer lists the breaks from there, the start included.
    let from_a = all_breaks(&test, owner.id, t.a, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&from_a), [t.a, t.b, t.file]);
    assert!(from_a[0].path.is_empty());
    assert_eq!(names(&from_a[2].path), ["A", "B"]);
    assert_eq!(
        from_a[0].inherited_from.as_ref().map(|c| c.id),
        Some(t.library)
    );

    // A start that inherits is not listed itself.
    test.db.restore_inheritance(owner.id, t.a).await.unwrap();
    let from_a = all_breaks(&test, owner.id, t.a, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&from_a), [t.b, t.file]);
    assert_eq!(
        from_a[0].inherited_from.as_ref().map(|c| c.id),
        Some(t.library)
    );
}

#[tokio::test]
async fn exceptions_keep_ancestor_paths() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let t = tree(&test, owner.id).await;
    // A is broken and the owner taken off it; B, inside A, is broken and given back to them.
    break_at(&test, admin.id, t.a).await;
    test.db
        .set_node_access(admin.id, t.a, Principal::User(owner.id), None)
        .await
        .unwrap();
    give(&test, admin.id, t.b, Principal::User(owner.id), "owner").await;

    let found = all_breaks(&test, owner.id, t.site, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.site, t.b]);
    let b = &found[1];
    // The path runs through A, where the owner has no access, so that B is found where it is.
    assert_eq!(names(&b.path), ["Site", "Documents", "A"]);
    assert_eq!(b.path[2].role, None);
    assert!(b.path[1].role.as_ref().unwrap().can_manage_access());
    assert_eq!(b.inherited_from.as_ref().map(|c| c.id), Some(t.a));
    assert_eq!(found[0].breaks_below, 1);

    // Nothing is listed from A, which the owner does not manage.
    assert!(
        test.db
            .breaks(owner.id, t.a, &everything(), None, 10)
            .await
            .unwrap()
            .is_none()
    );
    // From B, the scope above it is not theirs to see.
    let from_b = all_breaks(&test, owner.id, t.b, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&from_b), [t.b]);
    assert_eq!(from_b[0].inherited_from, None);
    // An administrator sees both, with the same paths.
    let all = all_breaks(&test, admin.id, t.site, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&all), [t.site, t.a, t.b]);
    assert_eq!(names(&all[2].path), ["Site", "Documents", "A"]);
    assert_eq!(all[1].breaks_below, 1);
}

#[tokio::test]
async fn counts_match_the_list() {
    let test = TestDb::create().await;
    let owner = user(&test, "Owner", false).await;
    let t = tree(&test, owner.id).await;
    let c = folder(&test, t.library, t.site, "C").await;
    let d = folder(&test, c, t.site, "D").await;
    let other = put(
        &test,
        new_id(),
        NodeKind::Library,
        Some(t.site),
        t.site,
        "Other",
    )
    .await;
    for node in [t.a, t.b, t.file, c, d, other] {
        break_at(&test, owner.id, node).await;
    }

    let found = all_breaks(&test, owner.id, t.site, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.site, t.a, t.b, t.file, c, d, other]);
    assert_eq!(
        test.db.break_counts(owner.id, t.site).await.unwrap(),
        [(t.library, 5), (other, 1)]
    );
    assert_eq!(
        test.db.break_counts(owner.id, t.library).await.unwrap(),
        [(t.a, 3), (c, 2)]
    );
    // Every break's count in its parent's tree is itself and what is below it.
    for b in &found[1..] {
        let parent = b.path.last().unwrap().id;
        let counts = test.db.break_counts(owner.id, parent).await.unwrap();
        let own = counts.iter().find(|(id, _)| *id == b.node.id).unwrap().1;
        assert_eq!(own, 1 + b.breaks_below, "{b:?}");
    }
    // A child with no break below it is left out.
    let quiet = folder(&test, t.library, t.site, "Quiet").await;
    let counts = test.db.break_counts(owner.id, t.library).await.unwrap();
    assert!(!counts.iter().any(|(id, _)| *id == quiet));
    assert!(
        test.db
            .break_counts(owner.id, t.file)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn filter_by_principal_kind_and_place() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, owner.id).await;
    let reviewers = test.db.create_group(admin.id, "Reviewers").await.unwrap();
    test.db
        .add_member(admin.id, reviewers.id, bob.id)
        .await
        .unwrap();
    break_at(&test, owner.id, t.library).await;
    give(
        &test,
        admin.id,
        t.a,
        Principal::Group(reviewers.id),
        "viewer",
    )
    .await;
    give(&test, owner.id, t.b, Principal::User(bob.id), "editor").await;
    break_at(&test, owner.id, t.file).await;

    let by = |principal| ExplorerFilter {
        principal: Some(principal),
        kinds: Vec::new(),
    };
    // An administrator sees the members of every group: Bob is on A through Reviewers.
    let found = all_breaks(&test, admin.id, t.site, &by(Principal::User(bob.id)))
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.a, t.b, t.file]);
    // The site's owner may not see an organisation group's members: only Bob's own assignments.
    let found = all_breaks(&test, owner.id, t.site, &by(Principal::User(bob.id)))
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.b, t.file]);
    // The counts follow the filter.
    assert_eq!(found[0].breaks_below, 1);
    let found = all_breaks(&test, owner.id, t.site, &by(Principal::Group(reviewers.id)))
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.a, t.b, t.file]);
    let a = &found[0];
    assert!(a.assignments.iter().any(|x| x.principal
        == PrincipalView::Group {
            id: reviewers.id,
            name: "Reviewers".to_owned(),
            site: None,
        }));
    // By kind.
    let folders = ExplorerFilter {
        principal: None,
        kinds: vec![NodeKind::Folder],
    };
    let found = all_breaks(&test, owner.id, t.site, &folders).await.unwrap();
    assert_eq!(ids(&found), [t.a, t.b]);
    let files = ExplorerFilter {
        principal: Some(Principal::User(bob.id)),
        kinds: vec![NodeKind::Document],
    };
    let found = all_breaks(&test, owner.id, t.site, &files).await.unwrap();
    assert_eq!(ids(&found), [t.file]);
    // By place: the explorer's root.
    let found = all_breaks(&test, owner.id, t.b, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.b, t.file]);

    // The site's owner manages the site's groups and sees their members: in Visitors, Bob is
    // on every break, each carrying the site's groups. The group is shown with its site.
    let visitors = site_group(&test, t.site, SiteGroup::Visitors).await;
    test.db
        .add_member(owner.id, visitors, bob.id)
        .await
        .unwrap();
    let found = all_breaks(&test, owner.id, t.site, &by(Principal::User(bob.id)))
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.site, t.library, t.a, t.b, t.file]);
    assert!(found[0].assignments.iter().any(|x| x.principal
        == PrincipalView::Group {
            id: visitors,
            name: "Site Visitors".to_owned(),
            site: Some((t.site, "Site".to_owned())),
        }));
}

#[tokio::test]
async fn explorer_stays_inside_the_viewers_scope() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let carol = user(&test, "Carol", false).await;
    let t = tree(&test, owner.id).await;
    let c = folder(&test, t.library, t.site, "C").await;
    break_at(&test, owner.id, t.library).await;
    break_at(&test, owner.id, c).await;
    give(&test, admin.id, t.a, Principal::User(carol.id), "owner").await;
    break_at(&test, admin.id, t.b).await;
    break_at(&test, admin.id, t.file).await;

    for outside in [t.site, t.library, c] {
        assert!(
            test.db
                .breaks(carol.id, outside, &everything(), None, 10)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            test.db
                .break_counts(carol.id, outside)
                .await
                .unwrap()
                .is_empty()
        );
    }
    let found = all_breaks(&test, carol.id, t.a, &everything())
        .await
        .unwrap();
    assert_eq!(ids(&found), [t.a, t.b, t.file]);
    let outside_names = ["Site", "Documents", "C"];
    for b in &found {
        for crumb in b.path.iter().chain(b.inherited_from.iter()) {
            assert!(!outside_names.contains(&crumb.name.as_str()), "{b:?}");
        }
    }
    assert_eq!(found[0].inherited_from, None);
    assert_eq!(found[0].breaks_below, 2);
    assert_eq!(
        test.db.break_counts(carol.id, t.a).await.unwrap(),
        [(t.b, 2)]
    );
    // Restoring A is judged on the library, which Carol does not manage.
    assert_eq!(
        test.db.restore_preview(carol.id, t.a).await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        test.db.restore_preview(carol.id, c).await,
        Err(AccessError::NotFound)
    );
    // Someone who manages nothing sees nothing.
    let nobody = user(&test, "Nobody", false).await;
    assert!(
        test.db
            .breaks(nobody.id, t.site, &everything(), None, 10)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn broken_by_and_changed_by_from_events() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let bob = user(&test, "Bob", false).await;
    let t = tree(&test, owner.id).await;
    break_at(&test, owner.id, t.a).await;
    give(&test, admin.id, t.b, Principal::User(bob.id), "viewer").await;

    let found = all_breaks(&test, admin.id, t.a, &everything())
        .await
        .unwrap();
    let (a, b) = (&found[0], &found[1]);
    let who = |stamp: &Option<(UserRef, time::OffsetDateTime)>| {
        stamp.as_ref().map(|(user, _)| user.clone())
    };
    let owner_ref = UserRef {
        id: owner.id,
        name: "Owner".to_owned(),
    };
    let admin_ref = UserRef {
        id: admin.id,
        name: "Admin".to_owned(),
    };
    assert_eq!(who(&a.broken_by), Some(owner_ref.clone()));
    assert_eq!(who(&a.changed_by), Some(owner_ref.clone()));
    // Giving Bob a role on B broke inheritance there first, in the admin's name.
    assert_eq!(who(&b.broken_by), Some(admin_ref.clone()));
    assert_eq!(who(&b.changed_by), Some(admin_ref.clone()));

    give(&test, owner.id, t.a, Principal::User(bob.id), "editor").await;
    let found = all_breaks(&test, admin.id, t.a, &everything())
        .await
        .unwrap();
    let a = &found[0];
    assert_eq!(who(&a.broken_by), Some(owner_ref.clone()));
    assert_eq!(who(&a.changed_by), Some(owner_ref.clone()));
    test.db
        .set_node_access(admin.id, t.a, Principal::User(bob.id), None)
        .await
        .unwrap();
    let found = all_breaks(&test, admin.id, t.a, &everything())
        .await
        .unwrap();
    let a = &found[0];
    assert_eq!(who(&a.broken_by), Some(owner_ref));
    assert_eq!(who(&a.changed_by), Some(admin_ref));
    assert!(a.changed_by.as_ref().unwrap().1 >= a.broken_by.as_ref().unwrap().1);
    // The site was never broken: it is a scope from the start.
    let site = all_breaks(&test, admin.id, t.site, &everything())
        .await
        .unwrap();
    assert_eq!(site[0].broken_by, None);
}

#[tokio::test]
async fn restore_preview_lists_losers_gainers_and_independent_breaks() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let [alice, carol, dave, erin] = [
        user(&test, "Alice", false).await,
        user(&test, "Carol", false).await,
        user(&test, "Dave", false).await,
        user(&test, "Erin", false).await,
    ];
    let t = tree(&test, owner.id).await;
    let readers = test.db.create_group(admin.id, "Readers").await.unwrap();
    test.db
        .add_member(admin.id, readers.id, erin.id)
        .await
        .unwrap();
    // The library: Carol a viewer, Dave an editor.
    give(
        &test,
        owner.id,
        t.library,
        Principal::User(carol.id),
        "viewer",
    )
    .await;
    give(
        &test,
        owner.id,
        t.library,
        Principal::User(dave.id),
        "editor",
    )
    .await;
    // A, broken from it: Alice an editor, Carol made an editor, Dave taken off.
    give(&test, owner.id, t.a, Principal::User(alice.id), "editor").await;
    give(&test, owner.id, t.a, Principal::User(carol.id), "editor").await;
    test.db
        .set_node_access(owner.id, t.a, Principal::User(dave.id), None)
        .await
        .unwrap();
    // Then the library gives Readers (Erin) view, and below A two breaks of their own.
    give(
        &test,
        owner.id,
        t.library,
        Principal::Group(readers.id),
        "viewer",
    )
    .await;
    break_at(&test, owner.id, t.b).await;
    break_at(&test, owner.id, t.file).await;

    // An administrator sees every group's members: Erin gains through Readers.
    let preview = test.db.restore_preview(admin.id, t.a).await.unwrap();
    assert_eq!(preview.node.id, t.a);
    assert_eq!(preview.inherit_from.id, t.library);
    let people: Vec<(&str, Perms, Perms)> = preview
        .people
        .iter()
        .map(|(user, before, after)| (user.name.as_str(), *before, *after))
        .collect();
    assert_eq!(
        people,
        [
            ("Alice", editor_perms(), Perms::NONE),
            ("Carol", editor_perms(), Perms::VIEW),
            ("Dave", Perms::NONE, editor_perms()),
            ("Erin", Perms::NONE, Perms::VIEW),
        ]
    );
    assert!(preview.groups.is_empty());
    assert_eq!(preview.unchanged_breaks_below, 2);
    // The site's owner may not see Readers' members: the group's change is listed instead.
    let preview = test.db.restore_preview(owner.id, t.a).await.unwrap();
    let people: Vec<&str> = preview
        .people
        .iter()
        .map(|(u, _, _)| u.name.as_str())
        .collect();
    assert_eq!(people, ["Alice", "Carol", "Dave"]);
    assert_eq!(
        preview.groups,
        [(
            PrincipalView::Group {
                id: readers.id,
                name: "Readers".to_owned(),
                site: None,
            },
            Perms::NONE,
            Perms::VIEW
        )]
    );
    assert_eq!(preview.unchanged_breaks_below, 2);
    // The preview changes nothing.
    assert!(
        test.db
            .effective_access(alice.id, t.a)
            .await
            .unwrap()
            .is_some()
    );

    assert_eq!(
        test.db.restore_preview(owner.id, t.site).await,
        Err(AccessError::IsSite)
    );
    assert_eq!(
        test.db.restore_preview(alice.id, t.a).await,
        Err(AccessError::Forbidden)
    );
    // A node that inherits: restoring would change nothing.
    let q = folder(&test, t.library, t.site, "Q").await;
    let quiet = test.db.restore_preview(owner.id, q).await.unwrap();
    assert!(quiet.people.is_empty());
    assert_eq!(quiet.inherit_from.id, t.library);
    assert_eq!(quiet.unchanged_breaks_below, 0);
}

#[tokio::test]
async fn restore_preview_counts_people_not_principals() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let frank = user(&test, "Frank", false).await;
    let gina = user(&test, "Gina", false).await;
    let t = tree(&test, owner.id).await;
    let writers = test.db.create_group(admin.id, "Writers").await.unwrap();
    let editors = test.db.create_group(admin.id, "Editors").await.unwrap();
    for (group, person) in [
        (writers.id, frank.id),
        (editors.id, frank.id),
        (writers.id, gina.id),
    ] {
        test.db.add_member(admin.id, group, person).await.unwrap();
    }
    give(
        &test,
        admin.id,
        t.library,
        Principal::Group(editors.id),
        "editor",
    )
    .await;
    give(&test, admin.id, t.a, Principal::Group(writers.id), "editor").await;

    // Writers lose their role on A; Frank keeps editing through Editors, Gina does not. An
    // administrator sees both groups' members.
    let preview = test.db.restore_preview(admin.id, t.a).await.unwrap();
    let people: Vec<(UserId, Perms, Perms)> = preview
        .people
        .iter()
        .map(|(user, before, after)| (user.id, *before, *after))
        .collect();
    assert_eq!(people, [(gina.id, editor_perms(), Perms::NONE)]);
}

#[tokio::test]
async fn restore_preview_hides_members_of_hidden_groups() {
    let test = TestDb::create().await;
    let admin = user(&test, "Admin", true).await;
    let owner = user(&test, "Owner", false).await;
    let erin = user(&test, "Erin", false).await;
    let hal = user(&test, "Hal", false).await;
    let t = tree(&test, owner.id).await;
    let readers = test.db.create_group(admin.id, "Readers").await.unwrap();
    for person in [erin.id, hal.id] {
        test.db
            .add_member(admin.id, readers.id, person)
            .await
            .unwrap();
    }
    // Readers view A only; Hal is also given view on A directly.
    give(&test, admin.id, t.a, Principal::Group(readers.id), "viewer").await;
    give(&test, owner.id, t.a, Principal::User(hal.id), "viewer").await;

    // The site's owner (not an administrator) sees "Readers: view -> none" and no member,
    // not even Hal, whose own assignment is all that is counted for him.
    let preview = test.db.restore_preview(owner.id, t.a).await.unwrap();
    let people: Vec<(UserId, Perms, Perms)> = preview
        .people
        .iter()
        .map(|(user, before, after)| (user.id, *before, *after))
        .collect();
    assert_eq!(people, [(hal.id, Perms::VIEW, Perms::NONE)]);
    assert!(!preview.people.iter().any(|(u, _, _)| u.id == erin.id));
    assert_eq!(
        preview.groups,
        [(
            PrincipalView::Group {
                id: readers.id,
                name: "Readers".to_owned(),
                site: None,
            },
            Perms::VIEW,
            Perms::NONE
        )]
    );
    // An administrator sees the people, and no group entry.
    let preview = test.db.restore_preview(admin.id, t.a).await.unwrap();
    let people: Vec<(UserId, Perms, Perms)> = preview
        .people
        .iter()
        .map(|(user, before, after)| (user.id, *before, *after))
        .collect();
    assert_eq!(
        people,
        [
            (erin.id, Perms::VIEW, Perms::NONE),
            (hal.id, Perms::VIEW, Perms::NONE)
        ]
    );
    assert!(preview.groups.is_empty());
}

#[test]
fn csv_quotes_and_neutralises_formulas() {
    let crumb = |name: &str, kind| Crumb {
        id: NodeId(Uuid::nil()),
        kind,
        name: name.to_owned(),
        role: None,
    };
    let user = |name: &str| UserRef {
        id: UserId(Uuid::nil()),
        name: name.to_owned(),
    };
    let role = |id: &str, name: &str| RoleRef {
        id: id.to_owned(),
        name: name.to_owned(),
    };
    let at = time::Date::from_calendar_date(2026, time::Month::November, 11)
        .unwrap()
        .with_hms(9, 30, 5)
        .unwrap()
        .assume_offset(time::UtcOffset::from_hms(2, 0, 0).unwrap());
    let breaks = [
        Break {
            node: crumb("=SUM(A1)", NodeKind::Folder),
            path: vec![
                crumb("Site", NodeKind::Site),
                crumb("Docs, old", NodeKind::Library),
            ],
            kind: NodeKind::Folder,
            assignments: vec![
                AssignmentView {
                    principal: PrincipalView::User(user("Smith, \"Jo\"")),
                    role: role("owner", "Owner"),
                    perms: Perms::ALL,
                },
                AssignmentView {
                    principal: PrincipalView::Group {
                        id: GroupId(Uuid::nil()),
                        name: "-cmd".to_owned(),
                        site: None,
                    },
                    role: role("x", "+Contributor"),
                    perms: Perms::ADD,
                },
                AssignmentView {
                    principal: PrincipalView::Group {
                        id: GroupId(Uuid::nil()),
                        name: "Site Visitors".to_owned(),
                        site: Some((NodeId(Uuid::nil()), "Site".to_owned())),
                    },
                    role: role("viewer", "Viewer"),
                    perms: Perms::VIEW,
                },
            ],
            inherited_from: None,
            broken_by: Some((user("@admin"), at)),
            changed_by: None,
            breaks_below: 0,
        },
        Break {
            node: crumb("Notes\nv2", NodeKind::Document),
            path: Vec::new(),
            kind: NodeKind::Document,
            assignments: Vec::new(),
            inherited_from: None,
            broken_by: None,
            changed_by: None,
            breaks_below: 0,
        },
    ];
    let csv = breaks_csv(&breaks);
    let expected = "path,kind,principal kind,principal,role,permissions,broken by,broken at\r\n\
        \"Site / Docs, old / =SUM(A1)\",folder,person,\"Smith, \"\"Jo\"\"\",Owner,\
        view add edit delete manage_access manage_site,'@admin,2026-11-11T07:30:05Z\r\n\
        \"Site / Docs, old / =SUM(A1)\",folder,group,'-cmd,'+Contributor,view add,\
        '@admin,2026-11-11T07:30:05Z\r\n\
        \"Site / Docs, old / =SUM(A1)\",folder,site group,Site Visitors,Viewer,view,\
        '@admin,2026-11-11T07:30:05Z\r\n\
        \"Notes\nv2\",document,,,,,,\r\n";
    assert_eq!(csv, expected);
    // A cell that starts a formula is neutralised even where it is the whole path.
    let lone = [Break {
        path: Vec::new(),
        ..breaks[0].clone()
    }];
    assert!(breaks_csv(&lone).contains("\r\n'=SUM(A1),folder,"));
}

/// Rows of `nodes` read so far in the transaction on `conn`, and how many sequential scans of
/// it were started.
async fn nodes_read(conn: &mut sqlx::PgConnection) -> (i64, i64) {
    sqlx::query_as(
        "select (coalesce(seq_tup_read, 0) + coalesce(idx_tup_fetch, 0))::int8, \
                coalesce(seq_scan, 0)::int8 \
         from pg_stat_xact_user_tables \
         where relname = 'nodes' and schemaname = current_schema()",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

/// Rows of `nodes` the explorer reads, as `viewer`, listing every break from `site` and the
/// counts of the site's and the library's children; and the number of breaks listed.
async fn explorer_reads(
    test: &TestDb,
    viewer: UserId,
    site: NodeId,
    library: NodeId,
) -> (i64, usize) {
    let mut tx = test.db.test_pool().begin().await.unwrap();
    let (read_before, scans_before) = nodes_read(&mut tx).await;
    let (found, next) = breaks_on(&mut tx, viewer, site, &everything(), None, 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next, None);
    assert!(
        !break_counts_on(&mut tx, viewer, site)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !break_counts_on(&mut tx, viewer, library)
            .await
            .unwrap()
            .is_empty()
    );
    let (read_after, scans_after) = nodes_read(&mut tx).await;
    tx.rollback().await.unwrap();
    assert_eq!(scans_after, scans_before, "the explorer scanned nodes");
    (read_after - read_before, found.len())
}

/// Adds files `first..=last` named `f<i>.txt`, spread over `folders`.
async fn add_files(test: &TestDb, site: NodeId, folders: &[NodeId], first: i32, last: i32) {
    let folders: Vec<Uuid> = folders.iter().map(|f| f.0).collect();
    sqlx::query(
        "insert into nodes (id, kind, parent_id, site_id, name, created_at) \
         select gen_random_uuid(), 'document', ($1::uuid[])[i % cardinality($1::uuid[]) + 1], \
                $2, 'f' || i || '.txt', now() \
         from generate_series($3, $4) i",
    )
    .bind(&folders)
    .bind(site.0)
    .bind(first)
    .bind(last)
    .execute(test.db.test_pool())
    .await
    .unwrap();
    sqlx::query("analyze nodes")
        .execute(test.db.test_pool())
        .await
        .unwrap();
}

async fn file_named(test: &TestDb, name: &str) -> NodeId {
    NodeId(
        sqlx::query_scalar("select id from nodes where name = $1")
            .bind(name)
            .fetch_one(test.db.test_pool())
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn explorer_cost_follows_breaks() {
    let test = TestDb::create().await;
    let owner = user(&test, "Owner", false).await;
    let Tree { site, library, .. } = tree(&test, owner.id).await;
    let mut folders = Vec::new();
    for i in 0..10 {
        folders.push(folder(&test, library, site, &format!("Folder {i}")).await);
    }
    let sub = folder(&test, folders[0], site, "Sub").await;
    add_files(&test, site, &folders, 1, 5_000).await;
    // Ten breaks: the library, four folders, a sub-folder and four files.
    let mut broken = vec![library, folders[0], folders[1], folders[2], folders[3], sub];
    for name in ["f1.txt", "f2.txt", "f3.txt", "f4.txt"] {
        broken.push(file_named(&test, name).await);
    }
    for node in &broken {
        break_at(&test, owner.id, *node).await;
    }
    // The scopes and every node above them: the site, the library, the four folders, the
    // sub-folder and the four files (whose folders are already counted).
    let scopes_and_ancestors = 11;

    let (read_5k, listed) = explorer_reads(&test, owner.id, site, library).await;
    assert_eq!(listed, 11);
    assert!(
        read_5k <= 20 * scopes_and_ancestors,
        "read {read_5k} rows of nodes with 5,000 files"
    );

    add_files(&test, site, &folders, 5_001, 50_000).await;
    let (read_50k, listed) = explorer_reads(&test, owner.id, site, library).await;
    assert_eq!(listed, 11);
    assert!(
        read_50k <= read_5k,
        "read {read_50k} rows of nodes with 50,000 files, {read_5k} with 5,000"
    );
    println!("rows of nodes read: {read_5k} with 5,000 files, {read_50k} with 50,000");
}

/// The explorer's walk starts from the live scopes of the explorer's own site, through their
/// index: another site's scopes, however many, are never read.
#[tokio::test]
async fn explorer_reads_only_its_own_sites_scopes() {
    let test = TestDb::create().await;
    let owner = user(&test, "Owner", false).await;
    let Tree {
        site, library, a, ..
    } = tree(&test, owner.id).await;
    // Twenty other sites of 250 files each: enough rows that every read goes through an index.
    for i in 0..20 {
        let other = common::site(&test, &format!("Site {i}"), None).await;
        add_files(&test, other, &[other], 1, 250).await;
    }
    break_at(&test, owner.id, library).await;
    break_at(&test, owner.id, a).await;
    let plan = test.db.explain_walk(site, site).await.unwrap();
    assert!(
        plan.contains("Index Scan using nodes_site_scopes on nodes n"),
        "{plan}"
    );
    let (before, listed) = explorer_reads(&test, owner.id, site, library).await;
    assert_eq!(listed, 3);

    // Another site with 5,000 scopes of its own.
    let other = common::site(&test, "Other", None).await;
    sqlx::query(
        "insert into nodes (id, kind, parent_id, site_id, name, unique_scope, created_at) \
         select gen_random_uuid(), 'document', $1, $1, 'g' || i || '.txt', true, now() \
         from generate_series(1, 5000) i",
    )
    .bind(other.0)
    .execute(test.db.test_pool())
    .await
    .unwrap();
    sqlx::query("analyze nodes")
        .execute(test.db.test_pool())
        .await
        .unwrap();
    let (after, listed) = explorer_reads(&test, owner.id, site, library).await;
    assert_eq!(listed, 3);
    assert_eq!(after, before, "the explorer read another site's scopes");
}
