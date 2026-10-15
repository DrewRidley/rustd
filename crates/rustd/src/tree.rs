//! Finding source files that were not there when the workspace was loaded, and losing the
//! ones that are gone.
//!
//! A directory's timestamp changes when a file is added to it or taken out of it, so one
//! `stat` per directory tells whether any has to be listed again.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use ide_db::{
    RootDatabase,
    base_db::{SourceDatabase, SourceRoot, SourceRootId, salsa::Durability},
};
use rustc_hash::{FxHashMap, FxHashSet};
use triomphe::Arc;
use vfs::{AbsPathBuf, FileId, Vfs, VfsPath, file_set::FileSet};

/// The directories of the source files that can change.
pub(crate) struct Tree {
    vfs: Vfs,
    directories: FxHashMap<PathBuf, Directory>,
}

struct Directory {
    /// The source root its files belong to.
    root: SourceRootId,
    /// When it was last listed, or `None` if it has not been.
    listed: Option<SystemTime>,
}

/// Files that joined or left the source roots.
#[derive(Default)]
pub(crate) struct Moved {
    pub(crate) added: Vec<(FileId, PathBuf)>,
}

impl Tree {
    pub(crate) fn new(db: &RootDatabase, vfs: Vfs, watched: &[(FileId, &Path)]) -> Tree {
        let mut directories = FxHashMap::default();
        for &(file, path) in watched {
            let root = db.file_source_root(file).source_root_id(db);
            if db.source_root(root).source_root(db).is_library {
                continue;
            }
            let Some(parent) = path.parent() else { continue };
            let listed = std::fs::metadata(parent).and_then(|it| it.modified()).ok();
            directories.entry(parent.to_owned()).or_insert(Directory { root, listed });
        }
        Tree { vfs, directories }
    }

    /// The source files that are new since the last look. Each is in its source root once
    /// this returns, with no text in it yet.
    pub(crate) fn added(&mut self, db: &mut RootDatabase) -> Moved {
        let mut stale: Vec<PathBuf> = self
            .directories
            .iter()
            .filter(|(path, dir)| {
                let now = std::fs::metadata(path).and_then(|it| it.modified()).ok();
                now.is_some() && now != dir.listed
            })
            .map(|(path, _)| path.clone())
            .collect();
        let mut moved = Moved::default();
        let mut joined: FxHashMap<SourceRootId, Vec<(FileId, VfsPath)>> = FxHashMap::default();
        while let Some(path) = stale.pop() {
            let Some(dir) = self.directories.get_mut(&path) else { continue };
            let root = dir.root;
            dir.listed = std::fs::metadata(&path).and_then(|it| it.modified()).ok();
            let Ok(entries) = std::fs::read_dir(&path) else { continue };
            // A package's directory holds much that is not source: only the directories
            // with source right in them are looked into there.
            let package = path.join("Cargo.toml").exists();
            for entry in entries.flatten() {
                let entry_path = entry.path();
                let Ok(kind) = entry.file_type() else { continue };
                let hidden = entry.file_name().to_str().is_none_or(|it| it.starts_with('.'));
                if kind.is_dir() {
                    let source = !hidden
                        && (!package || entry.file_name() != "target" && holds_source(&entry_path));
                    if source && !self.directories.contains_key(&entry_path) {
                        self.directories
                            .insert(entry_path.clone(), Directory { root, listed: None });
                        stale.push(entry_path);
                    }
                    continue;
                }
                if entry_path.extension().is_none_or(|it| it != "rs") {
                    continue;
                }
                if entry_path.to_str().is_none() {
                    continue;
                }
                let vfs_path = VfsPath::from(AbsPathBuf::assert_utf8(entry_path.clone()));
                let known = self.vfs.file_id(&vfs_path).map(|it| it.0);
                let source_root = db.source_root(root).source_root(db);
                if known.is_some_and(|it| source_root.path_for_file(&it).is_some()) {
                    continue;
                }
                let file = match known {
                    Some(file) => file,
                    None => {
                        self.vfs.set_file_contents(vfs_path.clone(), Some(Vec::new()));
                        let Some((file, _)) = self.vfs.file_id(&vfs_path) else { continue };
                        db.set_file_text_with_durability(file, "", Durability::LOW);
                        file
                    }
                };
                joined.entry(root).or_default().push((file, vfs_path));
                moved.added.push((file, entry_path));
            }
        }
        for (root, files) in joined {
            for &(file, _) in &files {
                db.set_file_source_root_with_durability(file, root, Durability::LOW);
            }
            self.rewrite(db, root, files, &FxHashSet::default());
        }
        moved
    }

    /// Takes files that are gone out of their source roots, so that a path no longer leads
    /// to them.
    pub(crate) fn remove(&mut self, db: &mut RootDatabase, files: &[FileId]) {
        let mut left: FxHashMap<SourceRootId, FxHashSet<FileId>> = FxHashMap::default();
        for &file in files {
            let root = db.file_source_root(file).source_root_id(db);
            left.entry(root).or_default().insert(file);
        }
        for (root, gone) in left {
            self.rewrite(db, root, Vec::new(), &gone);
        }
    }

    fn rewrite(
        &self,
        db: &mut RootDatabase,
        root: SourceRootId,
        joined: Vec<(FileId, VfsPath)>,
        gone: &FxHashSet<FileId>,
    ) {
        let was = db.source_root(root).source_root(db);
        let mut files = FileSet::default();
        for file in was.iter().filter(|it| !gone.contains(it)) {
            if let Some(path) = was.path_for_file(&file) {
                files.insert(file, path.clone());
            }
        }
        for (file, path) in joined {
            files.insert(file, path);
        }
        db.set_source_root_with_durability(
            root,
            Arc::new(SourceRoot::new_local(files)),
            Durability::LOW,
        );
    }
}

fn holds_source(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else { return false };
    // Another package has a source root of its own.
    if directory.join("Cargo.toml").exists() {
        return false;
    }
    entries.flatten().any(|it| it.path().extension().is_some_and(|it| it == "rs"))
}
