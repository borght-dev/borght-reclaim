//! The volume's USN change journal names every folder whose contents changed, which turns
//! a rescan of the whole volume into a re-read of just those folders.
use std::collections::HashSet;
use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::winfs::{self, OwnedHandle};

const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
/// Unlike `FSCTL_READ_USN_JOURNAL` this needs no elevation. It blanks file names, but keeps
/// the parent folder id of every record.
const FSCTL_READ_UNPRIVILEGED_USN_JOURNAL: u32 = 0x0009_03ab;
const READ_BUFFER_BYTES: usize = 1 << 20;

const RECORD_LENGTH_OFFSET: usize = 0;
const MAJOR_VERSION_OFFSET: usize = 4;
const V2_PARENT_OFFSET: usize = 16;
const V3_PARENT_OFFSET: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalCursor {
    pub journal_id: u64,
    pub next_usn: i64,
}

pub struct JournalChanges {
    /// File ids of the folders that had an entry created, removed, renamed or rewritten.
    pub changed_directories: HashSet<u128>,
    pub cursor: JournalCursor,
}

#[repr(C)]
struct JournalData {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
    supported_versions: u64,
    reserved: [u64; 2],
}

#[repr(C)]
struct ReadRequest {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    journal_id: u64,
    min_major_version: u16,
    max_major_version: u16,
}

fn query(volume: &OwnedHandle) -> io::Result<JournalData> {
    let mut data: JournalData = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    let queried = unsafe {
        DeviceIoControl(
            volume.raw(),
            FSCTL_QUERY_USN_JOURNAL,
            null(),
            0,
            &mut data as *mut JournalData as *mut c_void,
            std::mem::size_of::<JournalData>() as u32,
            &mut returned,
            null_mut(),
        )
    };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(data)
}

pub fn cursor(path_on_volume: &Path) -> io::Result<JournalCursor> {
    let data = query(&winfs::open_volume_for_journal(path_on_volume)?)?;
    Ok(JournalCursor { journal_id: data.journal_id, next_usn: data.next_usn })
}

/// Fails when the journal was recreated or has already discarded records after `since`;
/// the caller then has to scan from scratch.
pub fn changes_since(path_on_volume: &Path, since: JournalCursor) -> io::Result<JournalChanges> {
    let volume = winfs::open_volume_for_journal(path_on_volume)?;
    let journal = query(&volume)?;
    if journal.journal_id != since.journal_id {
        return Err(io::Error::other("the change journal was recreated since the last scan"));
    }
    if since.next_usn < journal.first_usn {
        return Err(io::Error::other("the change journal no longer reaches back to the last scan"));
    }

    let mut changed_directories = HashSet::new();
    let mut buffer = vec![0u64; READ_BUFFER_BYTES / 8];
    let mut position = since.next_usn;
    while position < journal.next_usn {
        let request = ReadRequest {
            start_usn: position,
            reason_mask: u32::MAX,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            journal_id: journal.journal_id,
            min_major_version: 2,
            max_major_version: 3,
        };
        let mut returned = 0u32;
        let read = unsafe {
            DeviceIoControl(
                volume.raw(),
                FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
                &request as *const ReadRequest as *const c_void,
                std::mem::size_of::<ReadRequest>() as u32,
                buffer.as_mut_ptr() as *mut c_void,
                READ_BUFFER_BYTES as u32,
                &mut returned,
                null_mut(),
            )
        };
        if read == 0 {
            return Err(io::Error::last_os_error());
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u8, returned as usize) };
        let Some(next_position) =
            bytes.get(..8).map(|usn| i64::from_le_bytes(usn.try_into().unwrap()))
        else {
            break;
        };
        collect_parents(&bytes[8..], &mut changed_directories);
        if next_position <= position {
            break;
        }
        position = next_position;
    }
    let cursor = JournalCursor { journal_id: journal.journal_id, next_usn: position };
    Ok(JournalChanges { changed_directories, cursor })
}

fn collect_parents(mut records: &[u8], parents: &mut HashSet<u128>) {
    while records.len() >= 8 {
        let length = read_u32(records, RECORD_LENGTH_OFFSET) as usize;
        if length < 8 || length > records.len() {
            return;
        }
        let record = &records[..length];
        match read_u16(record, MAJOR_VERSION_OFFSET) {
            2 if length >= V2_PARENT_OFFSET + 8 => {
                let parent = &record[V2_PARENT_OFFSET..V2_PARENT_OFFSET + 8];
                parents.insert(u128::from(u64::from_le_bytes(parent.try_into().unwrap())));
            }
            3 if length >= V3_PARENT_OFFSET + 16 => {
                let parent = &record[V3_PARENT_OFFSET..V3_PARENT_OFFSET + 16];
                parents.insert(u128::from_le_bytes(parent.try_into().unwrap()));
            }
            _ => {}
        }
        records = &records[length..];
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(major: u16, parent: u128, length: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; length];
        bytes[0..4].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&major.to_le_bytes());
        match major {
            2 => bytes[16..24].copy_from_slice(&(parent as u64).to_le_bytes()),
            _ => bytes[24..40].copy_from_slice(&parent.to_le_bytes()),
        }
        bytes
    }

    #[test]
    fn parents_are_read_from_both_record_versions() {
        let refs_parent = 0x0000_0000_0000_4a2f_0000_0000_0000_0000u128;
        let mut records = record(3, refs_parent, 80);
        records.extend(record(2, 0x0026_0000_0000_ca55, 64));
        records.extend(record(3, refs_parent, 80));
        let mut parents = HashSet::new();

        collect_parents(&records, &mut parents);

        assert_eq!(parents, HashSet::from([refs_parent, 0x0026_0000_0000_ca55]));
    }

    #[test]
    fn truncated_or_unknown_records_are_ignored_without_reading_past_the_buffer() {
        let mut records = record(4, 7, 64);
        records.extend(record(3, 9, 80));
        records.truncate(64 + 40);
        let mut parents = HashSet::new();

        collect_parents(&records, &mut parents);

        assert!(parents.is_empty());
    }
}
