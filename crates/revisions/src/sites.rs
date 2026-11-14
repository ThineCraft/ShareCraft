//! Site records, saved site templates and the organisation's global navigation, recorded one
//! JSON file each beside the content tree.
//!
//! A site's record lives at `sites/<first two hex digits of the id>/<id>.json` beside its tree
//! record, a saved template at `templates/<xx>/<id>.json`, and the global navigation at
//! `org/navigation.json`. `refs/sharecraft/sites` names the last commit that changed any of
//! them. A change that also changes the tree (creating a site, renaming one) writes both in one
//! commit, which moves `refs/sharecraft/tree` as well.

use gix::object::tree::EntryKind;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::meta::Author;
use crate::store::{
    RevisionStore, SITES_REF, TREE_REF, commit_on_main_moving, corrupt, main_tip, read_blob,
    ref_target, signature, valid_author,
};
use crate::tree::{Stamp, TreeRecord, flat_path, is_shard_name, record_path, shard};
use crate::{CheckpointError, GitError, RevisionId};

/// The format version of a [`SiteRecord`], [`TemplateRecord`] and [`OrgNavigation`]; currently 1.
pub const SITE_SCHEMA: u32 = 1;
/// The values a site's `kind` may have.
pub const SITE_KINDS: [&str; 2] = ["team", "communication"];
/// The longest description of a site or template, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 500;
/// The longest name of a saved template, in characters.
pub const MAX_TEMPLATE_NAME_CHARS: usize = 80;
/// The folder of the site records in a commit.
const SITES_DIR: &str = "sites";
/// The folder of the saved templates in a commit.
const TEMPLATES_DIR: &str = "templates";
/// Where the organisation's global navigation is recorded.
const ORG_NAVIGATION_PATH: &str = "org/navigation.json";

/// Where a navigation link leads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum NavTarget {
    /// A node of the content tree: `{"node": id}`.
    Node(Uuid),
    /// An address: `{"url": text}`, `https:`, `http:`, `mailto:` or a path on this server.
    Url(String),
}

/// One link or heading of a navigation. Field order here is the file's field order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavItem {
    /// The item's lasting identifier.
    pub id: Uuid,
    /// What the item shows. Empty only for a node target, which then shows the node's current
    /// name.
    pub label: String,
    /// Where the item leads; `None` for a heading.
    pub target: Option<NavTarget>,
    /// The groups the item is shown to; empty for everyone. Only hides a link, never grants or
    /// refuses access.
    #[serde(default)]
    pub audience: Vec<Uuid>,
    /// The items below this one: one level only.
    #[serde(default)]
    pub children: Vec<NavItem>,
}

/// What a hub carries: the navigation bar its member sites share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubRecord {
    /// The hub's navigation bar.
    pub navigation: Vec<NavItem>,
}

/// Contents of `sites/<xx>/<id>.json`: a site's settings beside its tree record. Field order
/// here is the file's field order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteRecord {
    /// Format version; currently 1.
    pub schema: u32,
    /// The site's node id.
    pub id: Uuid,
    /// `team` or `communication`.
    pub kind: String,
    /// What the site is for; 0 to 500 characters.
    pub description: String,
    /// A BCP 47 language tag, used for dates and numbers on its pages.
    pub language: String,
    /// An IANA time zone name, used for dates and times on its pages.
    pub time_zone: String,
    /// The template it was created from: a saved template's id or a built-in name.
    pub template: Option<String>,
    /// When and by whom it was archived, if it is.
    pub archived: Option<Stamp>,
    /// Its hub settings, if it is a hub.
    pub hub: Option<HubRecord>,
    /// The hub it has joined, if any.
    pub joined_hub: Option<Uuid>,
    /// Its own navigation; `None` for automatic (every library, by name).
    pub navigation: Option<Vec<NavItem>>,
}

impl SiteRecord {
    /// Why the record cannot be stored or was stored wrongly, if so.
    fn problem(&self) -> Option<String> {
        if self.schema != SITE_SCHEMA {
            return Some(format!("unknown schema {}", self.schema));
        }
        if !SITE_KINDS.contains(&self.kind.as_str()) {
            return Some(format!("unknown site kind {:?}", self.kind));
        }
        if self.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Some("a description longer than 500 characters".to_owned());
        }
        None
    }
}

/// Where a template's navigation link leads: libraries by name, never identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum TemplateNavTarget {
    /// The template's library of that name: `{"library": name}`.
    Library(String),
    /// An address: `{"url": text}`.
    Url(String),
}

/// One link or heading of a template's navigation: a [`NavItem`] without identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateNavItem {
    /// What the item shows; empty only for a library target.
    pub label: String,
    /// Where the item leads; `None` for a heading.
    pub target: Option<TemplateNavTarget>,
    /// The site groups the item is shown to, by purpose: `owners`, `members` or `visitors`.
    #[serde(default)]
    pub audience: Vec<String>,
    /// The items below this one: one level only.
    #[serde(default)]
    pub children: Vec<TemplateNavItem>,
}

/// A folder a template makes, with the folders inside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateFolder {
    /// The folder's name.
    pub name: String,
    /// The folders inside it.
    #[serde(default)]
    pub folders: Vec<TemplateFolder>,
}

/// A library a template makes, with its folders.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateLibrary {
    /// The library's name.
    pub name: String,
    /// The folders inside it.
    #[serde(default)]
    pub folders: Vec<TemplateFolder>,
}

/// The shape of a site a template makes: settings, libraries, folders and navigation. Never
/// files, members or identifiers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateBody {
    /// `team` or `communication`.
    pub kind: String,
    /// A BCP 47 language tag.
    pub language: String,
    /// An IANA time zone name.
    pub time_zone: String,
    /// The libraries the site gets.
    pub libraries: Vec<TemplateLibrary>,
    /// The site's navigation; `None` for automatic.
    pub navigation: Option<Vec<TemplateNavItem>>,
}

/// Contents of `templates/<xx>/<id>.json`: a template saved from a site or imported. Field
/// order here is the file's field order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateRecord {
    /// Format version; currently 1.
    pub schema: u32,
    /// The template's lasting identifier.
    pub id: Uuid,
    /// Its name; 1 to 80 characters.
    pub name: String,
    /// What it is for; 0 to 500 characters.
    pub description: String,
    /// When and by whom it was saved.
    pub created: Stamp,
    /// When and by whom it was deleted, if it was.
    pub deleted: Option<Stamp>,
    /// The shape of the site it makes.
    pub body: TemplateBody,
}

impl TemplateRecord {
    /// Why the record cannot be stored or was stored wrongly, if so.
    fn problem(&self) -> Option<String> {
        if self.schema != SITE_SCHEMA {
            return Some(format!("unknown schema {}", self.schema));
        }
        let name = self.name.chars().count();
        if !(1..=MAX_TEMPLATE_NAME_CHARS).contains(&name) {
            return Some("a name that is not 1 to 80 characters".to_owned());
        }
        if self.description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Some("a description longer than 500 characters".to_owned());
        }
        if !SITE_KINDS.contains(&self.body.kind.as_str()) {
            return Some(format!("unknown site kind {:?}", self.body.kind));
        }
        None
    }
}

/// Contents of `org/navigation.json`: the global navigation every page shows. Field order here
/// is the file's field order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgNavigation {
    /// Format version; currently 1.
    pub schema: u32,
    /// The items, in order.
    pub navigation: Vec<NavItem>,
}

impl OrgNavigation {
    /// Why the record cannot be stored or was stored wrongly, if so.
    fn problem(&self) -> Option<String> {
        (self.schema != SITE_SCHEMA).then(|| format!("unknown schema {}", self.schema))
    }
}

/// Pretty-printed with two-space indent and a trailing newline.
fn file_bytes(value: &impl Serialize) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Where the record of site `id` is written: `sites/<xx>/<id>.json`.
fn site_path(id: Uuid) -> String {
    format!("{SITES_DIR}/{}/{id}.json", shard(id))
}

/// Where the saved template `id` is written: `templates/<xx>/<id>.json`.
fn template_path(id: Uuid) -> String {
    format!("{TEMPLATES_DIR}/{}/{id}.json", shard(id))
}

/// One change to the site records, saved templates or global navigation, with any tree
/// records that go with it, stored as one commit.
#[derive(Clone, Debug)]
pub struct SitesChange<'a> {
    /// Who made the change; the commit author.
    pub author: &'a Author,
    /// The commit message.
    pub message: &'a str,
    /// Tree records written in the same commit (see [`RevisionStore::write_tree`]).
    pub tree: &'a [TreeRecord],
    /// Site records to write, each replacing any stored for the same site.
    pub sites: &'a [SiteRecord],
    /// Sites whose record is removed: a creation taken back.
    pub removed_sites: &'a [Uuid],
    /// Saved templates to write, each replacing any stored under the same id.
    pub templates: &'a [TemplateRecord],
    /// Saved templates whose record is removed: a save or import taken back.
    pub removed_templates: &'a [Uuid],
    /// The global navigation, if it changes.
    pub org_navigation: Option<&'a OrgNavigation>,
}

impl<'a> SitesChange<'a> {
    /// A change by `author` described by `message` that writes nothing yet.
    #[must_use]
    pub fn new(author: &'a Author, message: &'a str) -> Self {
        SitesChange {
            author,
            message,
            tree: &[],
            sites: &[],
            removed_sites: &[],
            templates: &[],
            removed_templates: &[],
            org_navigation: None,
        }
    }

    /// Whether the change writes nothing of sites, templates or the global navigation.
    fn is_empty(&self) -> bool {
        self.sites.is_empty()
            && self.removed_sites.is_empty()
            && self.templates.is_empty()
            && self.removed_templates.is_empty()
            && self.org_navigation.is_none()
    }
}

/// Parses `bytes`, stored at `path` in `commit`, as a `T`.
fn parse<T: DeserializeOwned>(
    commit: &gix::Commit<'_>,
    path: &str,
    bytes: &[u8],
) -> Result<T, CheckpointError> {
    serde_json::from_slice(bytes)
        .map_err(|e| corrupt(commit, format!("{path} cannot be read: {e}")))
}

/// Parses the site record at `path`, which must name the site the file is called after.
fn parse_site(
    commit: &gix::Commit<'_>,
    path: &str,
    bytes: &[u8],
) -> Result<SiteRecord, CheckpointError> {
    let record: SiteRecord = parse(commit, path, bytes)?;
    if let Some(problem) = record.problem() {
        return Err(corrupt(commit, format!("{path}: {problem}")));
    }
    if site_path(record.id) != path {
        return Err(corrupt(
            commit,
            format!("{path} holds the site {}", record.id),
        ));
    }
    Ok(record)
}

/// Parses the saved template at `path`, which must be the template the file is called after.
fn parse_template(
    commit: &gix::Commit<'_>,
    path: &str,
    bytes: &[u8],
) -> Result<TemplateRecord, CheckpointError> {
    let record: TemplateRecord = parse(commit, path, bytes)?;
    if let Some(problem) = record.problem() {
        return Err(corrupt(commit, format!("{path}: {problem}")));
    }
    if template_path(record.id) != path {
        return Err(corrupt(
            commit,
            format!("{path} holds the template {}", record.id),
        ));
    }
    Ok(record)
}

/// Every file in the sub-folders of `dir` on `main`'s tip `commit`, parsed by `read`.
fn read_sharded<T>(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    dir: &str,
    read: impl Fn(&gix::Commit<'_>, &str, &[u8]) -> Result<T, CheckpointError>,
) -> Result<Vec<T>, CheckpointError> {
    let root = commit.tree()?;
    let Some(entry) = root.lookup_entry_by_path(dir)? else {
        return Ok(Vec::new());
    };
    let folder = entry.object().map_err(GitError::new)?.into_tree();
    let mut found = Vec::new();
    for entry in folder.iter() {
        let entry = entry.map_err(GitError::new)?;
        if !(entry.mode().is_tree() && is_shard_name(entry.filename())) {
            continue;
        }
        let name = entry.filename().to_string();
        let sub = repo
            .find_object(entry.oid())
            .map_err(GitError::new)?
            .into_tree();
        for file in sub.iter() {
            let file = file.map_err(GitError::new)?;
            if !file.mode().is_blob() {
                continue;
            }
            let path = format!("{dir}/{name}/{}", file.filename());
            let object = repo.find_object(file.oid()).map_err(GitError::new)?;
            found.push(read(commit, &path, &object.data)?);
        }
    }
    Ok(found)
}

impl RevisionStore {
    /// Records one change to the site records, saved templates or global navigation, with the
    /// tree records that go with it, as one commit on `main` credited to the change's author.
    /// Points `refs/sharecraft/sites` at it, and `refs/sharecraft/tree` too when it writes tree
    /// records. No document ref moves.
    ///
    /// # Errors
    /// `InvalidTreeChange` if it writes nothing of sites, templates or the global navigation,
    /// or a record is not valid; `InvalidAuthor` for a bad author; otherwise Git or encoding
    /// errors.
    pub fn write_sites(&self, change: &SitesChange<'_>) -> Result<RevisionId, CheckpointError> {
        if change.is_empty() {
            return Err(CheckpointError::InvalidTreeChange(
                "a sites change needs at least one site, template or navigation record".into(),
            ));
        }
        if !valid_author(change.author) {
            return Err(CheckpointError::InvalidAuthor);
        }
        let mut files = Vec::new();
        let mut flat = Vec::new();
        for record in change.tree {
            if let Some(problem) = record.problem() {
                return Err(CheckpointError::InvalidTreeChange(problem));
            }
            files.push((record_path(record.id), record.to_file_bytes()?));
            flat.push(flat_path(record.id));
        }
        for record in change.sites {
            if let Some(problem) = record.problem() {
                return Err(CheckpointError::InvalidTreeChange(problem));
            }
            files.push((site_path(record.id), file_bytes(record)?));
        }
        for record in change.templates {
            if let Some(problem) = record.problem() {
                return Err(CheckpointError::InvalidTreeChange(problem));
            }
            files.push((template_path(record.id), file_bytes(record)?));
        }
        if let Some(navigation) = change.org_navigation {
            if let Some(problem) = navigation.problem() {
                return Err(CheckpointError::InvalidTreeChange(problem));
            }
            files.push((ORG_NAVIGATION_PATH.to_owned(), file_bytes(navigation)?));
        }
        let removed: Vec<String> = change
            .removed_sites
            .iter()
            .map(|id| site_path(*id))
            .chain(change.removed_templates.iter().map(|id| template_path(*id)))
            .collect();

        let _guard = self.lock();
        let repo = self.repo()?;
        let parent = main_tip(&repo)?;
        let mut message = change.message.to_owned();
        if !message.ends_with('\n') {
            message.push('\n');
        }
        let mut refs = vec![(SITES_REF, ref_target(&repo, SITES_REF)?)];
        if !change.tree.is_empty() {
            refs.push((TREE_REF, ref_target(&repo, TREE_REF)?));
        }
        let oid = commit_on_main_moving(
            &repo,
            parent.as_ref(),
            &signature(change.author),
            message,
            &refs,
            |editor| {
                for path in removed {
                    editor.remove(path.as_str())?;
                }
                for path in flat {
                    editor.remove(path.as_str())?;
                }
                for (path, bytes) in files {
                    let blob = repo.write_blob(&bytes)?.detach();
                    editor.upsert(path, EntryKind::Blob, blob)?;
                }
                Ok(())
            },
        )?;
        Ok(RevisionId::from_oid(oid))
    }

    /// Every site record on `main`, in no particular order; none if there are none.
    ///
    /// # Errors
    /// `Corrupt`, naming `main`'s commit and the file, if a record cannot be read, has an
    /// unknown schema or kind, or is stored under another site's name; otherwise the Git error.
    pub fn read_sites(&self) -> Result<Vec<SiteRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(Vec::new());
        };
        read_sharded(&repo, &commit, SITES_DIR, parse_site)
    }

    /// The record of site `id` on `main`; `None` if it has none (a site created before site
    /// records, or no such site).
    ///
    /// # Errors
    /// `Corrupt` if the record cannot be read, otherwise the Git error.
    pub fn read_site_record(&self, id: Uuid) -> Result<Option<SiteRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(None);
        };
        let path = site_path(id);
        read_blob(&commit.tree()?, &path)?
            .map(|bytes| parse_site(&commit, &path, &bytes))
            .transpose()
    }

    /// Every saved template on `main`, deleted ones included, in no particular order.
    ///
    /// # Errors
    /// `Corrupt` if a template cannot be read, otherwise the Git error.
    pub fn read_templates(&self) -> Result<Vec<TemplateRecord>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(Vec::new());
        };
        read_sharded(&repo, &commit, TEMPLATES_DIR, parse_template)
    }

    /// The global navigation on `main`; `None` if none was ever recorded.
    ///
    /// # Errors
    /// `Corrupt` if it cannot be read, otherwise the Git error.
    pub fn read_org_navigation(&self) -> Result<Option<OrgNavigation>, CheckpointError> {
        let repo = self.repo()?;
        let Some(commit) = main_tip(&repo)? else {
            return Ok(None);
        };
        let Some(bytes) = read_blob(&commit.tree()?, ORG_NAVIGATION_PATH)? else {
            return Ok(None);
        };
        let navigation: OrgNavigation = parse(&commit, ORG_NAVIGATION_PATH, &bytes)?;
        if let Some(problem) = navigation.problem() {
            return Err(corrupt(
                &commit,
                format!("{ORG_NAVIGATION_PATH}: {problem}"),
            ));
        }
        Ok(Some(navigation))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::store::MAIN;

    fn ann() -> Author {
        Author {
            id: "u1".into(),
            name: "Ann".into(),
        }
    }

    fn stamp() -> Stamp {
        Stamp {
            at: "2026-11-14T09:30:00Z".into(),
            by: ann(),
        }
    }

    fn uuid(text: &str) -> Uuid {
        Uuid::parse_str(text).unwrap()
    }

    fn store() -> (tempfile::TempDir, RevisionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = RevisionStore::open(&dir.path().join("lib.git")).unwrap();
        (dir, store)
    }

    fn site(id: Uuid) -> SiteRecord {
        SiteRecord {
            schema: 1,
            id,
            kind: "team".into(),
            description: String::new(),
            language: "en".into(),
            time_zone: "UTC".into(),
            template: None,
            archived: None,
            hub: None,
            joined_hub: None,
            navigation: None,
        }
    }

    fn tree_site(id: Uuid) -> TreeRecord {
        TreeRecord {
            schema: 1,
            id,
            kind: "site".into(),
            parent: None,
            site: id,
            name: "Team".into(),
            created: stamp(),
            deleted: None,
            bin_stage: None,
            deleted_with: None,
            purged: None,
        }
    }

    #[test]
    fn site_record_round_trip_exact_bytes() {
        let id = uuid("ab111111-1111-4111-8111-111111111111");
        let library = uuid("22222222-2222-4222-8222-222222222222");
        let item = uuid("33333333-3333-4333-8333-333333333333");
        let group = uuid("44444444-4444-4444-8444-444444444444");
        let mut record = site(id);
        record.kind = "communication".into();
        record.description = "News for everyone".into();
        record.language = "en-GB".into();
        record.time_zone = "Europe/London".into();
        record.template = Some("communication".into());
        record.navigation = Some(vec![NavItem {
            id: item,
            label: String::new(),
            target: Some(NavTarget::Node(library)),
            audience: vec![group],
            children: Vec::new(),
        }]);
        let text = String::from_utf8(file_bytes(&record).unwrap()).unwrap();
        let expected = r#"{
  "schema": 1,
  "id": "ab111111-1111-4111-8111-111111111111",
  "kind": "communication",
  "description": "News for everyone",
  "language": "en-GB",
  "time_zone": "Europe/London",
  "template": "communication",
  "archived": null,
  "hub": null,
  "joined_hub": null,
  "navigation": [
    {
      "id": "33333333-3333-4333-8333-333333333333",
      "label": "",
      "target": {
        "node": "22222222-2222-4222-8222-222222222222"
      },
      "audience": [
        "44444444-4444-4444-8444-444444444444"
      ],
      "children": []
    }
  ]
}
"#;
        assert_eq!(text, expected);
        assert_eq!(serde_json::from_str::<SiteRecord>(&text).unwrap(), record);

        // Stored and read back, byte for byte, at its place.
        let (_dir, store) = store();
        let author = ann();
        let mut change = SitesChange::new(&author, "Change site");
        let records = [record.clone()];
        change.sites = &records;
        store.write_sites(&change).unwrap();
        assert_eq!(store.read_site_record(id).unwrap(), Some(record.clone()));
        assert_eq!(store.read_sites().unwrap(), vec![record]);
        let repo = store.repo().unwrap();
        let tip = main_tip(&repo).unwrap().unwrap();
        let bytes = read_blob(&tip.tree().unwrap(), &format!("sites/ab/{id}.json"))
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    }

    #[test]
    fn tree_and_site_records_in_one_commit() {
        let (_dir, store) = store();
        let id = Uuid::new_v4();
        let author = ann();
        let tree = [tree_site(id)];
        let sites = [site(id)];
        let mut change = SitesChange::new(&author, "Create site Team");
        change.tree = &tree;
        change.sites = &sites;
        let rev = store.write_sites(&change).unwrap();
        let repo = store.repo().unwrap();
        let oid = rev.to_oid().unwrap();
        for name in [MAIN, TREE_REF, SITES_REF] {
            assert_eq!(ref_target(&repo, name).unwrap(), Some(oid), "{name}");
        }
        let commit = repo.find_commit(oid).unwrap();
        assert_eq!(commit.parent_ids().count(), 0);
        assert_eq!(store.read_tree_record(id).unwrap(), Some(tree[0].clone()));
        assert_eq!(store.read_site_record(id).unwrap(), Some(sites[0].clone()));

        // A change of the site alone leaves the tree ref where it was.
        let mut changed = sites[0].clone();
        changed.description = "Ours".into();
        let changed = [changed];
        let mut change = SitesChange::new(&author, "Change site Team");
        change.sites = &changed;
        let second = store.write_sites(&change).unwrap();
        let repo = store.repo().unwrap();
        assert_eq!(ref_target(&repo, TREE_REF).unwrap(), Some(oid));
        assert_eq!(
            ref_target(&repo, SITES_REF).unwrap(),
            Some(second.to_oid().unwrap())
        );

        // A removal takes the record away.
        let removed = [id];
        let mut change = SitesChange::new(&author, "Undo: Create site Team");
        change.removed_sites = &removed;
        store.write_sites(&change).unwrap();
        assert_eq!(store.read_site_record(id).unwrap(), None);
        assert!(store.read_sites().unwrap().is_empty());
    }

    #[test]
    fn templates_and_global_navigation_round_trip() {
        let (_dir, store) = store();
        assert_eq!(store.read_org_navigation().unwrap(), None);
        assert!(store.read_templates().unwrap().is_empty());
        let template = TemplateRecord {
            schema: 1,
            id: Uuid::new_v4(),
            name: "Project".into(),
            description: String::new(),
            created: stamp(),
            deleted: None,
            body: TemplateBody {
                kind: "team".into(),
                language: "en".into(),
                time_zone: "UTC".into(),
                libraries: vec![TemplateLibrary {
                    name: "Documents".into(),
                    folders: vec![TemplateFolder {
                        name: "Plans".into(),
                        folders: Vec::new(),
                    }],
                }],
                navigation: Some(vec![TemplateNavItem {
                    label: "Files".into(),
                    target: Some(TemplateNavTarget::Library("Documents".into())),
                    audience: vec!["members".into()],
                    children: Vec::new(),
                }]),
            },
        };
        let navigation = OrgNavigation {
            schema: 1,
            navigation: vec![NavItem {
                id: Uuid::new_v4(),
                label: "Intranet".into(),
                target: Some(NavTarget::Url("https://example.invalid/".into())),
                audience: Vec::new(),
                children: Vec::new(),
            }],
        };
        let author = ann();
        let templates = [template.clone()];
        let mut change = SitesChange::new(&author, "Save template Project");
        change.templates = &templates;
        change.org_navigation = Some(&navigation);
        store.write_sites(&change).unwrap();
        assert_eq!(store.read_templates().unwrap(), vec![template.clone()]);
        assert_eq!(store.read_org_navigation().unwrap(), Some(navigation));

        // A removal takes a template away.
        let removed = [template.id];
        let mut change = SitesChange::new(&author, "Undo: Save template Project");
        change.removed_templates = &removed;
        store.write_sites(&change).unwrap();
        assert!(store.read_templates().unwrap().is_empty());

        // Unknown fields in a template are refused.
        let text = r#"{"kind": "team", "language": "en", "time_zone": "UTC",
            "libraries": [], "navigation": null, "files": []}"#;
        assert!(serde_json::from_str::<TemplateBody>(text).is_err());
    }

    #[test]
    fn empty_or_bad_change_writes_nothing() {
        let (_dir, store) = store();
        let author = ann();
        let tree = [tree_site(Uuid::new_v4())];
        let mut change = SitesChange::new(&author, "Nothing");
        change.tree = &tree;
        assert!(matches!(
            store.write_sites(&change),
            Err(CheckpointError::InvalidTreeChange(_))
        ));
        let mut bad = site(Uuid::new_v4());
        bad.kind = "wiki".into();
        let bad = [bad];
        let mut change = SitesChange::new(&author, "Bad");
        change.sites = &bad;
        assert!(matches!(
            store.write_sites(&change),
            Err(CheckpointError::InvalidTreeChange(_))
        ));
        assert!(ref_target(&store.repo().unwrap(), MAIN).unwrap().is_none());
    }
}
