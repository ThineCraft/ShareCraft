//! Keeping a connected library in step with a branch of a GitHub repository.

use std::collections::{HashMap, HashSet};

use records::{Editor, SourceItem, validate_document_name, validate_node_name};

use crate::github::{EntryKind, TreeEntry};
use crate::tree::unique_node_name;

/// One item of the repository as the library shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub sha: String,
    pub size: u64,
    pub dir: bool,
}

/// What a sync changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub add: Vec<Entry>,
    pub change: Vec<(SourceItem, Entry)>,
    pub remove: Vec<SourceItem>,
    pub moves: Vec<(SourceItem, Entry)>,
    pub unchanged: usize,
}

/// What the repository holds that the library does not show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hidden {
    pub symlinks: usize,
    pub submodules: usize,
    pub policy: usize,
    pub too_big: usize,
    pub unnameable: usize,
    pub renamed: usize,
}

impl Hidden {
    /// How many files are not shown at all (`renamed` files are shown, so not counted).
    #[must_use]
    pub fn total(&self) -> usize {
        self.symlinks
            .saturating_add(self.submodules)
            .saturating_add(self.policy)
            .saturating_add(self.too_big)
            .saturating_add(self.unnameable)
    }
}

/// The difference between what the library holds (`known`) and what it should show (`wanted`):
///
/// - a file at the same path with the same blob is unchanged; with another blob it is a change
///   (a new version of the same document);
/// - a file whose path vanished and a new file with the same blob, exactly one of each and opened
///   by the same editor, is a move (the same node renamed or re-parented);
/// - folders are matched by path alone (a folder is never moved: its files are);
/// - everything else is added or removed.
///
/// `unchanged` counts files. `add` and `remove` come in path order, so a folder comes before
/// what is in it.
#[must_use]
pub fn plan(wanted: &[Entry], known: &[SourceItem]) -> Plan {
    let known_at: HashMap<&str, &SourceItem> =
        known.iter().map(|item| (item.path.as_str(), item)).collect();
    let wanted_paths: HashSet<&str> = wanted.iter().map(|e| e.path.as_str()).collect();
    let mut plan = Plan::default();
    let mut new = Vec::new();
    for entry in wanted {
        match known_at.get(entry.path.as_str()) {
            None => new.push(entry),
            Some(item) => match (&item.blob_sha, entry.dir) {
                (None, true) => {}
                (Some(sha), false) if *sha == entry.sha => plan.unchanged += 1,
                (Some(_), false) => plan.change.push(((*item).clone(), entry.clone())),
                // A folder that became a file, or the other way round.
                _ => {
                    plan.remove.push((*item).clone());
                    new.push(entry);
                }
            },
        }
    }
    let vanished: Vec<&SourceItem> = known
        .iter()
        .filter(|item| !wanted_paths.contains(item.path.as_str()))
        .collect();

    let mut new_by_blob: HashMap<&str, usize> = HashMap::new();
    for entry in new.iter().filter(|e| !e.dir) {
        *new_by_blob.entry(entry.sha.as_str()).or_default() += 1;
    }
    let mut gone_by_blob: HashMap<&str, usize> = HashMap::new();
    for item in &vanished {
        if let Some(sha) = &item.blob_sha {
            *gone_by_blob.entry(sha.as_str()).or_default() += 1;
        }
    }
    let single = |counts: &HashMap<&str, usize>, sha: &str| counts.get(sha) == Some(&1);
    let mut moved_blobs = HashSet::new();
    for item in &vanished {
        let Some(sha) = item.blob_sha.as_deref() else {
            continue;
        };
        if !single(&gone_by_blob, sha) || !single(&new_by_blob, sha) {
            continue;
        }
        let Some(entry) = new.iter().find(|e| !e.dir && e.sha == sha) else {
            continue;
        };
        if Editor::for_file_name(last_name(&item.path)) != Editor::for_file_name(last_name(&entry.path))
        {
            continue;
        }
        plan.moves.push(((*item).clone(), (*entry).clone()));
        moved_blobs.insert(sha.to_owned());
    }
    plan.add = new
        .into_iter()
        .filter(|e| e.dir || !moved_blobs.contains(&e.sha))
        .cloned()
        .collect();
    plan.remove.extend(
        vanished
            .into_iter()
            .filter(|item| {
                item.blob_sha
                    .as_ref()
                    .is_none_or(|sha| !moved_blobs.contains(sha))
            })
            .cloned(),
    );
    plan.add.sort_by(|a, b| a.path.cmp(&b.path));
    plan.remove.sort_by(|a, b| a.path.cmp(&b.path));
    plan
}

/// The last name of a `/`-separated path.
fn last_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `parent/name`, or `name` at the top.
fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// Whether ShareCraft takes `name` as it is for a folder (`file` false) or a file: no empty,
/// `.` or `..` name, no control, invisible or refused character, no name over the length
/// limit or ending in a dot, nothing trimmed, and for a file a name its editor accepts.
fn nameable(name: &str, file: bool) -> bool {
    if validate_node_name(name) != Ok(name) {
        return false;
    }
    !file || validate_document_name(name, Editor::for_file_name(name)).is_ok()
}

/// What of the tree `entries` the library shows, below `folder` (`/`-separated, outer slashes
/// ignored; empty for the whole repository), with paths relative to it, and what it hides.
///
/// Symbolic links and submodules are hidden and counted, never shown as files. A file with a
/// name ShareCraft refuses anywhere in its path (see [`nameable`]) is hidden and counted as
/// unnameable. Siblings whose names are equal ignoring case keep the first in byte order; the
/// others are shown as `name (2).ext`, `name (3).ext` … and counted as renamed. Folders are
/// shown only when they hold a shown file. Policy and size are judged by the caller.
///
/// The result is in path order, each folder before what is in it.
#[must_use]
pub fn shown_names(entries: Vec<TreeEntry>, folder: &str) -> (Vec<Entry>, Hidden) {
    let folder = folder.trim_matches('/');
    let mut hidden = Hidden::default();
    let mut files: Vec<(String, String, u64)> = Vec::new();
    for entry in entries {
        let relative = if folder.is_empty() {
            Some(entry.path.as_str())
        } else {
            entry
                .path
                .strip_prefix(folder)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        let Some(relative) = relative.filter(|r| !r.is_empty()) else {
            continue;
        };
        match entry.kind {
            EntryKind::Symlink => hidden.symlinks += 1,
            EntryKind::Submodule => hidden.submodules += 1,
            EntryKind::Dir => {}
            EntryKind::File => {
                let mut parts = relative.split('/').rev();
                let file_ok = parts.next().is_some_and(|name| nameable(name, true));
                if file_ok && parts.all(|name| nameable(name, false)) {
                    files.push((relative.to_owned(), entry.sha, entry.size));
                } else {
                    hidden.unnameable += 1;
                }
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    // Shown path of each folder, by its path in the repository; lower-cased names taken in
    // each shown folder.
    let mut folders: HashMap<String, String> = HashMap::new();
    let mut taken: HashMap<String, HashSet<String>> = HashMap::new();
    let mut shown = Vec::new();
    for (path, sha, size) in files {
        let mut parts: Vec<&str> = path.split('/').collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let mut original = String::new();
        let mut parent = String::new();
        for part in parts {
            original = join(&original, part);
            if let Some(known) = folders.get(&original) {
                parent = known.clone();
                continue;
            }
            let names = taken.entry(parent.clone()).or_default();
            let given = unique_node_name(part, None, names);
            if given != part {
                hidden.renamed += 1;
            }
            names.insert(given.to_lowercase());
            let at = join(&parent, &given);
            folders.insert(original.clone(), at.clone());
            shown.push(Entry {
                path: at.clone(),
                sha: String::new(),
                size: 0,
                dir: true,
            });
            parent = at;
        }
        let names = taken.entry(parent.clone()).or_default();
        let given = unique_node_name(name, Some(Editor::for_file_name(name)), names);
        if given != name {
            hidden.renamed += 1;
        }
        names.insert(given.to_lowercase());
        shown.push(Entry {
            path: join(&parent, &given),
            sha,
            size,
            dir: false,
        });
    }
    shown.sort_by(|a, b| a.path.cmp(&b.path));
    (shown, hidden)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use collab_core::ids::NodeId;
    use uuid::Uuid;

    use super::*;
    use crate::github::EntryKind;

    fn file(path: &str, sha: &str) -> Entry {
        Entry {
            path: path.to_owned(),
            sha: sha.to_owned(),
            size: 1,
            dir: false,
        }
    }

    fn known(path: &str, sha: &str) -> SourceItem {
        SourceItem {
            node: NodeId(Uuid::new_v4()),
            path: path.to_owned(),
            blob_sha: Some(sha.to_owned()),
            size: 1,
        }
    }

    fn tree(path: &str, kind: EntryKind, sha: &str) -> TreeEntry {
        TreeEntry {
            path: path.to_owned(),
            kind,
            sha: sha.to_owned(),
            size: 3,
        }
    }

    fn paths(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn file_paths(entries: &[Entry]) -> Vec<&str> {
        entries
            .iter()
            .filter(|e| !e.dir)
            .map(|e| e.path.as_str())
            .collect()
    }

    #[test]
    fn unchanged_changed_added_removed() {
        let same = known("same.md", "a1");
        let edited = known("edited.md", "b1");
        let gone = known("gone.md", "c1");
        let wanted = [
            file("same.md", "a1"),
            file("edited.md", "b2"),
            file("new.md", "d1"),
        ];
        let plan = plan(&wanted, &[same, edited.clone(), gone.clone()]);
        assert_eq!(plan.unchanged, 1);
        assert_eq!(plan.change, vec![(edited, file("edited.md", "b2"))]);
        assert_eq!(plan.add, vec![file("new.md", "d1")]);
        assert_eq!(plan.remove, vec![gone]);
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn a_rename_with_the_same_blob_is_a_move() {
        let old = known("docs/old.md", "a1");
        let wanted = [file("guide/new.md", "a1")];
        let plan = plan(&wanted, std::slice::from_ref(&old));
        assert_eq!(plan.moves, vec![(old, file("guide/new.md", "a1"))]);
        assert!(plan.add.is_empty());
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn two_files_with_one_blob_are_not_mistaken_for_a_move() {
        // One vanished, two new with its blob: which one moved cannot be told.
        let old = known("old.md", "a1");
        let wanted = [file("one.md", "a1"), file("two.md", "a1")];
        let plan = plan(&wanted, std::slice::from_ref(&old));
        assert!(plan.moves.is_empty());
        assert_eq!(paths(&plan.add), ["one.md", "two.md"]);
        assert_eq!(plan.remove, vec![old]);

        // Two vanished with one blob, one new: the same.
        let (a, b) = (known("a.md", "x1"), known("b.md", "x1"));
        let plan = super::plan(&[file("c.md", "x1")], &[a.clone(), b.clone()]);
        assert!(plan.moves.is_empty());
        assert_eq!(paths(&plan.add), ["c.md"]);
        assert_eq!(plan.remove, vec![a, b]);
    }

    #[test]
    fn a_move_to_another_kind_of_file_is_a_remove_and_an_add() {
        // The same bytes under an extension another editor opens: not a rename.
        let old = known("report.md", "a1");
        let plan = plan(&[file("report.docx", "a1")], std::slice::from_ref(&old));
        assert!(plan.moves.is_empty());
        assert_eq!(plan.remove, vec![old]);
        assert_eq!(paths(&plan.add), ["report.docx"]);
    }

    #[test]
    fn folders_are_matched_by_path() {
        let folder = SourceItem {
            node: NodeId(Uuid::new_v4()),
            path: "docs".to_owned(),
            blob_sha: None,
            size: 0,
        };
        let wanted = [Entry {
            path: "docs".to_owned(),
            sha: String::new(),
            size: 0,
            dir: true,
        }];
        let plan = plan(&wanted, std::slice::from_ref(&folder));
        assert!(plan.add.is_empty() && plan.remove.is_empty() && plan.change.is_empty());
        let gone = super::plan(&[], std::slice::from_ref(&folder));
        assert_eq!(gone.remove, vec![folder]);
    }

    #[test]
    fn case_collisions_keep_the_first_and_suffix_the_rest() {
        let (shown, hidden) = shown_names(
            vec![
                tree("a.md", EntryKind::File, "s2"),
                tree("A.md", EntryKind::File, "s1"),
                tree("Docs/x.md", EntryKind::File, "s3"),
                tree("docs/y.md", EntryKind::File, "s4"),
            ],
            "",
        );
        assert_eq!(
            paths(&shown),
            ["A.md", "Docs", "Docs/x.md", "a (2).md", "docs (2)", "docs (2)/y.md"]
        );
        let lower = shown.iter().find(|e| e.path == "a (2).md").unwrap();
        assert_eq!(lower.sha, "s2");
        assert_eq!(hidden.renamed, 2);
        assert_eq!(hidden.unnameable, 0);
    }

    #[test]
    fn a_file_named_a_dot_dot_is_unnameable() {
        let (shown, hidden) = shown_names(
            vec![
                tree("..", EntryKind::File, "s1"),
                tree("docs/../x.md", EntryKind::File, "s2"),
                tree("./y.md", EntryKind::File, "s3"),
                tree("ends-in-a-dot.", EntryKind::File, "s4"),
                tree("ok.md", EntryKind::File, "s5"),
            ],
            "",
        );
        assert_eq!(paths(&shown), ["ok.md"]);
        assert_eq!(hidden.unnameable, 4);
    }

    #[test]
    fn symlinks_and_submodules_are_hidden_and_counted() {
        let (shown, hidden) = shown_names(
            vec![
                tree("docs", EntryKind::Dir, "t1"),
                tree("docs/link.md", EntryKind::Symlink, "s1"),
                tree("vendor", EntryKind::Submodule, "c1"),
                tree("docs/real.md", EntryKind::File, "s2"),
            ],
            "",
        );
        assert_eq!(paths(&shown), ["docs", "docs/real.md"]);
        assert_eq!(hidden.symlinks, 1);
        assert_eq!(hidden.submodules, 1);
    }

    #[test]
    fn folder_prefix_is_stripped_and_outside_files_are_ignored() {
        let (shown, hidden) = shown_names(
            vec![
                tree("README.md", EntryKind::File, "s1"),
                tree("docs", EntryKind::Dir, "t1"),
                tree("docs/guide.md", EntryKind::File, "s2"),
                tree("docs/api/index.md", EntryKind::File, "s3"),
                tree("docsite/other.md", EntryKind::File, "s4"),
                tree("docs/only-a-link", EntryKind::Symlink, "s5"),
                tree("elsewhere/link", EntryKind::Symlink, "s6"),
            ],
            "/docs/",
        );
        assert_eq!(paths(&shown), ["api", "api/index.md", "guide.md"]);
        assert!(shown.iter().find(|e| e.path == "api").unwrap().dir);
        assert_eq!(file_paths(&shown), ["api/index.md", "guide.md"]);
        // What is outside the folder is not counted either.
        assert_eq!(hidden.symlinks, 1);
    }

    #[test]
    fn empty_names_and_names_over_255_are_unnameable() {
        let long = "x".repeat(300);
        let (shown, hidden) = shown_names(
            vec![
                tree("docs//a.md", EntryKind::File, "s1"),
                tree(&format!("{long}.md"), EntryKind::File, "s2"),
                tree(&format!("{long}/a.md"), EntryKind::File, "s3"),
                tree("tab\there.md", EntryKind::File, "s4"),
                tree("fine.md", EntryKind::File, "s5"),
            ],
            "",
        );
        assert_eq!(paths(&shown), ["fine.md"]);
        assert_eq!(hidden.unnameable, 4);
    }
}
