//! What every folder on the volume contains, as last read. A tree is built from it; the
//! change journal keeps it current without walking the volume again.
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::journal::{self, JournalCursor};
use crate::scan::{self, ScanProgress, Tree};
use crate::winfs::{self, OwnedHandle, RawEntry, WidePath};

pub const NOT_WALKED: u32 = u32::MAX;
const ROOT_LISTING: u32 = 0;

/// Plain data with no implicit padding, so a snapshot can store entries byte for byte.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Entry {
    pub name_end: u32,
    pub attributes: u32,
    pub allocated_bytes: u64,
    pub modified_unix: i64,
    file_id_halves: [u64; 2],
    /// Listing of this entry's own contents, or `NOT_WALKED` for files and links.
    pub listing: u32,
    reserved: u32,
}

impl Entry {
    pub fn file_id(&self) -> u128 {
        u128::from(self.file_id_halves[0]) | u128::from(self.file_id_halves[1]) << 64
    }
}

pub struct Listing {
    pub id: u32,
    pub directory_file_id: u128,
    /// Entry names back to back; each entry records where its name ends.
    pub names: String,
    pub entries: Vec<Entry>,
    pub unreadable: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pass {
    pub elapsed: Duration,
    /// Folders re-read because the journal named them; `None` after a full scan.
    pub refreshed_directories: Option<u64>,
}

pub struct Index {
    pub root_path: PathBuf,
    pub listings: Vec<Option<Listing>>,
    pub journal: Option<JournalCursor>,
    pub updated_unix: i64,
    pub last_pass: Pass,
}

impl Index {
    pub fn scan(root: &Path, progress: &ScanProgress) -> io::Result<Self> {
        let started = Instant::now();
        let root_path = std::path::absolute(root)?;
        let wide_root = WidePath::new(&root_path);
        if !winfs::is_directory(winfs::attributes(&wide_root)?) {
            return Err(io::Error::other(format!("{} is not a directory", root_path.display())));
        }
        progress.set_status("scanning every folder");
        // Read before walking: whatever changes during the walk is replayed by the next refresh.
        let journal = journal::cursor(&root_path).ok();
        let root_file_id = winfs::open_directory(&wide_root).and_then(|root| winfs::file_id(&root));

        let reader = Reader {
            progress,
            fresh: Mutex::new(Vec::new()),
            next_listing: AtomicU32::new(ROOT_LISTING + 1),
            known_directories: None,
            volume: None,
        };
        scan_pool()?.scope(|scope| {
            reader.walk_path(scope, wide_root, ROOT_LISTING, root_file_id.unwrap_or(0))
        });

        let mut index = Self {
            root_path,
            listings: Vec::new(),
            journal,
            updated_unix: winfs::unix_seconds_now(),
            last_pass: Pass { elapsed: Duration::ZERO, refreshed_directories: None },
        };
        index.adopt(reader.fresh.into_inner().unwrap());
        index.last_pass.elapsed = started.elapsed();
        Ok(index)
    }

    /// Re-reads only the folders the change journal names. Fails when the journal cannot
    /// vouch for everything since the last pass; the index is then unchanged and stale.
    pub fn refresh(&mut self, progress: &ScanProgress) -> io::Result<()> {
        let started = Instant::now();
        let since = self
            .journal
            .ok_or_else(|| io::Error::other("the last scan was made without a change journal"))?;
        progress.set_status("reading the change journal");
        let changes = journal::changes_since(&self.root_path, since)?;

        let known_directories: HashMap<u128, u32> = self
            .listings
            .iter()
            .flatten()
            .filter(|listing| listing.directory_file_id != 0)
            .map(|listing| (listing.directory_file_id, listing.id))
            .collect();
        let changed: Vec<(u128, u32)> = changes
            .changed_directories
            .iter()
            .filter_map(|file_id| Some((*file_id, *known_directories.get(file_id)?)))
            .collect();
        progress.set_status(&format!("re-reading {} changed folders", changed.len()));

        let volume = winfs::open_directory(&WidePath::new(&self.root_path))?;
        let reader = Reader {
            progress,
            fresh: Mutex::new(Vec::new()),
            next_listing: AtomicU32::new(self.listings.len() as u32),
            known_directories: Some(&known_directories),
            volume: Some(&volume),
        };
        scan_pool()?.scope(|scope| {
            for &(file_id, listing) in &changed {
                let reader = &reader;
                scope.spawn(move |scope| reader.walk_id(scope, file_id, listing));
            }
        });

        let fresh = reader.fresh.into_inner().unwrap();
        self.adopt(fresh);
        self.drop_unreachable_listings();
        self.journal = Some(changes.cursor);
        self.updated_unix = winfs::unix_seconds_now();
        self.last_pass =
            Pass { elapsed: started.elapsed(), refreshed_directories: Some(changed.len() as u64) };
        Ok(())
    }

    pub fn tree(&self) -> Tree {
        let mut tree = scan::build_tree(self.root_path.clone(), &self.listings);
        tree.stats.elapsed = self.last_pass.elapsed;
        tree.stats.refreshed_directories = self.last_pass.refreshed_directories;
        tree.stats.finished_unix = self.updated_unix;
        tree
    }

    fn adopt(&mut self, fresh: Vec<Listing>) {
        for listing in fresh {
            let slot = listing.id as usize;
            if slot >= self.listings.len() {
                self.listings.resize_with(slot + 1, || None);
            }
            self.listings[slot] = Some(listing);
        }
    }

    /// Deleted folders leave listings nothing points to any more.
    fn drop_unreachable_listings(&mut self) {
        let mut reachable = vec![false; self.listings.len()];
        let mut pending = vec![ROOT_LISTING];
        while let Some(slot) = pending.pop() {
            let already_seen = std::mem::replace(&mut reachable[slot as usize], true);
            let Some(listing) = self.listings[slot as usize].as_ref().filter(|_| !already_seen)
            else {
                continue;
            };
            let children = listing.entries.iter().map(|entry| entry.listing);
            pending.extend(children.filter(|&child| (child as usize) < reachable.len()));
        }
        for (listing, is_reachable) in self.listings.iter_mut().zip(reachable) {
            if !is_reachable {
                *listing = None;
            }
        }
    }
}

fn scan_pool() -> io::Result<rayon::ThreadPool> {
    let cores = std::thread::available_parallelism().map_or(8, |count| count.get());
    rayon::ThreadPoolBuilder::new()
        .num_threads((cores * 2).clamp(8, 64))
        .thread_name(|index| format!("scan-{index}"))
        .start_handler(|_| winfs::raise_current_thread_priority())
        .build()
        .map_err(io::Error::other)
}

struct Reader<'a> {
    progress: &'a ScanProgress,
    fresh: Mutex<Vec<Listing>>,
    next_listing: AtomicU32,
    /// During a refresh: folders already indexed keep their listing instead of being re-walked.
    known_directories: Option<&'a HashMap<u128, u32>>,
    volume: Option<&'a OwnedHandle>,
}

impl Reader<'_> {
    fn walk_path<'scope>(
        &'scope self,
        scope: &rayon::Scope<'scope>,
        path: WidePath,
        listing: u32,
        directory_file_id: u128,
    ) {
        let mut subdirectories = Vec::new();
        self.read(
            listing,
            directory_file_id,
            |on_entry| winfs::read_directory(&path, on_entry),
            |entry, child_listing| {
                subdirectories.push((path.join(entry.name), child_listing, entry.file_id));
            },
        );
        for (subdirectory, child_listing, file_id) in subdirectories {
            scope.spawn(move |scope| self.walk_path(scope, subdirectory, child_listing, file_id));
        }
    }

    /// A folder that can no longer be opened was deleted; re-reading its parent drops it.
    fn walk_id<'scope>(
        &'scope self,
        scope: &rayon::Scope<'scope>,
        directory_file_id: u128,
        listing: u32,
    ) {
        let Some(volume) = self.volume else {
            return;
        };
        let Ok(directory) = winfs::open_directory_by_id(volume, directory_file_id) else {
            return;
        };
        let mut new_subdirectories = Vec::new();
        self.read(
            listing,
            directory_file_id,
            |on_entry| winfs::read_open_directory(&directory, on_entry),
            |entry, child_listing| new_subdirectories.push((entry.file_id, child_listing)),
        );
        for (file_id, child_listing) in new_subdirectories {
            scope.spawn(move |scope| self.walk_id(scope, file_id, child_listing));
        }
    }

    fn read(
        &self,
        id: u32,
        directory_file_id: u128,
        read_entries: impl FnOnce(&mut dyn FnMut(RawEntry<'_>)) -> io::Result<()>,
        mut on_unindexed_subdirectory: impl FnMut(&RawEntry<'_>, u32),
    ) {
        if self.progress.is_cancelled() {
            return;
        }
        let mut listing = Listing {
            id,
            directory_file_id,
            names: String::new(),
            entries: Vec::new(),
            unreadable: false,
        };
        let mut file_count = 0u64;
        let mut file_bytes = 0u64;

        let read = read_entries(&mut |entry: RawEntry<'_>| {
            push_utf8(&mut listing.names, entry.name);
            let is_walkable = entry.is_directory() && !entry.is_link();
            let child_listing = if !is_walkable {
                NOT_WALKED
            } else if let Some(indexed) = self.indexed_listing(entry.file_id) {
                indexed
            } else {
                let child_listing = self.next_listing.fetch_add(1, Ordering::Relaxed);
                on_unindexed_subdirectory(&entry, child_listing);
                child_listing
            };
            if !entry.is_directory() {
                file_count += 1;
                file_bytes += entry.allocated_bytes;
            }
            listing.entries.push(Entry {
                name_end: listing.names.len() as u32,
                attributes: entry.attributes,
                allocated_bytes: entry.allocated_bytes,
                modified_unix: winfs::filetime_to_unix_seconds(entry.modified),
                file_id_halves: [entry.file_id as u64, (entry.file_id >> 64) as u64],
                listing: child_listing,
                reserved: 0,
            });
        });
        listing.unreadable = read.is_err();

        self.progress.count_directory(file_count, file_bytes);
        self.fresh.lock().unwrap().push(listing);
    }

    fn indexed_listing(&self, file_id: u128) -> Option<u32> {
        if file_id == 0 {
            return None;
        }
        self.known_directories?.get(&file_id).copied()
    }
}

fn push_utf8(target: &mut String, name: &[u16]) {
    if name.iter().all(|&unit| unit < 0x80) {
        target.extend(name.iter().map(|&unit| unit as u8 as char));
        return;
    }
    target.extend(
        char::decode_utf16(name.iter().copied())
            .map(|decoded| decoded.unwrap_or(char::REPLACEMENT_CHARACTER)),
    );
}

impl Listing {
    pub fn from_parts(
        id: u32,
        directory_file_id: u128,
        names: String,
        entries: Vec<Entry>,
        unreadable: bool,
    ) -> Option<Self> {
        let mut previous_end = 0u32;
        let names_are_well_formed = entries.iter().all(|entry| {
            let is_ordered = entry.name_end >= previous_end;
            previous_end = entry.name_end;
            is_ordered && names.is_char_boundary(entry.name_end as usize)
        });
        let covers_names = previous_end as usize == names.len() || entries.is_empty();
        (names_are_well_formed && covers_names).then_some(Self {
            id,
            directory_file_id,
            names,
            entries,
            unreadable,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ROOT;
    use std::fs;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; bytes]).unwrap();
    }

    /// Stands in for the journal: re-reads exactly the given folders the way a refresh does.
    fn reread(index: &mut Index, changed: &[&Path]) {
        let known_directories: HashMap<u128, u32> = index
            .listings
            .iter()
            .flatten()
            .map(|listing| (listing.directory_file_id, listing.id))
            .collect();
        let volume = winfs::open_directory(&WidePath::new(&index.root_path)).unwrap();
        let progress = ScanProgress::default();
        let reader = Reader {
            progress: &progress,
            fresh: Mutex::new(Vec::new()),
            next_listing: AtomicU32::new(index.listings.len() as u32),
            known_directories: Some(&known_directories),
            volume: Some(&volume),
        };
        let changed_ids: Vec<(u128, u32)> = changed
            .iter()
            .map(|path| {
                let directory = winfs::open_directory(&WidePath::new(path)).unwrap();
                let file_id = winfs::file_id(&directory).unwrap();
                (file_id, known_directories[&file_id])
            })
            .collect();
        scan_pool().unwrap().scope(|scope| {
            for &(file_id, listing) in &changed_ids {
                let reader = &reader;
                scope.spawn(move |scope| reader.walk_id(scope, file_id, listing));
            }
        });
        let fresh = reader.fresh.into_inner().unwrap();
        index.adopt(fresh);
        index.drop_unreachable_listings();
    }

    #[test]
    fn rereading_changed_folders_matches_a_full_rescan() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        write(&root_path.join("kept/data.bin"), 8192);
        write(&root_path.join("grows/log.txt"), 4096);
        write(&root_path.join("doomed/deep/old.bin"), 16384);
        write(&root_path.join("moves/inner/payload.bin"), 12288);
        let mut index = Index::scan(&root_path, &ScanProgress::default()).unwrap();

        write(&root_path.join("grows/log.txt"), 40960);
        write(&root_path.join("grows/new/deeper/fresh.bin"), 8192);
        fs::remove_dir_all(root_path.join("doomed")).unwrap();
        fs::rename(root_path.join("moves"), root_path.join("moved")).unwrap();
        write(&root_path.join("moved/inner/added.bin"), 4096);
        reread(&mut index, &[&root_path, &root_path.join("grows"), &root_path.join("moved/inner")]);

        let refreshed = index.tree();
        let rescanned = Index::scan(&root_path, &ScanProgress::default()).unwrap().tree();
        assert_eq!(refreshed.node(ROOT).size, rescanned.node(ROOT).size);
        assert_eq!(refreshed.node(ROOT).file_count, rescanned.node(ROOT).file_count);
        assert_eq!(refreshed.len(), rescanned.len());
        assert!(refreshed.find(&root_path.join("doomed")).is_none());
        assert!(refreshed.find(&root_path.join("grows/new/deeper/fresh.bin")).is_some());
        let moved = refreshed.find(&root_path.join("moved/inner")).unwrap();
        assert_eq!(refreshed.node(moved).file_count, 2);
    }

    #[test]
    fn refresh_without_a_journal_position_fails_and_leaves_the_index_alone() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("data.bin"), 8192);
        let mut index = Index::scan(root.path(), &ScanProgress::default()).unwrap();
        index.journal = None;
        let listings_before = index.listings.iter().flatten().count();

        let refreshed = index.refresh(&ScanProgress::default());

        assert!(refreshed.is_err());
        assert_eq!(index.listings.iter().flatten().count(), listings_before);
    }
}
