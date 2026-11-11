use std::collections::VecDeque;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rayon::prelude::*;

use crate::index::{Listing, NOT_WALKED};
use crate::winfs;

pub type NodeId = u32;
pub const ROOT: NodeId = 0;
const NO_NODE: NodeId = u32::MAX;

const IS_DIRECTORY: u8 = 1 << 0;
const IS_LINK: u8 = 1 << 1;
const IS_HARDLINKED: u8 = 1 << 2;
const SHARES_FILES_OUTSIDE: u8 = 1 << 3;
const IS_UNREADABLE: u8 = 1 << 4;
const IS_REMOVED: u8 = 1 << 5;

pub struct Node {
    pub parent: NodeId,
    first_child: NodeId,
    child_count: u32,
    name_start: u32,
    name_length: u16,
    flags: u8,
    pub file_count: u32,
    /// Share of the volume this node occupies; hardlinked bytes are split evenly across links.
    pub size: u64,
    /// Bytes the volume gets back when only this node is deleted.
    pub frees: u64,
    pub modified_unix: i64,
}

#[derive(Default)]
pub struct ScanProgress {
    pub files: AtomicU64,
    pub directories: AtomicU64,
    pub bytes: AtomicU64,
    pub cancelled: AtomicBool,
    status: Mutex<String>,
}

impl ScanProgress {
    pub fn count_directory(&self, files: u64, file_bytes: u64) {
        self.files.fetch_add(files, Ordering::Relaxed);
        self.directories.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(file_bytes, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn set_status(&self, status: &str) {
        *self.status.lock().unwrap() = status.to_string();
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanStats {
    pub files: u64,
    pub directories: u64,
    pub unreadable_directories: u64,
    pub hardlinked_files: u64,
    pub elapsed: Duration,
    pub finished_unix: i64,
    /// Folders re-read from the change journal; `None` when every folder was walked.
    pub refreshed_directories: Option<u64>,
}

pub struct Tree {
    nodes: Vec<Node>,
    names: String,
    root_path: PathBuf,
    pub stats: ScanStats,
}

impl Tree {
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn name(&self, id: NodeId) -> &str {
        let node = self.node(id);
        let start = node.name_start as usize;
        &self.names[start..start + node.name_length as usize]
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut below_root = Vec::new();
        let mut current = id;
        while current != ROOT {
            below_root.push(current);
            current = self.node(current).parent;
        }
        let mut path = self.root_path.clone();
        for ancestor in below_root.into_iter().rev() {
            path.push(self.name(ancestor));
        }
        path
    }

    pub fn children(&self, id: NodeId) -> Range<NodeId> {
        let node = self.node(id);
        if node.child_count == 0 {
            return 0..0;
        }
        node.first_child..node.first_child + node.child_count
    }

    pub fn child_named(&self, parent: NodeId, name: &str) -> Option<NodeId> {
        self.children(parent).find(|&child| self.name(child).eq_ignore_ascii_case(name))
    }

    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let relative = path.strip_prefix(&self.root_path).ok()?;
        relative.components().try_fold(ROOT, |node, component| {
            self.child_named(node, component.as_os_str().to_str()?)
        })
    }

    pub fn is_directory(&self, id: NodeId) -> bool {
        self.has_flag(id, IS_DIRECTORY)
    }

    pub fn is_link(&self, id: NodeId) -> bool {
        self.has_flag(id, IS_LINK)
    }

    pub fn is_hardlinked(&self, id: NodeId) -> bool {
        self.has_flag(id, IS_HARDLINKED)
    }

    /// True when some file below this directory is also hardlinked from outside it.
    pub fn shares_files_outside(&self, id: NodeId) -> bool {
        self.has_flag(id, SHARES_FILES_OUTSIDE)
    }

    pub fn is_unreadable(&self, id: NodeId) -> bool {
        self.has_flag(id, IS_UNREADABLE)
    }

    pub fn is_removed(&self, id: NodeId) -> bool {
        self.has_flag(id, IS_REMOVED)
    }

    /// Takes a deleted node out of every total without rescanning the volume.
    pub fn forget(&mut self, id: NodeId) {
        if id == ROOT || self.is_removed(id) {
            return;
        }
        let (size, frees, file_count) = {
            let node = &mut self.nodes[id as usize];
            let totals = (node.size, node.frees, node.file_count);
            node.size = 0;
            node.frees = 0;
            node.file_count = 0;
            node.child_count = 0;
            node.flags |= IS_REMOVED;
            totals
        };
        let mut ancestor = self.nodes[id as usize].parent;
        while ancestor != NO_NODE {
            let node = &mut self.nodes[ancestor as usize];
            node.size = node.size.saturating_sub(size);
            node.frees = node.frees.saturating_sub(frees);
            node.file_count = node.file_count.saturating_sub(file_count);
            ancestor = node.parent;
        }
    }

    pub fn is_inside(&self, id: NodeId, ancestor: NodeId) -> bool {
        let mut current = id;
        loop {
            if current == ancestor {
                return true;
            }
            if current == ROOT {
                return false;
            }
            current = self.node(current).parent;
        }
    }

    fn has_flag(&self, id: NodeId, flag: u8) -> bool {
        self.node(id).flags & flag != 0
    }
}

#[cfg(test)]
pub fn scan(root: &Path, progress: &ScanProgress) -> std::io::Result<Tree> {
    Ok(crate::index::Index::scan(root, progress)?.tree())
}

struct FileLink {
    file_id: u128,
    node: NodeId,
}

/// Each listing is entered at most once, so a damaged index cannot loop or duplicate a folder.
pub fn build_tree(root_path: PathBuf, listings: &[Option<Listing>]) -> Tree {
    let entry_count: usize = listings.iter().flatten().map(|listing| listing.entries.len()).sum();
    let name_bytes: usize = listings.iter().flatten().map(|listing| listing.names.len()).sum();
    let mut entered = vec![false; listings.len()];

    let root_name = root_path.to_string_lossy().into_owned();
    let mut names = String::with_capacity(name_bytes + root_name.len());
    names.push_str(&root_name);
    let mut nodes = Vec::with_capacity(entry_count + 1);
    nodes.push(Node {
        parent: NO_NODE,
        first_child: 0,
        child_count: 0,
        name_start: 0,
        name_length: root_name.len() as u16,
        flags: IS_DIRECTORY,
        file_count: 0,
        size: 0,
        frees: 0,
        modified_unix: 0,
    });

    let mut depths: Vec<u16> = Vec::with_capacity(entry_count + 1);
    depths.push(0);
    let mut links: Vec<FileLink> = Vec::new();
    let mut stats = ScanStats::default();
    let mut pending: VecDeque<(NodeId, u32)> = VecDeque::from([(ROOT, 0)]);

    while let Some((directory, listing_id)) = pending.pop_front() {
        stats.directories += 1;
        let slot = listing_id as usize;
        let first_entry = slot < entered.len() && !std::mem::replace(&mut entered[slot], true);
        let Some(listing) = listings.get(slot).and_then(Option::as_ref).filter(|_| first_entry)
        else {
            continue;
        };
        if listing.unreadable {
            nodes[directory as usize].flags |= IS_UNREADABLE;
            stats.unreadable_directories += 1;
        }
        let names_base = names.len() as u32;
        names.push_str(&listing.names);
        let child_depth = depths[directory as usize] + 1;
        nodes[directory as usize].first_child = nodes.len() as NodeId;
        nodes[directory as usize].child_count = listing.entries.len() as u32;

        let mut name_start = 0u32;
        for entry in &listing.entries {
            let id = nodes.len() as NodeId;
            let is_directory = winfs::is_directory(entry.attributes);
            let mut flags = 0;
            if is_directory {
                flags |= IS_DIRECTORY;
                if winfs::is_link(entry.attributes) {
                    flags |= IS_LINK;
                }
            }
            let file_bytes = if is_directory { 0 } else { entry.allocated_bytes };
            nodes.push(Node {
                parent: directory,
                first_child: 0,
                child_count: 0,
                name_start: names_base + name_start,
                name_length: (entry.name_end - name_start) as u16,
                flags,
                file_count: u32::from(!is_directory),
                size: file_bytes,
                frees: file_bytes,
                modified_unix: entry.modified_unix,
            });
            depths.push(child_depth);
            name_start = entry.name_end;

            if entry.listing != NOT_WALKED {
                pending.push_back((id, entry.listing));
            } else if !is_directory {
                stats.files += 1;
                if entry.file_id() != 0 && file_bytes > 0 {
                    links.push(FileLink { file_id: entry.file_id(), node: id });
                }
            }
        }
    }

    stats.hardlinked_files = attribute_hardlinks(&mut nodes, &depths, links);
    roll_up_totals(&mut nodes);
    Tree { nodes, names, root_path, stats }
}

/// A file with several links is only freed when all of them go, so its bytes are credited to
/// the lowest directory containing every link; each link shows an even share as its size.
fn attribute_hardlinks(nodes: &mut [Node], depths: &[u16], mut links: Vec<FileLink>) -> u64 {
    links.par_sort_unstable_by_key(|link| link.file_id);
    let mut hardlinked_files = 0;
    for group in links.chunk_by(|left, right| left.file_id == right.file_id) {
        if group.len() < 2 {
            continue;
        }
        hardlinked_files += group.len() as u64;
        let allocated_bytes = nodes[group[0].node as usize].size;
        let share = allocated_bytes / group.len() as u64;
        let rounding_remainder = allocated_bytes % group.len() as u64;
        let common_directory = group
            .iter()
            .map(|link| nodes[link.node as usize].parent)
            .reduce(|left, right| lowest_common_ancestor(nodes, depths, left, right))
            .unwrap();
        nodes[common_directory as usize].frees += allocated_bytes;

        for (position, link) in group.iter().enumerate() {
            let file = &mut nodes[link.node as usize];
            file.size = if position == 0 { share + rounding_remainder } else { share };
            file.frees = 0;
            file.flags |= IS_HARDLINKED;
            let mut ancestor = file.parent;
            while ancestor != common_directory {
                nodes[ancestor as usize].flags |= SHARES_FILES_OUTSIDE;
                ancestor = nodes[ancestor as usize].parent;
            }
        }
    }
    hardlinked_files
}

fn lowest_common_ancestor(nodes: &[Node], depths: &[u16], left: NodeId, right: NodeId) -> NodeId {
    let (mut left, mut right) = (left, right);
    while depths[left as usize] > depths[right as usize] {
        left = nodes[left as usize].parent;
    }
    while depths[right as usize] > depths[left as usize] {
        right = nodes[right as usize].parent;
    }
    while left != right {
        left = nodes[left as usize].parent;
        right = nodes[right as usize].parent;
    }
    left
}

/// Children always follow their parent in the node list, so one reverse pass sums every subtree.
fn roll_up_totals(nodes: &mut [Node]) {
    for id in (1..nodes.len()).rev() {
        let (size, frees, file_count, modified_unix, parent) = {
            let node = &nodes[id];
            (node.size, node.frees, node.file_count, node.modified_unix, node.parent as usize)
        };
        let parent = &mut nodes[parent];
        parent.size += size;
        parent.frees += frees;
        parent.file_count += file_count;
        parent.modified_unix = parent.modified_unix.max(modified_unix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scan_directory(path: &Path) -> Tree {
        scan(path, &ScanProgress::default()).unwrap()
    }

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; bytes]).unwrap();
    }

    #[test]
    fn sums_file_sizes_into_every_ancestor() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a/b/one.bin"), 8192);
        write(&root.path().join("a/two.bin"), 4096);
        write(&root.path().join("three.bin"), 4096);

        let tree = scan_directory(root.path());

        let a = tree.find(&root.path().join("a")).unwrap();
        let b = tree.find(&root.path().join("a/b")).unwrap();
        assert_eq!(tree.node(ROOT).file_count, 3);
        assert_eq!(tree.node(a).file_count, 2);
        assert_eq!(tree.node(a).size, tree.node(b).size + 4096);
        assert_eq!(tree.node(ROOT).size, tree.node(a).size + 4096);
        assert_eq!(tree.node(ROOT).frees, tree.node(ROOT).size);
        assert_eq!(tree.path(b), root.path().join("a").join("b"));
    }

    #[test]
    fn hardlinked_bytes_are_freed_only_by_the_directory_holding_every_link() {
        let root = tempfile::tempdir().unwrap();
        let cached = root.path().join("cache/pkg/index.js");
        write(&cached, 65536);
        for project in ["projects/one", "projects/two"] {
            let linked = root.path().join(project).join("node_modules/pkg/index.js");
            fs::create_dir_all(linked.parent().unwrap()).unwrap();
            fs::hard_link(&cached, &linked).unwrap();
        }

        let tree = scan_directory(root.path());

        let cache = tree.find(&root.path().join("cache")).unwrap();
        let projects = tree.find(&root.path().join("projects")).unwrap();
        let project_one = tree.find(&root.path().join("projects/one")).unwrap();
        let allocated = tree.node(ROOT).size;
        assert!(allocated >= 65536, "three links must be counted once, got {allocated}");
        assert!(allocated < 2 * 65536, "three links must be counted once, got {allocated}");
        assert_eq!(tree.node(ROOT).frees, allocated);
        assert_eq!(tree.node(cache).frees, 0);
        assert_eq!(tree.node(projects).frees, 0);
        assert!(tree.node(project_one).size.abs_diff(allocated / 3) <= 2);
        assert!(tree.shares_files_outside(cache));
        assert!(tree.shares_files_outside(project_one));
        assert!(!tree.shares_files_outside(ROOT));
        assert_eq!(tree.stats.hardlinked_files, 3);
    }

    #[test]
    fn links_confined_to_one_directory_are_freed_by_it() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("store/a/blob");
        write(&original, 32768);
        fs::create_dir_all(root.path().join("store/b")).unwrap();
        fs::hard_link(&original, root.path().join("store/b/blob")).unwrap();
        write(&root.path().join("other/file"), 4096);

        let tree = scan_directory(root.path());

        let store = tree.find(&root.path().join("store")).unwrap();
        let store_a = tree.find(&root.path().join("store/a")).unwrap();
        assert_eq!(tree.node(store).frees, tree.node(store).size);
        assert_eq!(tree.node(store_a).frees, 0);
        assert!(!tree.shares_files_outside(store));
        assert!(tree.shares_files_outside(store_a));
    }

    #[test]
    fn junctions_are_listed_but_never_followed() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("real/data.bin"), 16384);
        let junction = root.path().join("junction");
        let created = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(root.path().join("real"))
            .output()
            .unwrap();
        assert!(created.status.success());

        let tree = scan_directory(root.path());

        let junction = tree.find(&junction).unwrap();
        assert!(tree.is_link(junction));
        assert_eq!(tree.children(junction).len(), 0);
        assert_eq!(tree.node(ROOT).file_count, 1);
    }
}
