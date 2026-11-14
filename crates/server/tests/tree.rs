//! Tree changes, the start-up reconcile and the adoption of documents made before the tree.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use collab_core::ids::{DocId, NodeId, UserId};
use records::test_support::TestDb;
use records::{
    Access, AccessError, Db, Document, Editor, Group, NewUser, Node, NodeKind, Principal,
    SiteGroup, User,
};
use revisions::{
    Author, Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo,
    RevisionStore, Stamp, Stored, TreeChange, TreeMove, TreeRecord,
};
use serde_json::Map;
use server::tree::{self, Adopted, TreeError, TreeOp};
use server::{AppState, Checkpointer, Config};
use sqlx::Connection;
use tempfile::TempDir;
use uuid::Uuid;

/// The built-in roles, by which these tests name access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Viewer,
    Editor,
    Owner,
}

impl Role {
    fn id(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }

    /// The one built-in role giving `access`, if any; panics for any other access.
    fn of(access: Option<Access>) -> Option<Role> {
        let access = access?;
        let [only] = access.roles.as_slice() else {
            panic!("expected one role, got {access:?}");
        };
        let role = [Role::Viewer, Role::Editor, Role::Owner]
            .into_iter()
            .find(|r| r.id() == only.id)
            .unwrap_or_else(|| panic!("{only:?} is not a built-in role"));
        Some(role)
    }
}

/// A server state on a temporary library and a test schema, with nothing served.
struct Env {
    dir: TempDir,
    test: TestDb,
    store: Arc<RevisionStore>,
    state: AppState,
}

impl Env {
    async fn new() -> Env {
        Env::with(|store| store).await
    }

    /// As [`Env::new`], with the server reaching the library through `wrap`.
    async fn with(wrap: impl FnOnce(Arc<RevisionStore>) -> Arc<dyn Checkpointer>) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let test = TestDb::create().await;
        let store = Arc::new(RevisionStore::open(&library(dir.path())).unwrap());
        let checkpointer = wrap(store.clone());
        let blobs = revisions::BlobStore::open(&server::blobs_path(dir.path())).unwrap();
        let state = AppState::new(
            config(dir.path(), &test),
            checkpointer,
            blobs,
            test.db.clone(),
        );
        Env {
            dir,
            test,
            store,
            state,
        }
    }

    fn db(&self) -> &Db {
        &self.test.db
    }

    fn checkpointer(&self) -> Arc<dyn Checkpointer> {
        self.store.clone()
    }

    async fn user(&self, name: &str, is_admin: bool) -> User {
        let email = format!("{}@test.invalid", name.to_lowercase());
        let password = Uuid::new_v4().simple().to_string();
        self.db()
            .create_user(
                None,
                NewUser {
                    email: &email,
                    name,
                    password: &password,
                    is_admin,
                },
            )
            .await
            .unwrap()
    }

    async fn apply(&self, actor: &User, op: TreeOp) -> Result<Node, AccessError> {
        tree::apply(&self.state, actor, op).await
    }

    /// Creates a site named `name` as `admin`; returns it and its Documents library.
    async fn site(&self, admin: &User, name: &str) -> (NodeId, NodeId) {
        let id = NodeId(Uuid::new_v4());
        self.apply(
            admin,
            TreeOp::Create {
                parent: None,
                kind: NodeKind::Site,
                name: name.into(),
                id,
            },
        )
        .await
        .unwrap();
        let library = self.child(id, "Documents").await.expect("no library");
        (id, library)
    }

    async fn create(
        &self,
        actor: &User,
        parent: Option<NodeId>,
        kind: NodeKind,
        name: &str,
    ) -> Result<NodeId, AccessError> {
        let id = NodeId(Uuid::new_v4());
        self.apply(
            actor,
            TreeOp::Create {
                parent,
                kind,
                name: name.into(),
                id,
            },
        )
        .await
        .map(|node| node.id)
    }

    async fn folder(&self, actor: &User, parent: NodeId, name: &str) -> NodeId {
        self.create(actor, Some(parent), NodeKind::Folder, name)
            .await
            .unwrap()
    }

    async fn file(
        &self,
        actor: &User,
        parent: NodeId,
        name: &str,
    ) -> Result<Document, AccessError> {
        tree::create_document(
            &self.state,
            actor,
            parent,
            DocId(Uuid::new_v4()),
            name,
            Editor::GridCraft,
            None,
        )
        .await
    }

    /// The undeleted child of `parent` named `name`, from PostgreSQL.
    async fn child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        self.db()
            .node_rows()
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.parent == Some(parent) && r.name == name && r.deleted_at.is_none())
            .map(|r| r.id)
    }

    async fn grant(&self, admin: &User, node: NodeId, user: &User, role: Role) {
        self.db()
            .set_node_access(admin.id, node, Principal::User(user.id), Some(role.id()))
            .await
            .unwrap();
    }

    /// The number of commits on `main`.
    fn commits(&self) -> usize {
        let repo = gix::open(library(self.dir.path())).unwrap();
        let Ok(tip) = repo.find_reference("refs/heads/main") else {
            return 0;
        };
        repo.rev_walk([tip.id()]).all().unwrap().count()
    }

    /// Git's tree and PostgreSQL's nodes, each as id -> (kind, parent, name, deleted).
    async fn both(&self) -> (Shape, Shape) {
        let git = self
            .store
            .read_tree()
            .unwrap()
            .into_iter()
            .map(|r| (r.id, (r.kind, r.parent, r.name, r.deleted.is_some())))
            .collect();
        let db = self
            .db()
            .node_rows()
            .await
            .unwrap()
            .into_iter()
            .map(|r| {
                let kind = match r.kind {
                    NodeKind::Site => "site",
                    NodeKind::Library => "library",
                    NodeKind::Folder => "folder",
                    NodeKind::Document => "document",
                };
                (
                    r.id.0,
                    (
                        kind.to_owned(),
                        r.parent.map(|p| p.0),
                        r.name,
                        r.deleted_at.is_some(),
                    ),
                )
            })
            .collect();
        (git, db)
    }

    async fn assert_agree(&self) {
        let (git, db) = self.both().await;
        assert_eq!(git, db, "Git and PostgreSQL disagree");
    }

    async fn events(&self, kind: &str) -> Vec<(Option<Uuid>, Option<Uuid>, serde_json::Value)> {
        sqlx::query_as(
            "select node_id, document_id, detail from events where kind = $1 order by id",
        )
        .bind(kind)
        .fetch_all(self.db().test_pool())
        .await
        .unwrap()
    }

    /// Records a document the way the server did before the tree: no node.
    async fn old_document(&self, creator: &User, name: &str, editor: Editor) -> DocId {
        let id = DocId(Uuid::new_v4());
        let stand_in = match editor.native_extension() {
            Some(ext) => format!("stand-in{ext}"),
            None => "stand-in".to_owned(),
        };
        self.db()
            .create_document(creator.id, id, &stand_in, editor)
            .await
            .unwrap();
        // Old names were not checked as strictly, and the order of creation decides who keeps
        // a name.
        sqlx::query(
            "update documents set name = $2, \
             created_at = now() - interval '1 day' + (select count(*) from documents) * interval '1 second' \
             where id = $1",
        )
        .bind(id.0)
        .bind(name)
        .execute(self.db().test_pool())
        .await
        .unwrap();
        id
    }

    async fn old_grant(&self, doc: DocId, user: &User, role: &str) {
        sqlx::query("insert into document_access (document_id, user_id, role) values ($1, $2, $3)")
            .bind(doc.0)
            .bind(user.id.0)
            .bind(role)
            .execute(self.db().test_pool())
            .await
            .unwrap();
    }

    async fn old_grants(&self) -> i64 {
        sqlx::query_scalar("select count(*) from document_access")
            .fetch_one(self.db().test_pool())
            .await
            .unwrap()
    }

    async fn adopt(&self) -> Result<Option<Adopted>, TreeError> {
        tree::adopt_flat_documents(self.checkpointer(), self.db()).await
    }
}

type Shape = BTreeMap<Uuid, (String, Option<Uuid>, String, bool)>;

fn library(data_dir: &Path) -> PathBuf {
    server::library_path(data_dir)
}

fn config(data_dir: &Path, db: &TestDb) -> Config {
    Config {
        addr: "127.0.0.1:0".parse().unwrap(),
        data_dir: data_dir.to_path_buf(),
        web_dir: data_dir.join("web"),
        checkpoint_idle: Duration::from_millis(100),
        checkpoint_max: Duration::from_secs(30),
        room_grace: Duration::from_secs(60),
        write_timeout: Duration::from_secs(10),
        database_url: server::config::DatabaseUrl::new(db.url()),
        editors_dir: data_dir.join("editors"),
        max_file_bytes: 64 << 20,
    }
}

/// The groups of `site`, as the administrator `admin` sees them.
async fn site_groups(env: &Env, admin: &User, site: NodeId) -> Vec<Group> {
    env.db()
        .groups_visible_to(admin.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|g| g.site == Some(site))
        .collect()
}

#[tokio::test]
async fn create_site_makes_documents_library_and_owner() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (site, library) = env.site(&admin, "Head office").await;

    let node = env.db().node(site).await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Site);
    assert_eq!(node.name, "Head office");
    assert!(node.unique_scope);
    let lib = env.db().node(library).await.unwrap().unwrap();
    assert_eq!(lib.kind, NodeKind::Library);
    assert_eq!(lib.parent, Some(site));
    assert_eq!(lib.site, site);
    assert!(!lib.unique_scope);

    // The creator owns the site as the one member of its Owners group, which the site's
    // transaction made with its Members and Visitors groups; nobody has a grant of their own.
    let access = env.db().access(admin.id, site).await.unwrap();
    assert!(access.grants.is_empty());
    let groups = site_groups(&env, &admin, site).await;
    assert_eq!(groups.len(), 3);
    let owners = groups
        .iter()
        .find(|g| g.purpose == Some(SiteGroup::Owners))
        .unwrap();
    let (_, members) = env.db().group(admin.id, owners.id).await.unwrap().unwrap();
    assert_eq!(members.iter().map(|u| u.id).collect::<Vec<_>>(), [admin.id]);

    // One commit holds both records.
    assert_eq!(env.commits(), 1);
    assert_eq!(env.store.read_tree().unwrap().len(), 2);
    assert_eq!(env.events("node.created").await.len(), 2);
    env.assert_agree().await;

    // Site names are unique, ignoring case.
    let again = env
        .create(&admin, None, NodeKind::Site, "HEAD OFFICE")
        .await;
    assert_eq!(again, Err(AccessError::NameTaken));
}

#[tokio::test]
async fn only_administrators_create_sites() {
    let env = Env::new().await;
    let ann = env.user("Ann", false).await;
    let made = env.create(&ann, None, NodeKind::Site, "Mine").await;
    assert_eq!(made, Err(AccessError::Forbidden));
    assert_eq!(env.commits(), 0);
    assert!(env.db().node_rows().await.unwrap().is_empty());

    // A disabled administrator may not either.
    let admin = env.user("Ada", true).await;
    let other = env.user("Odo", true).await;
    sqlx::query("update users set disabled_at = now() where id = $1")
        .bind(admin.id.0)
        .execute(env.db().test_pool())
        .await
        .unwrap();
    assert!(!env.db().user(other.id).await.unwrap().unwrap().disabled);
    let made = env.create(&admin, None, NodeKind::Site, "Mine").await;
    assert_eq!(made, Err(AccessError::Forbidden));
    assert_eq!(env.commits(), 0);
}

#[tokio::test]
async fn library_needs_site_owner() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let bob = env.user("Bob", false).await;
    let (site, _) = env.site(&admin, "Team").await;

    let by_stranger = env
        .create(&bob, Some(site), NodeKind::Library, "Plans")
        .await;
    assert_eq!(by_stranger, Err(AccessError::NotFound));
    env.grant(&admin, site, &ann, Role::Editor).await;
    let by_editor = env
        .create(&ann, Some(site), NodeKind::Library, "Plans")
        .await;
    assert_eq!(by_editor, Err(AccessError::Forbidden));
    env.grant(&admin, site, &ann, Role::Owner).await;
    let library = env
        .create(&ann, Some(site), NodeKind::Library, "Plans")
        .await
        .unwrap();
    assert_eq!(
        env.db().node(library).await.unwrap().unwrap().kind,
        NodeKind::Library
    );
    env.assert_agree().await;
}

#[tokio::test]
async fn folder_needs_editor() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let (_, library) = env.site(&admin, "Team").await;

    assert_eq!(
        env.create(&ann, Some(library), NodeKind::Folder, "Plans")
            .await,
        Err(AccessError::NotFound)
    );
    env.grant(&admin, library, &ann, Role::Viewer).await;
    assert_eq!(
        env.create(&ann, Some(library), NodeKind::Folder, "Plans")
            .await,
        Err(AccessError::Forbidden)
    );
    assert_eq!(
        env.file(&ann, library, "Plan.xlsx").await,
        Err(AccessError::Forbidden)
    );
    env.grant(&admin, library, &ann, Role::Editor).await;
    let folder = env.folder(&ann, library, "Plans").await;
    let file = env.file(&ann, folder, "Plan.xlsx").await.unwrap();
    // The file inherits: no grant of its own, the folder's role.
    assert_eq!(
        Role::of(
            env.db()
                .effective_access(ann.id, file.id.into())
                .await
                .unwrap()
        ),
        Some(Role::Editor)
    );
    let node = env.db().node(file.id.into()).await.unwrap().unwrap();
    assert!(!node.unique_scope);
    let created = env.events("document.created").await;
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].0, Some(file.id.0));
    assert_eq!(created[0].1, Some(file.id.0));
    assert!(
        env.events("node.created")
            .await
            .iter()
            .all(|e| e.0 != Some(file.id.0))
    );
    env.assert_agree().await;
}

#[tokio::test]
async fn wrong_place_refused() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (site, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    let file = env.file(&admin, folder, "Plan.xlsx").await.unwrap();
    let file_node: NodeId = file.id.into();
    let commits = env.commits();

    let cases = [
        (Some(site), NodeKind::Folder),
        (Some(folder), NodeKind::Library),
        (Some(library), NodeKind::Library),
        (Some(file_node), NodeKind::Folder),
        (Some(library), NodeKind::Site),
        (None, NodeKind::Folder),
        (Some(library), NodeKind::Document),
    ];
    for (parent, kind) in cases {
        assert_eq!(
            env.create(&admin, parent, kind, "X").await,
            Err(AccessError::WrongPlace),
            "{kind:?} under {parent:?}"
        );
    }
    assert_eq!(
        env.file(&admin, site, "Other.xlsx").await,
        Err(AccessError::WrongPlace)
    );
    assert_eq!(
        env.file(&admin, file_node, "Other.xlsx").await,
        Err(AccessError::WrongPlace)
    );
    let moved = env
        .apply(
            &admin,
            TreeOp::Move {
                id: folder,
                parent: file_node,
            },
        )
        .await;
    assert_eq!(moved, Err(AccessError::WrongPlace));
    let moved = env
        .apply(
            &admin,
            TreeOp::Move {
                id: library,
                parent: folder,
            },
        )
        .await;
    assert_eq!(moved, Err(AccessError::WrongPlace));
    assert_eq!(env.commits(), commits);
}

#[tokio::test]
async fn rename_writes_one_commit_and_event() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    let file = env.file(&admin, folder, "Plan.xlsx").await.unwrap();
    let node: NodeId = file.id.into();
    let commits = env.commits();

    let renamed = env
        .apply(
            &admin,
            TreeOp::Rename {
                id: node,
                name: "Budget.xlsx".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Budget.xlsx");
    assert_eq!(env.commits(), commits + 1);
    assert_eq!(
        env.store.read_tree_record(file.id.0).unwrap().unwrap().name,
        "Budget.xlsx"
    );
    // The mirror renames the document row too, and the event names the node.
    assert_eq!(
        env.db().document(file.id).await.unwrap().unwrap().name,
        "Budget.xlsx"
    );
    let events = env.events("node.renamed").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, Some(file.id.0));
    assert_eq!(events[0].1, Some(file.id.0));
    assert_eq!(events[0].2["from"], "Plan.xlsx");
    assert_eq!(events[0].2["to"], "Budget.xlsx");

    // The same name again changes nothing; a file keeps its extension.
    env.apply(
        &admin,
        TreeOp::Rename {
            id: node,
            name: "Budget.xlsx".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(env.commits(), commits + 1);
    let wrong = env
        .apply(
            &admin,
            TreeOp::Rename {
                id: node,
                name: "Budget.docx".into(),
            },
        )
        .await;
    assert_eq!(wrong, Err(AccessError::WrongExtension));
    let bad = env
        .apply(
            &admin,
            TreeOp::Rename {
                id: folder,
                name: "a/b".into(),
            },
        )
        .await;
    assert_eq!(bad, Err(AccessError::InvalidName));
    assert_eq!(env.commits(), commits + 1);
    env.assert_agree().await;
}

#[tokio::test]
async fn rename_keeps_document_head() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let file = env.file(&admin, library, "Plan.xlsx").await.unwrap();
    let authors = [Author {
        id: admin.id.0.to_string(),
        name: "Ada".into(),
    }];
    let fields = Map::new();
    let Outcome::Written(first) = env
        .store
        .checkpoint(&Checkpoint {
            doc: file.id,
            file_name: "Plan.xlsx",
            blob: revisions::BlobRef::of(b"one"),
            base: None,
            authors: &authors,
            fields: &fields,
        })
        .unwrap()
    else {
        panic!("expected a new revision");
    };
    env.apply(
        &admin,
        TreeOp::Rename {
            id: file.id.into(),
            name: "Budget.xlsx".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(env.store.head(file.id).unwrap(), Some(first.clone()));
    // A checkpoint based on the head from before the rename is still accepted.
    let next = env
        .store
        .checkpoint(&Checkpoint {
            doc: file.id,
            file_name: "Budget.xlsx",
            blob: revisions::BlobRef::of(b"two"),
            base: Some(&first),
            authors: &authors,
            fields: &fields,
        })
        .unwrap();
    assert!(matches!(next, Outcome::Written(_)));
}

#[tokio::test]
async fn move_needs_editor_both_ends() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let (_, library) = env.site(&admin, "Team").await;
    let from = env.folder(&admin, library, "From").await;
    let to = env.folder(&admin, library, "To").await;
    let file = env.file(&admin, from, "Plan.xlsx").await.unwrap();
    let node: NodeId = file.id.into();
    let move_it = TreeOp::Move {
        id: node,
        parent: to,
    };

    env.grant(&admin, from, &ann, Role::Editor).await;
    assert_eq!(
        env.apply(&ann, move_it.clone()).await,
        Err(AccessError::NotFound)
    );
    env.grant(&admin, to, &ann, Role::Viewer).await;
    assert_eq!(
        env.apply(&ann, move_it.clone()).await,
        Err(AccessError::Forbidden)
    );
    env.grant(&admin, from, &ann, Role::Viewer).await;
    env.grant(&admin, to, &ann, Role::Editor).await;
    assert_eq!(
        env.apply(&ann, move_it.clone()).await,
        Err(AccessError::Forbidden)
    );
    env.grant(&admin, from, &ann, Role::Editor).await;
    let moved = env.apply(&ann, move_it).await.unwrap();
    assert_eq!(moved.parent, Some(to));
    assert_eq!(
        env.store
            .read_tree_record(file.id.0)
            .unwrap()
            .unwrap()
            .parent,
        Some(to.0)
    );
    let events = env.events("node.moved").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, Some(file.id.0));
    env.assert_agree().await;
}

#[tokio::test]
async fn move_into_descendant_refused() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let a = env.folder(&admin, library, "A").await;
    let b = env.folder(&admin, a, "B").await;
    let c = env.folder(&admin, b, "C").await;
    let commits = env.commits();
    for parent in [a, b, c] {
        assert_eq!(
            env.apply(&admin, TreeOp::Move { id: a, parent }).await,
            Err(AccessError::IntoItself)
        );
    }
    assert_eq!(env.commits(), commits);
    // Moving up is fine.
    let moved = env
        .apply(
            &admin,
            TreeOp::Move {
                id: c,
                parent: library,
            },
        )
        .await
        .unwrap();
    assert_eq!(moved.parent, Some(library));
    env.assert_agree().await;
}

#[tokio::test]
async fn crossing_moves_make_no_cycle() {
    for _ in 0..5 {
        let env = Env::new().await;
        let admin = env.user("Ada", true).await;
        let (_, library) = env.site(&admin, "Team").await;
        let a = env.folder(&admin, library, "A").await;
        let b = env.folder(&admin, library, "B").await;
        let (first, second) = tokio::join!(
            env.apply(&admin, TreeOp::Move { id: a, parent: b }),
            env.apply(&admin, TreeOp::Move { id: b, parent: a }),
        );
        let results = [first.map(|_| ()), second.map(|_| ())];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(results.contains(&Err(AccessError::IntoItself)));
        // Both folders still lead up to the site.
        for folder in [a, b] {
            assert_eq!(
                Role::of(env.db().effective_access(admin.id, folder).await.unwrap()),
                Some(Role::Owner)
            );
        }
        env.assert_agree().await;
    }
}

#[tokio::test]
async fn concurrent_same_name_one_wins() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    env.grant(&admin, folder, &ann, Role::Editor).await;

    // Two creates of the same name.
    let (first, second) = tokio::join!(
        env.file(&admin, folder, "Plan.xlsx"),
        env.file(&ann, folder, "plan.XLSX"),
    );
    let results = [first.map(|_| ()), second.map(|_| ())];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(results.contains(&Err(AccessError::NameTaken)));
    env.assert_agree().await;

    // A create racing a rename to the same name.
    let other = env.file(&admin, folder, "Other.xlsx").await.unwrap();
    let (created, renamed) = tokio::join!(
        env.file(&ann, folder, "Report.xlsx"),
        env.apply(
            &admin,
            TreeOp::Rename {
                id: other.id.into(),
                name: "Report.xlsx".into(),
            },
        ),
    );
    let results = [created.map(|_| ()), renamed.map(|_| ())];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(results.contains(&Err(AccessError::NameTaken)));
    env.assert_agree().await;

    // Two moves of different files with one name into one folder.
    let elsewhere = env.folder(&admin, library, "Elsewhere").await;
    let x = env.file(&admin, elsewhere, "Same.xlsx").await.unwrap();
    let y_folder = env.folder(&admin, library, "Other place").await;
    let y = env.file(&admin, y_folder, "SAME.xlsx").await.unwrap();
    let (one, two) = tokio::join!(
        env.apply(
            &admin,
            TreeOp::Move {
                id: x.id.into(),
                parent: folder,
            },
        ),
        env.apply(
            &admin,
            TreeOp::Move {
                id: y.id.into(),
                parent: folder,
            },
        ),
    );
    let results = [one.map(|_| ()), two.map(|_| ())];
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    assert!(results.contains(&Err(AccessError::NameTaken)));
    env.assert_agree().await;
}

#[tokio::test]
async fn delete_non_empty_folder_refused() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (site, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    let file = env.file(&admin, folder, "Plan.xlsx").await.unwrap();
    assert_eq!(
        env.apply(&admin, TreeOp::Delete { id: folder }).await,
        Err(AccessError::NotEmpty)
    );
    assert_eq!(
        env.apply(&admin, TreeOp::Delete { id: library }).await,
        Err(AccessError::WrongPlace)
    );
    assert_eq!(
        env.apply(&admin, TreeOp::Delete { id: site }).await,
        Err(AccessError::IsSite)
    );
    env.apply(&admin, TreeOp::Delete { id: file.id.into() })
        .await
        .unwrap();
    let gone = env
        .apply(&admin, TreeOp::Delete { id: folder })
        .await
        .unwrap();
    assert!(gone.deleted);
    assert_eq!(env.events("node.deleted").await.len(), 2);
    env.assert_agree().await;
}

#[tokio::test]
async fn deleted_file_is_not_found() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let (_, library) = env.site(&admin, "Team").await;
    env.grant(&admin, library, &ann, Role::Editor).await;
    let file = env.file(&ann, library, "Plan.xlsx").await.unwrap();
    let node: NodeId = file.id.into();
    env.apply(&ann, TreeOp::Delete { id: node }).await.unwrap();

    assert_eq!(env.db().effective_access(ann.id, node).await.unwrap(), None);
    assert_eq!(
        env.db().effective_access(admin.id, node).await.unwrap(),
        None
    );
    assert!(
        env.store
            .read_tree_record(file.id.0)
            .unwrap()
            .unwrap()
            .deleted
            .is_some()
    );
    for op in [
        TreeOp::Rename {
            id: node,
            name: "Again.xlsx".into(),
        },
        TreeOp::Delete { id: node },
        TreeOp::Move {
            id: node,
            parent: library,
        },
    ] {
        assert_eq!(env.apply(&admin, op).await, Err(AccessError::NotFound));
    }
    // Its name is free again.
    env.file(&ann, library, "Plan.xlsx").await.unwrap();
    env.assert_agree().await;
}

#[tokio::test]
async fn reconcile_restores_missing_rows() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    let file = env.file(&admin, folder, "Plan.xlsx").await.unwrap();
    let before = env.both().await.1;

    let report = tree::reconcile(env.checkpointer(), env.db()).await.unwrap();
    assert_eq!((report.nodes, report.updated), (4, 0));

    sqlx::query("delete from nodes where id = any($1)")
        .bind(vec![file.id.0, folder.0])
        .execute(env.db().test_pool())
        .await
        .unwrap();
    let report = tree::reconcile(env.checkpointer(), env.db()).await.unwrap();
    assert_eq!((report.nodes, report.updated), (4, 2));
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(env.both().await.1, before);
    let repaired: Vec<_> = env
        .events("node.created")
        .await
        .into_iter()
        .filter(|e| e.2["repaired"] == true)
        .map(|e| e.0.unwrap())
        .collect();
    assert_eq!(repaired, vec![folder.0, file.id.0], "parents first");
    assert_eq!(
        Role::of(
            env.db()
                .effective_access(admin.id, file.id.into())
                .await
                .unwrap()
        ),
        Some(Role::Owner)
    );
}

#[tokio::test]
async fn reconcile_fixes_changed_names() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    let file = env.file(&admin, folder, "Plan.xlsx").await.unwrap();
    sqlx::query("update nodes set name = 'Wrong' where id = $1")
        .bind(folder.0)
        .execute(env.db().test_pool())
        .await
        .unwrap();
    sqlx::query("update nodes set name = 'Wrong.xlsx' where id = $1")
        .bind(file.id.0)
        .execute(env.db().test_pool())
        .await
        .unwrap();
    sqlx::query("update documents set name = 'Wrong.xlsx' where id = $1")
        .bind(file.id.0)
        .execute(env.db().test_pool())
        .await
        .unwrap();

    let report = tree::reconcile(env.checkpointer(), env.db()).await.unwrap();
    assert_eq!(report.updated, 2);
    assert!(report.failed.is_empty());
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");
    assert_eq!(
        env.db().document(file.id).await.unwrap().unwrap().name,
        "Plan.xlsx"
    );
    let events = env.events("node.renamed").await;
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.2["repaired"] == true));
    env.assert_agree().await;
}

/// Writes `records` at `tree/<id>.json`, where servers before the sub-folders wrote them: one
/// commit on `main`, which the tree ref follows.
fn write_flat(env: &Env, records: &[TreeRecord]) {
    let repo = gix::open(library(env.dir.path())).unwrap();
    let parent = repo
        .find_reference("refs/heads/main")
        .ok()
        .map(|mut r| r.peel_to_id().unwrap().detach());
    let root = match parent {
        Some(p) => repo.find_commit(p).unwrap().tree_id().unwrap().detach(),
        None => gix::ObjectId::empty_tree(repo.object_hash()),
    };
    let mut editor = repo.edit_tree(root).unwrap();
    for record in records {
        let mut bytes = serde_json::to_vec_pretty(record).unwrap();
        bytes.push(b'\n');
        let blob = repo.write_blob(&bytes).unwrap().detach();
        editor
            .upsert(
                format!("tree/{}.json", record.id),
                gix::object::tree::EntryKind::Blob,
                blob,
            )
            .unwrap();
    }
    let tree = editor.write().unwrap().detach();
    let signature = gix::actor::Signature {
        name: "Old server".into(),
        email: "old@sharecraft.invalid".into(),
        time: gix::date::Time::now_local_or_utc(),
    };
    let commit = gix::objs::Commit {
        tree,
        parents: parent.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: "Old layout\n".into(),
        extra_headers: Vec::new(),
    };
    let oid = repo.write_object(&commit).unwrap().detach();
    for name in ["refs/heads/main", "refs/sharecraft/tree"] {
        repo.reference(
            name,
            oid,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .unwrap();
    }
}

/// The message and author name of the commit at the tip of `main`.
fn tip_commit(env: &Env) -> (String, String) {
    let repo = gix::open(library(env.dir.path())).unwrap();
    let tip = repo
        .find_reference("refs/heads/main")
        .unwrap()
        .id()
        .detach();
    let commit = repo.find_commit(tip).unwrap();
    (
        String::from_utf8_lossy(commit.message_raw().unwrap()).into_owned(),
        commit.author().unwrap().name.to_string(),
    )
}

#[tokio::test]
async fn old_layout_is_moved_before_the_reconcile() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (site, library) = env.site(&admin, "Team").await;
    let folder = Uuid::new_v4();
    let stamp = Stamp {
        at: "2026-11-12T09:00:00Z".into(),
        by: Author {
            id: admin.id.0.to_string(),
            name: "Ada".into(),
        },
    };
    let old = TreeRecord {
        schema: 1,
        id: folder,
        kind: "folder".into(),
        parent: Some(library.0),
        site: site.0,
        name: "Old plans".into(),
        created: stamp,
        deleted: None,
    };
    write_flat(&env, std::slice::from_ref(&old));
    let commits = env.commits();

    let moved = tree::move_records(env.checkpointer()).await.unwrap();
    assert_eq!(
        moved.0,
        TreeMove {
            moved: 1,
            duplicates: 0
        }
    );
    assert_eq!(moved.to_string(), "1 records moved into sub-folders");
    assert_eq!(env.commits(), commits + 1, "one commit");
    assert_eq!(
        tip_commit(&env),
        (
            "tree: move records into sub-folders\n".to_owned(),
            "ShareCraft".to_owned()
        )
    );
    let report = tree::reconcile(env.checkpointer(), env.db()).await.unwrap();
    assert_eq!((report.nodes, report.updated), (3, 1));
    assert_eq!(env.child(library, "Old plans").await, Some(NodeId(folder)));
    env.assert_agree().await;

    // A second start moves nothing and writes nothing.
    let again = tree::move_records(env.checkpointer()).await.unwrap();
    assert_eq!(again.0, TreeMove::default());
    assert_eq!(again.to_string(), "no records to move into sub-folders");
    assert_eq!(env.commits(), commits + 1);
}

#[tokio::test]
async fn adopt_moves_documents_and_grants() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let second = env.user("Abe", true).await;
    let ann = env.user("Ann", false).await;
    let bob = env.user("Bob", false).await;
    let shared = env.old_document(&ann, "Shared.txt", Editor::Text).await;
    let lonely = env
        .old_document(&ann, "Lonely.xlsx", Editor::GridCraft)
        .await;
    env.old_grant(shared, &ann, "owner").await;
    env.old_grant(shared, &bob, "viewer").await;

    let adopted = env.adopt().await.unwrap().unwrap();
    assert_eq!((adopted.documents, adopted.renamed), (2, 0));
    assert_eq!((adopted.scopes, adopted.grants), (1, 2));
    let site = adopted.site.unwrap();
    let library = adopted.library.unwrap();
    let site_node = env.db().node(site).await.unwrap().unwrap();
    assert_eq!(
        (site_node.kind, site_node.name.as_str()),
        (NodeKind::Site, "Home")
    );
    let lib_node = env.db().node(library).await.unwrap().unwrap();
    assert_eq!(lib_node.name, "Documents");
    assert_eq!(lib_node.parent, Some(site));
    for doc in [shared, lonely] {
        let node = env.db().node(doc.into()).await.unwrap().unwrap();
        assert_eq!(node.parent, Some(library));
    }

    let db = env.db();
    let role =
        |user: UserId, doc: DocId| async move { Role::of(db.doc_access(user, doc).await.unwrap()) };
    assert_eq!(role(ann.id, shared).await, Some(Role::Owner));
    assert_eq!(role(bob.id, shared).await, Some(Role::Viewer));
    assert_eq!(role(ann.id, lonely).await, None);
    assert_eq!(role(admin.id, lonely).await, Some(Role::Owner));
    assert_eq!(env.old_grants().await, 0);
    assert!(
        env.db()
            .node(shared.into())
            .await
            .unwrap()
            .unwrap()
            .unique_scope
    );

    // The site has its three groups, with nobody in them and nobody assigned on it: enabled
    // administrators (who have every permission everywhere) fill them.
    assert!(
        env.db()
            .access(admin.id, site)
            .await
            .unwrap()
            .grants
            .is_empty()
    );
    let groups = site_groups(&env, &admin, site).await;
    assert_eq!(groups.len(), 3);
    assert!(groups.iter().all(|g| g.member_count == 0));
    assert_eq!(
        Role::of(env.db().effective_access(second.id, site).await.unwrap()),
        Some(Role::Owner)
    );

    let events = env.events("tree.adopted").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].2["documents"], 2);
    // One commit, credited to ShareCraft.
    assert_eq!(env.commits(), 1);
    let repo = gix::open(library_path(&env)).unwrap();
    let tip = repo.find_reference("refs/heads/main").unwrap().id();
    let commit = repo.find_commit(tip).unwrap();
    assert_eq!(commit.author().unwrap().name, "ShareCraft");
    env.assert_agree().await;
}

fn library_path(env: &Env) -> PathBuf {
    library(env.dir.path())
}

#[tokio::test]
async fn adopt_renames_duplicates_and_refused_characters() {
    let env = Env::new().await;
    env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let a = env.old_document(&ann, "a.xlsx", Editor::GridCraft).await;
    let upper = env.old_document(&ann, "A.xlsx", Editor::GridCraft).await;
    let colon = env.old_document(&ann, "x:y.docx", Editor::WordCraft).await;

    let adopted = env.adopt().await.unwrap().unwrap();
    assert_eq!((adopted.documents, adopted.renamed), (3, 2));
    for (doc, name) in [(a, "a.xlsx"), (upper, "A (2).xlsx"), (colon, "x_y.docx")] {
        assert_eq!(env.db().document(doc).await.unwrap().unwrap().name, name);
        assert_eq!(env.db().node(doc.into()).await.unwrap().unwrap().name, name);
        assert_eq!(
            env.store.read_tree_record(doc.0).unwrap().unwrap().name,
            name
        );
    }
    env.assert_agree().await;
}

#[tokio::test]
async fn adopt_twice_does_nothing() {
    let env = Env::new().await;
    env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let doc = env.old_document(&ann, "Notes.txt", Editor::Text).await;
    env.old_grant(doc, &ann, "owner").await;
    assert!(env.adopt().await.unwrap().is_some());
    let commits = env.commits();
    let shape = env.both().await;
    assert_eq!(env.adopt().await.unwrap(), None);
    assert_eq!(env.commits(), commits);
    assert_eq!(env.both().await, shape);
    assert_eq!(env.events("tree.adopted").await.len(), 1);
}

#[tokio::test]
async fn adopt_after_crash_between_git_and_database() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let ann = env.user("Ann", false).await;
    let bob = env.user("Bob", false).await;
    let first = env.old_document(&ann, "Plan.xlsx", Editor::GridCraft).await;
    let second = env.old_document(&bob, "plan.xlsx", Editor::GridCraft).await;
    env.old_grant(first, &ann, "owner").await;
    env.old_grant(first, &bob, "editor").await;

    // The database refuses the adoption's transaction, as if the server stopped after the
    // commit.
    let pool = env.db().test_pool();
    sqlx::raw_sql(
        "create function refuse_adoption() returns trigger language plpgsql as $$ \
         begin if new.kind = 'tree.adopted' then raise exception 'crash'; end if; \
         return new; end $$; \
         create trigger refuse_adoption before insert on events \
         for each row execute function refuse_adoption();",
    )
    .execute(pool)
    .await
    .unwrap();
    let crashed = env.adopt().await;
    assert!(matches!(crashed, Err(TreeError::Db(_))), "{crashed:?}");
    assert_eq!(env.commits(), 1);
    assert!(env.db().node_rows().await.unwrap().is_empty());
    assert_eq!(env.old_grants().await, 2);
    sqlx::raw_sql("drop trigger refuse_adoption on events; drop function refuse_adoption();")
        .execute(pool)
        .await
        .unwrap();

    // The next start: reconcile, then adopt.
    let report = tree::reconcile(env.checkpointer(), env.db()).await.unwrap();
    assert_eq!((report.nodes, report.updated), (4, 4));
    assert_eq!(
        env.db().document(second).await.unwrap().unwrap().name,
        "plan (2).xlsx"
    );
    let adopted = env.adopt().await.unwrap().unwrap();
    assert_eq!(adopted.documents, 0);
    assert_eq!(adopted.grants, 2);
    let site = adopted.site.unwrap();
    assert_eq!(env.db().node(site).await.unwrap().unwrap().name, "Home");
    assert_eq!(env.commits(), 1);
    assert_eq!(env.old_grants().await, 0);
    assert_eq!(
        Role::of(env.db().doc_access(bob.id, first).await.unwrap()),
        Some(Role::Editor)
    );
    assert_eq!(env.db().doc_access(bob.id, second).await.unwrap(), None);
    assert!(
        env.db()
            .access(admin.id, site)
            .await
            .unwrap()
            .grants
            .is_empty()
    );
    assert_eq!(site_groups(&env, &admin, site).await.len(), 3);
    assert_eq!(env.adopt().await.unwrap(), None);
    env.assert_agree().await;

    // A document made the old way after that goes into the same Home library.
    let late = env.old_document(&ann, "PLAN.xlsx", Editor::GridCraft).await;
    let adopted = env.adopt().await.unwrap().unwrap();
    assert_eq!(adopted.site, Some(site));
    assert_eq!(
        env.db().document(late).await.unwrap().unwrap().name,
        "PLAN (3).xlsx"
    );
    env.assert_agree().await;
}

#[tokio::test]
async fn adopt_without_administrator_stops_with_message() {
    let env = Env::new().await;
    let ann = env.user("Ann", false).await;
    let doc = env.old_document(&ann, "Notes.txt", Editor::Text).await;
    env.old_grant(doc, &ann, "owner").await;
    let admin = env.user("Ada", true).await;
    // A disabled administrator does not count.
    sqlx::query("update users set disabled_at = now() where id = $1")
        .bind(admin.id.0)
        .execute(env.db().test_pool())
        .await
        .unwrap();

    let error = env.adopt().await.unwrap_err();
    assert!(matches!(error, TreeError::NoAdministrator));
    assert!(
        error
            .to_string()
            .contains("sharecraft-server add-user --admin"),
        "{error}"
    );
    assert_eq!(env.commits(), 0);
    assert!(env.db().node_rows().await.unwrap().is_empty());
    assert_eq!(env.old_grants().await, 1);
}

/// A store that keeps no tree, as a test double might.
struct NoTree;

impl Checkpointer for NoTree {
    fn write(&self, _c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        Err(CheckpointError::Unsupported)
    }

    fn load(&self, _doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        Ok(None)
    }

    fn load_at(
        &self,
        _doc: DocId,
        _revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        Ok(None)
    }

    fn describe(
        &self,
        _doc: DocId,
        _revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        Ok(None)
    }

    fn history(&self, _doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        Ok(Vec::new())
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn store_without_tree_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let test = TestDb::create().await;
    let blobs = revisions::BlobStore::open(&server::blobs_path(dir.path())).unwrap();
    let state = AppState::new(
        config(dir.path(), &test),
        Arc::new(NoTree),
        blobs,
        test.db.clone(),
    );
    let password = Uuid::new_v4().simple().to_string();
    let admin = test
        .db
        .create_user(
            None,
            NewUser {
                email: "ada@test.invalid",
                name: "Ada",
                password: &password,
                is_admin: true,
            },
        )
        .await
        .unwrap();
    let made = tree::apply(
        &state,
        &admin,
        TreeOp::Create {
            parent: None,
            kind: NodeKind::Site,
            name: "Team".into(),
            id: NodeId(Uuid::new_v4()),
        },
    )
    .await;
    match made {
        Err(AccessError::Db(e)) => assert!(e.to_string().contains("does not support"), "{e}"),
        other => panic!("expected a plain error, got {other:?}"),
    }
    let reconciled = tree::reconcile(Arc::new(NoTree), &test.db).await;
    assert!(
        matches!(
            reconciled,
            Err(TreeError::Git(CheckpointError::Unsupported))
        ),
        "{reconciled:?}"
    );
}

/// Makes the database refuse every event of `kind` (and, with `repairs`, the repairs too),
/// as a failing database would, after the Git commit is written.
async fn refuse_events(env: &Env, kind: &str, repairs: bool) {
    let condition = if repairs {
        format!("new.kind = '{kind}'")
    } else {
        format!("new.kind = '{kind}' and not (new.detail ? 'repaired')")
    };
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "create function refuse_events() returns trigger language plpgsql as $$ \
         begin if {condition} then raise exception 'database down'; end if; \
         return new; end $$; \
         create trigger refuse_events before insert on events \
         for each row execute function refuse_events();"
    )))
    .execute(env.db().test_pool())
    .await
    .unwrap();
}

async fn accept_events(env: &Env) {
    sqlx::raw_sql("drop trigger refuse_events on events; drop function refuse_events();")
        .execute(env.db().test_pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn database_failure_after_commit_is_repaired_from_git() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    refuse_events(&env, "node.renamed", false).await;
    let renamed = env
        .apply(
            &admin,
            TreeOp::Rename {
                id: folder,
                name: "Budgets".into(),
            },
        )
        .await;
    assert!(matches!(renamed, Err(AccessError::Db(_))), "{renamed:?}");
    // Git holds the change, and the repair from Git put it into PostgreSQL.
    assert_eq!(
        env.db().node(folder).await.unwrap().unwrap().name,
        "Budgets"
    );
    let repaired = env.events("node.renamed").await;
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired[0].2["repaired"], true);
    env.assert_agree().await;
    // The tree is in step: the next change goes through.
    accept_events(&env).await;
    env.folder(&admin, library, "Next").await;
    env.assert_agree().await;
}

#[tokio::test]
async fn failed_repair_marks_the_tree_out_of_step_until_reconciled() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    refuse_events(&env, "node.renamed", true).await;
    let renamed = env
        .apply(
            &admin,
            TreeOp::Rename {
                id: folder,
                name: "Budgets".into(),
            },
        )
        .await;
    assert!(matches!(renamed, Err(AccessError::Db(_))), "{renamed:?}");
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");
    let commits = env.commits();

    // Every tree change is refused while the reconcile still fails, and writes nothing.
    assert_eq!(
        env.create(&admin, Some(library), NodeKind::Folder, "Other")
            .await,
        Err(AccessError::OutOfStep)
    );
    assert_eq!(
        env.file(&admin, library, "Plan.xlsx").await,
        Err(AccessError::OutOfStep)
    );
    assert_eq!(env.commits(), commits);

    // Once the database works, the next change reconciles the whole tree first.
    accept_events(&env).await;
    env.folder(&admin, library, "Other").await;
    assert_eq!(
        env.db().node(folder).await.unwrap().unwrap().name,
        "Budgets"
    );
    env.assert_agree().await;
    env.folder(&admin, library, "Third").await;
}

#[tokio::test]
async fn refused_after_commit_is_taken_back_in_git() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let xan = env.user("Xan", false).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    env.grant(&admin, folder, &xan, Role::Editor).await;
    let commits = env.commits();

    // Hold the folder's row, so the change is committed to Git and then waits to be judged.
    let mut gate = sqlx::PgConnection::connect(&env.test.url()).await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut gate)
        .await
        .unwrap();
    sqlx::query("begin").execute(&mut gate).await.unwrap();
    sqlx::query("select id from nodes where id = $1 for update")
        .bind(folder.0)
        .execute(&mut gate)
        .await
        .unwrap();
    let state = env.state.clone();
    let actor = xan.clone();
    let renaming = tokio::spawn(async move {
        tree::apply(
            &state,
            &actor,
            TreeOp::Rename {
                id: folder,
                name: "Mine".into(),
            },
        )
        .await
    });
    let mut blocked = 0i64;
    for _ in 0..400 {
        blocked = sqlx::query_scalar(
            "select count(*) from pg_stat_activity where $1 = any(pg_blocking_pids(pid))",
        )
        .bind(pid)
        .fetch_one(env.db().test_pool())
        .await
        .unwrap();
        if blocked > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(blocked, 1, "the rename never waited to be judged");
    assert_eq!(env.commits(), commits + 1);
    // Meanwhile Xan becomes a viewer.
    sqlx::query(
        "update assignments set role_id = 'viewer' \
         where node_id = $1 and principal_kind = 'user' and principal_id = $2",
    )
    .bind(folder.0)
    .bind(xan.id.0)
    .execute(&mut gate)
    .await
    .unwrap();
    sqlx::query("commit").execute(&mut gate).await.unwrap();

    assert_eq!(renaming.await.unwrap(), Err(AccessError::Forbidden));
    // A second commit put the previous record back.
    assert_eq!(env.commits(), commits + 2);
    assert_eq!(
        env.store.read_tree_record(folder.0).unwrap().unwrap().name,
        "Plans"
    );
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");
    assert!(env.events("node.renamed").await.is_empty());
    env.assert_agree().await;
    // Nothing is out of step.
    env.folder(&admin, library, "Next").await;
}

/// A library store whose take-backs (the "Undo: …" commits) fail while `failing` is above
/// zero, one failure each time.
struct FlakyUndo {
    store: Arc<RevisionStore>,
    failing: Arc<AtomicUsize>,
}

impl Checkpointer for FlakyUndo {
    fn write(&self, c: &Checkpoint<'_>) -> Result<Outcome, CheckpointError> {
        Checkpointer::write(&*self.store, c)
    }

    fn load(&self, doc: DocId) -> Result<Option<Stored>, CheckpointError> {
        Checkpointer::load(&*self.store, doc)
    }

    fn load_at(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<Stored>, CheckpointError> {
        Checkpointer::load_at(&*self.store, doc, revision)
    }

    fn describe(
        &self,
        doc: DocId,
        revision: &RevisionId,
    ) -> Result<Option<RevisionInfo>, CheckpointError> {
        Checkpointer::describe(&*self.store, doc, revision)
    }

    fn history(&self, doc: DocId) -> Result<Vec<RevisionInfo>, CheckpointError> {
        Checkpointer::history(&*self.store, doc)
    }

    fn documents(&self) -> Result<Vec<DocId>, GitError> {
        Checkpointer::documents(&*self.store)
    }

    fn write_tree(&self, change: &TreeChange<'_>) -> Result<RevisionId, CheckpointError> {
        let fail = change.message.starts_with("Undo: ")
            && self
                .failing
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
        if fail {
            return Err(CheckpointError::Git(GitError::new("disk full")));
        }
        Checkpointer::write_tree(&*self.store, change)
    }

    fn read_tree(&self) -> Result<Vec<TreeRecord>, CheckpointError> {
        Checkpointer::read_tree(&*self.store)
    }

    fn read_tree_record(&self, id: Uuid) -> Result<Option<TreeRecord>, CheckpointError> {
        Checkpointer::read_tree_record(&*self.store, id)
    }
}

/// Makes PostgreSQL refuse the name `name` for any node as already taken, as a name taken
/// outside the server's lock would be.
async fn refuse_name(env: &Env, name: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "create function refuse_name() returns trigger language plpgsql as $$ \
         begin if new.name = '{name}' then \
         raise exception using errcode = 'unique_violation', message = 'name taken'; end if; \
         return new; end $$; \
         create trigger refuse_name before insert or update on nodes \
         for each row execute function refuse_name();"
    )))
    .execute(env.db().test_pool())
    .await
    .unwrap();
}

async fn accept_names(env: &Env) {
    sqlx::raw_sql("drop trigger refuse_name on nodes; drop function refuse_name();")
        .execute(env.db().test_pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_take_back_is_written_before_the_tree_is_reconciled() {
    let failing = Arc::new(AtomicUsize::new(0));
    let flaky = Arc::clone(&failing);
    let env = Env::with(move |store| {
        Arc::new(FlakyUndo {
            store,
            failing: flaky,
        })
    })
    .await;
    let admin = env.user("Ada", true).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    refuse_name(&env, "Mine").await;
    failing.store(2, Ordering::SeqCst);

    let renamed = env
        .apply(
            &admin,
            TreeOp::Rename {
                id: folder,
                name: "Mine".into(),
            },
        )
        .await;
    assert_eq!(renamed, Err(AccessError::NameTaken));
    // The refused change is in Git, and could not be taken back yet.
    assert_eq!(
        env.store.read_tree_record(folder.0).unwrap().unwrap().name,
        "Mine"
    );
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");

    // From here PostgreSQL would accept the refused name, so a reconcile alone would copy it.
    accept_names(&env).await;
    // The take-back fails again: changes stay refused, and nothing is reconciled.
    assert_eq!(
        env.create(&admin, Some(library), NodeKind::Folder, "Other")
            .await,
        Err(AccessError::OutOfStep)
    );
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");

    // Now the take-back is written first, then the tree reconciled: the refused change never
    // reaches PostgreSQL.
    env.folder(&admin, library, "Other").await;
    assert_eq!(
        env.store.read_tree_record(folder.0).unwrap().unwrap().name,
        "Plans"
    );
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");
    assert!(env.events("node.renamed").await.is_empty());
    env.assert_agree().await;
}

#[tokio::test]
async fn change_that_could_not_be_judged_is_taken_back_not_repaired() {
    let env = Env::new().await;
    let admin = env.user("Ada", true).await;
    let xan = env.user("Xan", false).await;
    let (_, library) = env.site(&admin, "Team").await;
    let folder = env.folder(&admin, library, "Plans").await;
    env.grant(&admin, folder, &xan, Role::Editor).await;
    let commits = env.commits();

    // Hold the folder's row, so the change is committed to Git and then waits to be judged.
    let mut gate = sqlx::PgConnection::connect(&env.test.url()).await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut gate)
        .await
        .unwrap();
    sqlx::query("begin").execute(&mut gate).await.unwrap();
    sqlx::query("select id from nodes where id = $1 for update")
        .bind(folder.0)
        .execute(&mut gate)
        .await
        .unwrap();
    let state = env.state.clone();
    let actor = xan.clone();
    let renaming = tokio::spawn(async move {
        tree::apply(
            &state,
            &actor,
            TreeOp::Rename {
                id: folder,
                name: "Mine".into(),
            },
        )
        .await
    });
    let mut waiting: Option<i32> = None;
    for _ in 0..400 {
        waiting = sqlx::query_scalar(
            "select pid from pg_stat_activity where $1 = any(pg_blocking_pids(pid))",
        )
        .bind(pid)
        .fetch_optional(env.db().test_pool())
        .await
        .unwrap();
        if waiting.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let waiting = waiting.expect("the rename never waited to be judged");
    assert_eq!(env.commits(), commits + 1);
    // The connection judging it is lost while it locks.
    let ended: bool = sqlx::query_scalar("select pg_terminate_backend($1)")
        .bind(waiting)
        .fetch_one(env.db().test_pool())
        .await
        .unwrap();
    assert!(ended);

    let renamed = renaming.await.unwrap();
    assert!(
        matches!(renamed, Err(AccessError::Unjudged(_))),
        "{renamed:?}"
    );
    sqlx::query("commit").execute(&mut gate).await.unwrap();
    // Taken back in Git, and not repaired into PostgreSQL from Git.
    assert_eq!(env.commits(), commits + 2);
    assert_eq!(
        env.store.read_tree_record(folder.0).unwrap().unwrap().name,
        "Plans"
    );
    assert_eq!(env.db().node(folder).await.unwrap().unwrap().name, "Plans");
    assert!(env.events("node.renamed").await.is_empty());
    env.assert_agree().await;
    // Nothing is out of step.
    env.folder(&admin, library, "Next").await;
}
