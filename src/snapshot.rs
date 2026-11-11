//! The index of each scanned root, kept on disk so the next start shows real numbers at once
//! and only has to replay the change journal.
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::index::{Entry, Index, Listing, Pass};
use crate::journal::JournalCursor;
use crate::scan::ScanProgress;

const MAGIC: &[u8; 8] = b"RECLAIM2";
const HEADER_FIELDS: usize = 11;
const NOT_REFRESHED: u64 = u64::MAX;

#[repr(C)]
#[derive(Clone, Copy)]
struct ListingRecord {
    id: u32,
    entry_count: u32,
    names_length: u32,
    unreadable: u32,
    directory_file_id_halves: [u64; 2],
}

pub fn file_for(root: &Path) -> Option<PathBuf> {
    let local_data = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    let name: String = root
        .to_string_lossy()
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
        .collect();
    Some(local_data.join("borght-reclaim").join(format!("{name}.index")))
}

pub fn save(index: &Index, file: &Path) -> io::Result<()> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let listings: Vec<&Listing> = index.listings.iter().flatten().collect();
    let root_path = index.root_path.to_string_lossy();
    let journal = index.journal.unwrap_or(JournalCursor { journal_id: 0, next_usn: 0 });
    let header: [u64; HEADER_FIELDS] = [
        listings.len() as u64,
        index.listings.len() as u64,
        listings.iter().map(|listing| listing.entries.len() as u64).sum(),
        listings.iter().map(|listing| listing.names.len() as u64).sum(),
        root_path.len() as u64,
        u64::from(index.journal.is_some()),
        journal.journal_id,
        journal.next_usn as u64,
        index.updated_unix as u64,
        index.last_pass.elapsed.as_millis() as u64,
        index.last_pass.refreshed_directories.unwrap_or(NOT_REFRESHED),
    ];

    let unfinished = file.with_extension("index.writing");
    let mut writer = BufWriter::with_capacity(1 << 20, File::create(&unfinished)?);
    writer.write_all(MAGIC)?;
    for field in header {
        writer.write_all(&field.to_le_bytes())?;
    }
    writer.write_all(root_path.as_bytes())?;
    for listing in &listings {
        let record = ListingRecord {
            id: listing.id,
            entry_count: listing.entries.len() as u32,
            names_length: listing.names.len() as u32,
            unreadable: u32::from(listing.unreadable),
            directory_file_id_halves: [
                listing.directory_file_id as u64,
                (listing.directory_file_id >> 64) as u64,
            ],
        };
        writer.write_all(bytes_of(std::slice::from_ref(&record)))?;
    }
    for listing in &listings {
        writer.write_all(bytes_of(&listing.entries))?;
    }
    for listing in &listings {
        writer.write_all(listing.names.as_bytes())?;
    }
    writer.into_inner()?.sync_data()?;
    std::fs::rename(&unfinished, file)
}

/// Only used for the padding-free `#[repr(C)]` records of this file format.
fn bytes_of<T: Copy>(records: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(records.as_ptr() as *const u8, std::mem::size_of_val(records))
    }
}

fn read_records<T: Copy>(reader: &mut impl Read, count: usize) -> io::Result<Vec<T>> {
    let mut records: Vec<T> = Vec::with_capacity(count);
    unsafe {
        let bytes = std::slice::from_raw_parts_mut(
            records.as_mut_ptr() as *mut u8,
            count * std::mem::size_of::<T>(),
        );
        reader.read_exact(bytes)?;
        records.set_len(count);
    }
    Ok(records)
}

pub fn load(file: &Path) -> io::Result<Index> {
    let mut reader = File::open(file)?;
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("not a Borght Reclaim index"));
    }
    let mut header = [0u64; HEADER_FIELDS];
    for field in &mut header {
        let mut bytes = [0u8; 8];
        reader.read_exact(&mut bytes)?;
        *field = u64::from_le_bytes(bytes);
    }
    let [listing_count, slot_count, entry_count, names_length, root_length, has_journal, journal_id, next_usn, updated_unix, elapsed_ms, refreshed] =
        header;
    let expected_bytes = (MAGIC.len() + HEADER_FIELDS * 8) as u128
        + u128::from(root_length)
        + u128::from(listing_count) * std::mem::size_of::<ListingRecord>() as u128
        + u128::from(entry_count) * std::mem::size_of::<Entry>() as u128
        + u128::from(names_length);
    if u128::from(reader.metadata()?.len()) != expected_bytes || listing_count > slot_count {
        return Err(invalid("index is truncated"));
    }

    let root_path = String::from_utf8(read_records(&mut reader, root_length as usize)?)
        .map_err(|_| invalid("root path is not UTF-8"))?;
    let records: Vec<ListingRecord> = read_records(&mut reader, listing_count as usize)?;
    let entries: Vec<Entry> = read_records(&mut reader, entry_count as usize)?;
    let names = String::from_utf8(read_records(&mut reader, names_length as usize)?)
        .map_err(|_| invalid("names are not UTF-8"))?;

    let mut listings: Vec<Option<Listing>> = Vec::new();
    listings.resize_with(slot_count as usize, || None);
    let (mut entries_taken, mut names_taken) = (0usize, 0usize);
    for record in records {
        let entries_end = entries_taken + record.entry_count as usize;
        let names_end = names_taken + record.names_length as usize;
        let slot = listings.get_mut(record.id as usize).filter(|slot| slot.is_none());
        let listing_names = names.get(names_taken..names_end);
        let (Some(slot), Some(listing_names), Some(listing_entries)) =
            (slot, listing_names, entries.get(entries_taken..entries_end))
        else {
            return Err(invalid("index is inconsistent"));
        };
        let directory_file_id = u128::from(record.directory_file_id_halves[0])
            | u128::from(record.directory_file_id_halves[1]) << 64;
        *slot = Some(
            Listing::from_parts(
                record.id,
                directory_file_id,
                listing_names.to_string(),
                listing_entries.to_vec(),
                record.unreadable != 0,
            )
            .ok_or_else(|| invalid("index is inconsistent"))?,
        );
        entries_taken = entries_end;
        names_taken = names_end;
    }

    Ok(Index {
        root_path: PathBuf::from(root_path),
        listings,
        journal: (has_journal != 0)
            .then_some(JournalCursor { journal_id, next_usn: next_usn as i64 }),
        updated_unix: updated_unix as i64,
        last_pass: Pass {
            elapsed: Duration::from_millis(elapsed_ms),
            refreshed_directories: (refreshed != NOT_REFRESHED).then_some(refreshed),
        },
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub struct Current {
    pub index: Index,
    /// Why the change journal could not be used and every folder was walked instead.
    pub full_scan_reason: Option<String>,
}

/// The cheapest route to a current index: replay the change journal onto the saved one, and
/// only walk the whole root when that is impossible. `on_remembered` sees the saved index
/// before it is brought up to date, so a window can show it straight away.
pub fn current_index(
    root: &Path,
    progress: &ScanProgress,
    force_full_scan: bool,
    on_remembered: impl FnOnce(&Index),
) -> io::Result<Current> {
    let root = std::path::absolute(root)?;
    let file = file_for(&root);
    let mut full_scan_reason = force_full_scan.then(|| "a full scan was asked for".to_string());
    if !force_full_scan {
        progress.set_status("loading the last scan");
        let remembered = file
            .as_deref()
            .filter(|file| file.exists())
            .map(load)
            .filter(|loaded| loaded.as_ref().map_or(true, |index| index.root_path == root));
        match remembered {
            None => full_scan_reason = Some("no earlier scan of this folder".to_string()),
            Some(Err(error)) => {
                full_scan_reason = Some(format!("the saved scan is unusable: {error}"))
            }
            Some(Ok(mut index)) => {
                on_remembered(&index);
                match index.refresh(progress) {
                    Ok(()) => {
                        save_quietly(&index, file.as_deref());
                        return Ok(Current { index, full_scan_reason: None });
                    }
                    Err(error) => full_scan_reason = Some(error.to_string()),
                }
            }
        }
    }
    let index = Index::scan(&root, progress)?;
    save_quietly(&index, file.as_deref());
    Ok(Current { index, full_scan_reason })
}

fn save_quietly(index: &Index, file: Option<&Path>) {
    if let Some(file) = file {
        if let Err(error) = save(index, file) {
            eprintln!("reclaim: the scan could not be saved for next time: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ROOT;

    fn scanned(root: &Path) -> Index {
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/data.bin"), vec![3u8; 20_000]).unwrap();
        std::fs::write(root.join("a/ünïcode.txt"), vec![3u8; 9_000]).unwrap();
        Index::scan(root, &ScanProgress::default()).unwrap()
    }

    #[test]
    fn saved_index_loads_back_identically() {
        let root = tempfile::tempdir().unwrap();
        let index = scanned(root.path());
        let file = root.path().join("cache/test.index");

        save(&index, &file).unwrap();
        let loaded = load(&file).unwrap();

        let (original, restored) = (index.tree(), loaded.tree());
        assert_eq!(restored.len(), original.len());
        assert_eq!(restored.node(ROOT).size, original.node(ROOT).size);
        assert_eq!(loaded.journal, index.journal);
        assert_eq!(loaded.updated_unix, index.updated_unix);
        let unicode = restored.find(&root.path().join("a/ünïcode.txt")).unwrap();
        assert_eq!(restored.path(unicode), root.path().join("a").join("ünïcode.txt"));
    }

    #[test]
    fn damaged_index_is_rejected_instead_of_trusted() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("test.index");
        save(&scanned(root.path()), &file).unwrap();
        let bytes = std::fs::read(&file).unwrap();

        let truncated = root.path().join("truncated.index");
        std::fs::write(&truncated, &bytes[..bytes.len() - 7]).unwrap();
        let mut garbled = bytes.clone();
        let names_start = garbled.len() - 10;
        garbled[names_start] = 0xff;
        let corrupted = root.path().join("corrupted.index");
        std::fs::write(&corrupted, &garbled).unwrap();

        assert!(load(&truncated).is_err());
        assert!(load(&corrupted).is_err());
    }
}
