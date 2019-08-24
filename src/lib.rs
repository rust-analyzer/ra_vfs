//! VFS stands for Virtual File System.
//!
//! When doing analysis, we don't want to do any IO, we want to keep all source
//! code in memory. However, the actual source code is stored on disk, so you
//! need to get it into the memory in the first place somehow. VFS is the
//! component which does this.
//!
//! It is also responsible for watching the disk for changes, and for merging
//! editor state (modified, unsaved files) with disk state.
//!
//! TODO: Some LSP clients support watching the disk, so this crate should to
//! support custom watcher events (related to
//! <https://github.com/rust-analyzer/rust-analyzer/issues/131>)
//!
//! VFS is based on a concept of roots: a set of directories on the file system
//! which are watched for changes. Typically, there will be a root for each
//! Cargo package.
mod roots;
mod io;

use std::{
    fmt, fs, mem,
    path::{Path, PathBuf},
    sync::Arc,
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    io::{TaskResult, Worker},
    roots::{Roots, FileType},
};

pub use relative_path::{RelativePath, RelativePathBuf};
pub use crate::roots::VfsRoot;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LineEndings {
    Unix,
    Dos,
}

impl Default for LineEndings {
    fn default() -> Self {
        LineEndings::Unix
    }
}

/// a `Filter` is used to determine whether a file or a folder
/// under the specific root is included.
///
/// *NOTE*: If the parent folder of a file is not included, then
/// `include_file` will not be called.
///
/// # Example
///
/// Implementing `Filter` for rust files:
///
/// ```
/// use ra_vfs::{Filter, RelativePath};
///
/// struct IncludeRustFiles;
///
/// impl Filter for IncludeRustFiles {
///     fn include_dir(&self, dir_path: &RelativePath) -> bool {
///         // These folders are ignored
///         const IGNORED_FOLDERS: &[&str] = &["node_modules", "target", ".git"];
///
///         let is_ignored = dir_path.components().any(|c| IGNORED_FOLDERS.contains(&c.as_str()));
///
///         !is_ignored
///     }
///
///     fn include_file(&self, file_path: &RelativePath) -> bool {
///         // Only include rust files
///         file_path.extension() == Some("rs")
///     }
/// }
/// ```
pub trait Filter: Send + Sync {
    fn include_dir(&self, dir_path: &RelativePath) -> bool;
    fn include_file(&self, file_path: &RelativePath) -> bool;
}

/// RootEntry identifies a root folder with a given filter
/// used to determine whether to include or exclude files and folders under it.
pub struct RootEntry {
    path: PathBuf,
    filter: Box<dyn Filter>,
}

impl std::fmt::Debug for RootEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RootEntry({})", self.path.display())
    }
}

impl Eq for RootEntry {}
impl PartialEq for RootEntry {
    fn eq(&self, other: &Self) -> bool {
        // Entries are equal based on their paths
        self.path == other.path
    }
}

impl RootEntry {
    /// Create a new `RootEntry` with the given `filter` applied to
    /// files and folder under it.
    pub fn new(path: PathBuf, filter: Box<dyn Filter>) -> Self {
        RootEntry { path, filter }
    }
}
/// Opaque wrapper around file-system event.
///
/// Calling code is expected to just pass `VfsTask` to `handle_task` method. It
/// is exposed as a public API so that the caller can plug vfs events into the
/// main event loop and be notified when changes happen.
pub struct VfsTask(TaskResult);

impl fmt::Debug for VfsTask {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("VfsTask { ... }")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VfsFile(pub u32);

struct VfsFileData {
    root: VfsRoot,
    path: RelativePathBuf,
    is_overlayed: bool,
    text: Arc<String>,
    line_endings: LineEndings,
}

pub struct Vfs {
    roots: Arc<Roots>,
    files: Vec<VfsFileData>,
    root2files: FxHashMap<VfsRoot, FxHashSet<VfsFile>>,
    pending_changes: Vec<VfsChange>,
    #[allow(unused)]
    worker: Worker,
}

impl fmt::Debug for Vfs {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Vfs")
            .field("n_roots", &self.roots.len())
            .field("n_files", &self.files.len())
            .field("n_pending_changes", &self.pending_changes.len())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub enum VfsChange {
    AddRoot { root: VfsRoot, files: Vec<(VfsFile, RelativePathBuf, Arc<String>)> },
    AddFile { root: VfsRoot, file: VfsFile, path: RelativePathBuf, text: Arc<String> },
    RemoveFile { root: VfsRoot, file: VfsFile, path: RelativePathBuf },
    ChangeFile { file: VfsFile, text: Arc<String> },
}

impl Vfs {
    pub fn new(roots: Vec<RootEntry>, on_task: Box<dyn FnMut(VfsTask) + Send>) -> (Vfs, Vec<VfsRoot>) {
        let roots = Arc::new(Roots::new(roots));
        let worker = io::start(Arc::clone(&roots), on_task);
        let mut root2files = FxHashMap::default();

        for root in roots.iter() {
            root2files.insert(root, Default::default());
            worker.send(io::Task::AddRoot { root });
        }
        let res = Vfs { roots, files: Vec::new(), root2files, worker, pending_changes: Vec::new() };
        let vfs_roots = res.roots.iter().collect();
        (res, vfs_roots)
    }

    pub fn root2path(&self, root: VfsRoot) -> PathBuf {
        self.roots.path(root).to_path_buf()
    }

    pub fn path2file(&self, path: &Path) -> Option<VfsFile> {
        if let Some((_root, _path, Some(file))) = self.find_root(path) {
            return Some(file);
        }
        None
    }

    pub fn file2path(&self, file: VfsFile) -> PathBuf {
        let rel_path = &self.file(file).path;
        let root_path = &self.roots.path(self.file(file).root);
        rel_path.to_path(root_path)
    }

    pub fn file_line_endings(&self, file: VfsFile) -> LineEndings {
        self.file(file).line_endings
    }

    pub fn n_roots(&self) -> usize {
        self.roots.len()
    }

    pub fn load(&mut self, path: &Path) -> Option<VfsFile> {
        if let Some((root, rel_path, file)) = self.find_root(path) {
            return if let Some(file) = file {
                Some(file)
            } else {
                let (text, line_endings) = read_to_string(path).unwrap_or_default();
                let text = Arc::new(text);
                let file = self.raw_add_file(
                    root,
                    rel_path.clone(),
                    Arc::clone(&text),
                    line_endings,
                    false,
                );
                let change = VfsChange::AddFile { file, text, root, path: rel_path };
                self.pending_changes.push(change);
                Some(file)
            };
        }
        None
    }

    pub fn add_file_overlay(&mut self, path: &Path, mut text: String) -> Option<VfsFile> {
        let line_endings = normalize_newlines(&mut text);
        let (root, rel_path, file) = self.find_root(path)?;
        if let Some(file) = file {
            self.change_file_event(file, text, true);
            Some(file)
        } else {
            self.add_file_event(root, rel_path, text, line_endings, true)
        }
    }

    pub fn change_file_overlay(&mut self, path: &Path, mut new_text: String) {
        let _line_endings = normalize_newlines(&mut new_text);
        if let Some((_root, _path, file)) = self.find_root(path) {
            let file = file.expect("can't change a file which wasn't added");
            self.change_file_event(file, new_text, true);
        }
    }

    pub fn remove_file_overlay(&mut self, path: &Path) -> Option<VfsFile> {
        let (root, rel_path, file) = self.find_root(path)?;
        let file = file.expect("can't remove a file which wasn't added");
        let full_path = rel_path.to_path(&self.roots.path(root));
        if let Ok(text) = fs::read_to_string(&full_path) {
            self.change_file_event(file, text, false);
        } else {
            self.remove_file_event(root, rel_path, file);
        }
        Some(file)
    }

    pub fn commit_changes(&mut self) -> Vec<VfsChange> {
        // FIXME: ideally we should compact changes here, such that we send at
        // most one event per VfsFile.
        mem::replace(&mut self.pending_changes, Vec::new())
    }

    pub fn handle_task(&mut self, task: VfsTask) {
        match task.0 {
            TaskResult::BulkLoadRoot { root, files } => {
                let mut cur_files = Vec::new();
                // While we were scanning the root in the background, a file might have
                // been open in the editor, so we need to account for that.
                let existing = self.root2files[&root]
                    .iter()
                    .map(|&file| (self.file(file).path.clone(), file))
                    .collect::<FxHashMap<_, _>>();
                for (path, text, line_endings) in files {
                    if let Some(&file) = existing.get(&path) {
                        let text = Arc::clone(&self.file(file).text);
                        cur_files.push((file, path, text));
                        continue;
                    }
                    let text = Arc::new(text);
                    let file = self.raw_add_file(
                        root,
                        path.clone(),
                        Arc::clone(&text),
                        line_endings,
                        false,
                    );
                    cur_files.push((file, path, text));
                }

                let change = VfsChange::AddRoot { root, files: cur_files };
                self.pending_changes.push(change);
            }
            TaskResult::SingleFile { root, path, text, line_endings } => {
                let existing_file = self.find_file(root, &path);
                if existing_file.map(|file| self.file(file).is_overlayed) == Some(true) {
                    return;
                }
                match (existing_file, text) {
                    (Some(file), None) => {
                        self.remove_file_event(root, path, file);
                    }
                    (None, Some(text)) => {
                        self.add_file_event(root, path, text, line_endings, false);
                    }
                    (Some(file), Some(text)) => {
                        if *self.file(file).text != text {
                            self.change_file_event(file, text, false);
                        }
                    }
                    (None, None) => (),
                }
            }
        }
    }

    // *_event calls change the state of VFS and push a change onto pending
    // changes array.

    fn add_file_event(
        &mut self,
        root: VfsRoot,
        path: RelativePathBuf,
        text: String,
        line_endings: LineEndings,
        is_overlay: bool,
    ) -> Option<VfsFile> {
        let text = Arc::new(text);
        let file =
            self.raw_add_file(root, path.clone(), Arc::clone(&text), line_endings, is_overlay);
        self.pending_changes.push(VfsChange::AddFile { file, root, path, text });
        Some(file)
    }

    fn change_file_event(&mut self, file: VfsFile, text: String, is_overlay: bool) {
        let text = Arc::new(text);
        self.raw_change_file(file, text.clone(), is_overlay);
        self.pending_changes.push(VfsChange::ChangeFile { file, text });
    }

    fn remove_file_event(&mut self, root: VfsRoot, path: RelativePathBuf, file: VfsFile) {
        self.raw_remove_file(file);
        self.pending_changes.push(VfsChange::RemoveFile { root, path, file });
    }

    // raw_* calls change the state of VFS, but **do not** emit events.

    fn raw_add_file(
        &mut self,
        root: VfsRoot,
        path: RelativePathBuf,
        text: Arc<String>,
        line_endings: LineEndings,
        is_overlayed: bool,
    ) -> VfsFile {
        let data = VfsFileData { root, path, text, line_endings, is_overlayed };
        let file = VfsFile(self.files.len() as u32);
        self.files.push(data);
        self.root2files.get_mut(&root).unwrap().insert(file);
        file
    }

    fn raw_change_file(&mut self, file: VfsFile, new_text: Arc<String>, is_overlayed: bool) {
        let mut file_data = &mut self.file_mut(file);
        file_data.text = new_text;
        file_data.is_overlayed = is_overlayed;
    }

    fn raw_remove_file(&mut self, file: VfsFile) {
        // FIXME: use arena with removal
        self.file_mut(file).text = Default::default();
        self.file_mut(file).path = Default::default();
        let root = self.file(file).root;
        let removed = self.root2files.get_mut(&root).unwrap().remove(&file);
        assert!(removed);
    }

    fn find_root(&self, path: &Path) -> Option<(VfsRoot, RelativePathBuf, Option<VfsFile>)> {
        let (root, path) = self.roots.find(&path, FileType::File)?;
        let file = self.find_file(root, &path);
        Some((root, path, file))
    }

    fn find_file(&self, root: VfsRoot, path: &RelativePath) -> Option<VfsFile> {
        self.root2files[&root].iter().map(|&it| it).find(|&file| self.file(file).path == path)
    }

    fn file(&self, file: VfsFile) -> &VfsFileData {
        &self.files[file.0 as usize]
    }

    fn file_mut(&mut self, file: VfsFile) -> &mut VfsFileData {
        &mut self.files[file.0 as usize]
    }
}

fn read_to_string(path: &Path) -> Option<(String, LineEndings)> {
    let mut text =
        fs::read_to_string(&path).map_err(|e| log::warn!("failed to read file {}", e)).ok()?;
    let line_endings = normalize_newlines(&mut text);
    Some((text, line_endings))
}

/// Replaces `\r\n` with `\n` in-place in `src`.
pub fn normalize_newlines(src: &mut String) -> LineEndings {
    if !src.as_bytes().contains(&b'\r') {
        return LineEndings::Unix;
    }

    // We replace `\r\n` with `\n` in-place, which doesn't break utf-8 encoding.
    // While we *can* call `as_mut_vec` and do surgery on the live string
    // directly, let's rather steal the contents of `src`. This makes the code
    // safe even if a panic occurs.

    let mut buf = std::mem::replace(src, String::new()).into_bytes();
    let mut gap_len = 0;
    let mut tail = buf.as_mut_slice();
    loop {
        let idx = match find_crlf(&tail[gap_len..]) {
            None => tail.len(),
            Some(idx) => idx + gap_len,
        };
        tail.copy_within(gap_len..idx, 0);
        tail = &mut tail[idx - gap_len..];
        if tail.len() == gap_len {
            break;
        }
        gap_len += 1;
    }

    // Account for removed `\r`.
    // After `set_len`, `buf` is guaranteed to contain utf-8 again.
    let new_len = buf.len() - gap_len;
    unsafe {
        buf.set_len(new_len);
        *src = String::from_utf8_unchecked(buf);
    }
    return LineEndings::Dos;

    fn find_crlf(src: &[u8]) -> Option<usize> {
        let mut search_idx = 0;
        while let Some(idx) = find_cr(&src[search_idx..]) {
            if src[search_idx..].get(idx + 1) != Some(&b'\n') {
                search_idx += idx + 1;
                continue;
            }
            return Some(search_idx + idx);
        }
        None
    }

    fn find_cr(src: &[u8]) -> Option<usize> {
        src.iter().enumerate().find_map(|(idx, &b)| if b == b'\r' { Some(idx) } else { None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct NoopFilter;

    impl Filter for NoopFilter {
        fn include_dir(&self, _: &RelativePath) -> bool {
            true
        }
        fn include_file(&self, _: &RelativePath) -> bool {
            true
        }
    }

    fn entry(s: &str) -> RootEntry {
        RootEntry::new(s.into(), Box::new(NoopFilter))
    }

    #[test]
    fn vfs_deduplicates() {
        let entries = vec!["/foo", "/bar", "/foo"].into_iter().map(entry).collect();
        let (_, roots) = Vfs::new(entries);
        assert_eq!(roots.len(), 2);
    }
}
