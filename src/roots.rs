use std::{
    iter,
    path::{Path, PathBuf},
};

use relative_path::{ RelativePath, RelativePathBuf};

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

/// VfsRoot identifies a watched directory on the file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VfsRoot(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileType {
    File,
    Dir,
}

impl FileType {
    pub(crate) fn is_dir(&self) -> bool {
        *self == FileType::Dir
    }
}

impl std::convert::From<std::fs::FileType> for FileType {
    fn from(v: std::fs::FileType) -> Self {
        if v.is_file() {
            FileType::File
        } else {
            FileType::Dir
        }
    }
}

/// Describes the contents of a single source root.
///
/// `RootData` can be thought of as a glob pattern like `src/**.rs` which
/// specifies the source root or as a function which takes a `PathBuf` and
/// returns `true` if path belongs to the source root
struct RootData {
    entry: RootEntry,
    // result of `root.canonicalize()` if that differs from `root`; `None` otherwise.
    canonical_path: Option<PathBuf>,
    excluded_dirs: Vec<RelativePathBuf>,
}

pub(crate) struct Roots {
    roots: Vec<RootData>,
}

impl Roots {
    pub(crate) fn new(mut paths: Vec<RootEntry>) -> Roots {
        // A hack to make nesting work.
        paths.sort_by_key(|it| std::cmp::Reverse(it.path.as_os_str().len()));
        paths.dedup();

        // First gather all the nested roots for each path
        let nested_roots = paths
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                paths[..i]
                    .iter()
                    .filter_map(|it| rel_path(&entry.path, &it.path))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        // Then combine the entry with the matching nested_roots
        let roots = paths
            .into_iter()
            .zip(nested_roots.into_iter())
            .map(|(entry, nested_roots)| RootData::new(entry, nested_roots))
            .collect::<Vec<_>>();

        Roots { roots }
    }
    pub(crate) fn find(
        &self,
        path: &Path,
        expected: FileType,
    ) -> Option<(VfsRoot, RelativePathBuf)> {
        self.iter().find_map(|root| {
            let rel_path = self.contains(root, path, expected)?;
            Some((root, rel_path))
        })
    }
    pub(crate) fn len(&self) -> usize {
        self.roots.len()
    }
    pub(crate) fn iter<'a>(&'a self) -> impl Iterator<Item = VfsRoot> + 'a {
        (0..self.roots.len()).into_iter().map(|idx| VfsRoot(idx as u32))
    }
    pub(crate) fn path(&self, root: VfsRoot) -> &Path {
        self.root(root).path().as_path()
    }

    /// Checks if root contains a path with the given `FileType`
    /// and returns a root-relative path.
    pub(crate) fn contains(
        &self,
        root: VfsRoot,
        path: &Path,
        expected: FileType,
    ) -> Option<RelativePathBuf> {
        let data = self.root(root);
        iter::once(data.path())
            .chain(data.canonical_path.as_ref().into_iter())
            .find_map(|base| to_relative_path(base, path, &data, expected))
    }

    fn root(&self, root: VfsRoot) -> &RootData {
        &self.roots[root.0 as usize]
    }
}

impl RootData {
    pub fn new(entry: RootEntry, excluded_dirs: Vec<RelativePathBuf>) -> Self {
        let mut canonical_path = entry.path.canonicalize().ok();
        if Some(&entry.path) == canonical_path.as_ref() {
            canonical_path = None;
        }
        RootData { entry, canonical_path, excluded_dirs }
    }

    fn path(&self) -> &PathBuf {
        &self.entry.path
    }

    /// Returns true if the given `RelativePath` is included inside this `RootData`
    fn is_included(&self, rel_path: &RelativePathBuf, expected: FileType) -> bool {
        if self.excluded_dirs.contains(&rel_path) {
            return false;
        }

        let parent_included =
            rel_path.parent().map(|d| self.entry.filter.include_dir(&d)).unwrap_or(true);

        if !parent_included {
            return false;
        }

        match expected {
            FileType::File => self.entry.filter.include_file(&rel_path),
            FileType::Dir => self.entry.filter.include_dir(&rel_path),
        }
    }
}

/// Returns the path relative to `base`
fn rel_path(base: &Path, path: &Path) -> Option<RelativePathBuf> {
    let path = path.strip_prefix(base).ok()?;
    let path = RelativePathBuf::from_path(path).unwrap();
    Some(path)
}

/// Returns the path relative to `base` with filtering applied based on `data`
fn to_relative_path(
    base: &Path,
    path: &Path,
    data: &RootData,
    expected: FileType,
) -> Option<RelativePathBuf> {
    let rel_path = rel_path(base, path)?;

    // Apply filtering _only_ if the relative path is non-empty
    // if it's empty, it means we are currently processing the root
    if rel_path.as_str().is_empty() {
        return Some(rel_path);
    }

    if data.is_included(&rel_path, expected) {
        Some(rel_path)
    } else {
        None
    }
}
