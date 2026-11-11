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
use std::time::Duration;

use collab_core::ids::{DocId, NodeId, UserId};
use records::test_support::TestDb;
use records::{AccessError, Db, Document, Editor, NewUser, Node, NodeKind, Role, User};
use revisions::{
    Author, Checkpoint, CheckpointError, GitError, Outcome, RevisionId, RevisionInfo,
    RevisionStore, Stored,
};
use serde_json::Map;
use server::tree::{self, Adopted, TreeError, TreeOp};
use server::{AppState, Checkpointer, Config};
use tempfile::TempDir;
use uuid::Uuid;

/// A server state on a temporary library and a test schema, with nothing served.
struct Env {
    dir: TempDir,
    test: TestDb,
    store: Arc<RevisionStore>,
    state: AppState,
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let test = TestDb::create().await;
        let store = Arc::new(RevisionStore::open(&library(dir.path())).unwrap());
        let checkpointer: Arc<dyn Checkpointer> = store.clone();
        let state = AppState::new(config(dir.path(), &test), checkpointer, test.db.clone());
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
            .set_node_access(admin.id, node, user.id, Some(role))
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

    let access = env.db().access(admin.id, site).await.unwrap();
    assert_eq!(access.grants.len(), 1);
    assert_eq!(access.grants[0].user.id, admin.id);
    assert_eq!(access.grants[0].role, Role::Owner);

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
        env.db()
            .effective_role(ann.id, file.id.into())
            .await
            .unwrap(),
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
            bytes: b"one",
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
            bytes: b"two",
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
                env.db().effective_role(admin.id, folder).await.unwrap(),
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

    assert_eq!(env.db().effective_role(ann.id, node).await.unwrap(), None);
    assert_eq!(env.db().effective_role(admin.id, node).await.unwrap(), None);
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
        env.db()
            .effective_role(admin.id, file.id.into())
            .await
            .unwrap(),
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
    let role = |user: UserId, doc: DocId| async move { db.role(user, doc).await.unwrap() };
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

    // Every enabled administrator owns the site.
    let owners: Vec<UserId> = env
        .db()
        .access(admin.id, site)
        .await
        .unwrap()
        .grants
        .into_iter()
        .filter(|g| g.role == Role::Owner)
        .map(|g| g.user.id)
        .collect();
    assert_eq!(owners.len(), 2);
    assert!(owners.contains(&admin.id) && owners.contains(&second.id));

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
        env.db().role(bob.id, first).await.unwrap(),
        Some(Role::Editor)
    );
    assert_eq!(env.db().role(bob.id, second).await.unwrap(), None);
    let owners = env.db().access(admin.id, site).await.unwrap().grants;
    assert_eq!(owners.len(), 1);
    assert_eq!((owners[0].user.id, owners[0].role), (admin.id, Role::Owner));
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
    let state = AppState::new(config(dir.path(), &test), Arc::new(NoTree), test.db.clone());
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
