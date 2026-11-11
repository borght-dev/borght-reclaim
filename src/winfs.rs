use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
    HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteFileW, ExtendedFileIdType, FileDispositionInfoEx, FileFullDirectoryInfo,
    FileIdExtdDirectoryInfo, FileIdInfo, GetDiskFreeSpaceExW, GetDriveTypeW, GetFileAttributesW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetLogicalDrives, MoveFileExW,
    OpenFileById, RemoveDirectoryW, SetFileAttributesW, SetFileInformationByHandle,
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_DESCRIPTOR, FILE_ID_INFO, FILE_INFO_BY_HANDLE_CLASS, FILE_LIST_DIRECTORY,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, INVALID_FILE_ATTRIBUTES, OPEN_EXISTING,
};
use windows_sys::Win32::UI::Shell::{
    SHEmptyRecycleBinW, SHERB_NOCONFIRMATION, SHERB_NOPROGRESSUI, SHERB_NOSOUND,
};

const SHARE_EVERYTHING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const DIRECTORY_BUFFER_BYTES: usize = 256 * 1024;
const BACKSLASH: u16 = b'\\' as u16;

const DISPOSITION_DELETE: u32 = 0x1;
const DISPOSITION_POSIX_SEMANTICS: u32 = 0x2;
const DISPOSITION_IGNORE_READONLY: u32 = 0x10;

/// A `\\?\`-prefixed UTF-16 path without a terminating NUL.
#[derive(Clone)]
pub struct WidePath(Vec<u16>);

impl WidePath {
    pub fn new(path: &Path) -> Self {
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let units: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        let verbatim: &[u16] = &[BACKSLASH, BACKSLASH, b'?' as u16, BACKSLASH];
        if units.starts_with(verbatim) {
            return Self(units);
        }
        let mut prefixed = verbatim.to_vec();
        if units.starts_with(&[BACKSLASH, BACKSLASH]) {
            prefixed.extend("UNC".encode_utf16());
            prefixed.extend_from_slice(&units[1..]);
        } else {
            prefixed.extend_from_slice(&units);
        }
        Self(prefixed)
    }

    pub fn join(&self, name: &[u16]) -> Self {
        let mut joined = Vec::with_capacity(self.0.len() + 1 + name.len());
        joined.extend_from_slice(&self.0);
        if joined.last() != Some(&BACKSLASH) {
            joined.push(BACKSLASH);
        }
        joined.extend_from_slice(name);
        Self(joined)
    }

    fn with_terminator<T>(&self, call: impl FnOnce(*const u16) -> T) -> T {
        let mut terminated = Vec::with_capacity(self.0.len() + 1);
        terminated.extend_from_slice(&self.0);
        terminated.push(0);
        call(terminated.as_ptr())
    }
}

pub struct RawEntry<'a> {
    pub name: &'a [u16],
    pub attributes: u32,
    pub allocated_bytes: u64,
    pub modified: i64,
    pub file_id: u128,
}

impl RawEntry<'_> {
    pub fn is_directory(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    pub fn is_link(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

thread_local! {
    static DIRECTORY_BUFFER: RefCell<Vec<u64>> = RefCell::new(vec![0u64; DIRECTORY_BUFFER_BYTES / 8]);
}

pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Kernel handles may be used from any thread; only the raw pointer type says otherwise.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

pub fn open_directory(path: &WidePath) -> io::Result<OwnedHandle> {
    open(path, FILE_LIST_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS)
}

/// Opens a directory by its file id, which keeps working after it or a parent was renamed.
/// `on_same_volume` is any open handle on that volume.
pub fn open_directory_by_id(
    on_same_volume: &OwnedHandle,
    file_id: u128,
) -> io::Result<OwnedHandle> {
    let mut descriptor: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    descriptor.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
    descriptor.Type = ExtendedFileIdType;
    descriptor.Anonymous.ExtendedFileId.Identifier = file_id.to_le_bytes();
    let handle = unsafe {
        OpenFileById(
            on_same_volume.0,
            &descriptor,
            FILE_LIST_DIRECTORY,
            SHARE_EVERYTHING,
            null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

pub fn file_id(handle: &OwnedHandle) -> io::Result<u128> {
    let mut information: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileIdInfo,
            &mut information as *mut FILE_ID_INFO as *mut c_void,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u128::from_le_bytes(information.FileId.Identifier))
}

/// The volume device itself, with just enough access to read its change journal unelevated.
pub fn open_volume_for_journal(path_on_volume: &Path) -> io::Result<OwnedHandle> {
    const SYNCHRONIZE_AND_TRAVERSE: u32 = 0x0010_0020;
    let root = volume_root(path_on_volume);
    let drive = root.to_string_lossy();
    let drive = drive.trim_end_matches('\\');
    if drive.len() != 2 || !drive.ends_with(':') {
        return Err(io::Error::other(format!("{} is not on a lettered drive", root.display())));
    }
    let device: Vec<u16> = format!(r"\\.\{drive}").encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            SYNCHRONIZE_AND_TRAVERSE,
            SHARE_EVERYTHING,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

fn open(path: &WidePath, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let handle = path.with_terminator(|pointer| unsafe {
        CreateFileW(pointer, access, SHARE_EVERYTHING, null(), OPEN_EXISTING, flags, null_mut())
    });
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedHandle(handle))
}

struct EntryLayout {
    info_class: FILE_INFO_BY_HANDLE_CLASS,
    file_id_offset: Option<usize>,
    name_offset: usize,
}

const WITH_FILE_IDS: EntryLayout =
    EntryLayout { info_class: FileIdExtdDirectoryInfo, file_id_offset: Some(72), name_offset: 88 };
const WITHOUT_FILE_IDS: EntryLayout =
    EntryLayout { info_class: FileFullDirectoryInfo, file_id_offset: None, name_offset: 68 };

const NEXT_ENTRY_OFFSET: usize = 0;
const LAST_WRITE_OFFSET: usize = 24;
const ALLOCATION_SIZE_OFFSET: usize = 48;
const ATTRIBUTES_OFFSET: usize = 56;
const NAME_LENGTH_OFFSET: usize = 60;

/// Lists a directory with one syscall per ~256 KB of entries, skipping `.` and `..`.
/// `on_entry` must not list another directory itself: the buffer is shared per thread.
pub fn read_directory(path: &WidePath, on_entry: impl FnMut(RawEntry<'_>)) -> io::Result<()> {
    read_open_directory(&open_directory(path)?, on_entry)
}

pub fn read_open_directory(
    directory: &OwnedHandle,
    on_entry: impl FnMut(RawEntry<'_>),
) -> io::Result<()> {
    DIRECTORY_BUFFER
        .with(|buffer| read_directory_into(directory, &mut buffer.borrow_mut(), on_entry))
}

fn read_directory_into(
    directory: &OwnedHandle,
    buffer: &mut [u64],
    mut on_entry: impl FnMut(RawEntry<'_>),
) -> io::Result<()> {
    let mut layout = &WITH_FILE_IDS;
    let mut received_any_batch = false;
    loop {
        let filled = unsafe {
            GetFileInformationByHandleEx(
                directory.0,
                layout.info_class,
                buffer.as_mut_ptr() as *mut c_void,
                DIRECTORY_BUFFER_BYTES as u32,
            )
        };
        if filled == 0 {
            let error = unsafe { GetLastError() };
            if error == ERROR_NO_MORE_FILES {
                return Ok(());
            }
            let file_ids_unsupported = !received_any_batch && layout.file_id_offset.is_some();
            if error == ERROR_INVALID_PARAMETER && file_ids_unsupported {
                layout = &WITHOUT_FILE_IDS;
                continue;
            }
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        received_any_batch = true;
        let bytes = buffer.as_ptr() as *const u8;
        let mut offset = 0usize;
        loop {
            let entry = unsafe { bytes.add(offset) };
            let next = unsafe { read::<u32>(entry, NEXT_ENTRY_OFFSET) } as usize;
            let name_units = unsafe { read::<u32>(entry, NAME_LENGTH_OFFSET) } as usize / 2;
            let name = unsafe {
                std::slice::from_raw_parts(entry.add(layout.name_offset) as *const u16, name_units)
            };
            if !is_dot_entry(name) {
                on_entry(RawEntry {
                    name,
                    attributes: unsafe { read::<u32>(entry, ATTRIBUTES_OFFSET) },
                    allocated_bytes: unsafe { read::<i64>(entry, ALLOCATION_SIZE_OFFSET) } as u64,
                    modified: unsafe { read::<i64>(entry, LAST_WRITE_OFFSET) },
                    file_id: layout
                        .file_id_offset
                        .map_or(0, |id_offset| unsafe { read::<u128>(entry, id_offset) }),
                });
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}

unsafe fn read<T: Copy>(entry: *const u8, offset: usize) -> T {
    std::ptr::read_unaligned(entry.add(offset) as *const T)
}

fn is_dot_entry(name: &[u16]) -> bool {
    const DOT: u16 = b'.' as u16;
    matches!(name, [DOT] | [DOT, DOT])
}

pub fn attributes(path: &WidePath) -> io::Result<u32> {
    let attributes = path.with_terminator(|pointer| unsafe { GetFileAttributesW(pointer) });
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    Ok(attributes)
}

pub fn is_link(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn is_directory(attributes: u32) -> bool {
    attributes & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// Fails while any file below `from` is open or is a process's working directory,
/// which makes it an atomic "nobody is using this" probe.
pub fn rename(from: &WidePath, to: &WidePath) -> io::Result<()> {
    let moved = from.with_terminator(|from_pointer| {
        to.with_terminator(|to_pointer| unsafe { MoveFileExW(from_pointer, to_pointer, 0) })
    });
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn delete_file(path: &WidePath) -> io::Result<()> {
    if path.with_terminator(|pointer| unsafe { DeleteFileW(pointer) }) != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
        return Err(error);
    }
    delete_ignoring_readonly(path).or_else(|_| {
        path.with_terminator(|pointer| unsafe {
            SetFileAttributesW(pointer, FILE_ATTRIBUTE_NORMAL);
            if DeleteFileW(pointer) != 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        })
    })
}

fn delete_ignoring_readonly(path: &WidePath) -> io::Result<()> {
    let handle = open(path, DELETE, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)?;
    let disposition: u32 =
        DISPOSITION_DELETE | DISPOSITION_POSIX_SEMANTICS | DISPOSITION_IGNORE_READONLY;
    let applied = unsafe {
        SetFileInformationByHandle(
            handle.0,
            FileDispositionInfoEx,
            &disposition as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if applied == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes an empty directory, or a junction/symlink without touching its target.
pub fn remove_directory(path: &WidePath) -> io::Result<()> {
    if path.with_terminator(|pointer| unsafe { RemoveDirectoryW(pointer) }) != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_ACCESS_DENIED as i32) {
        return Err(error);
    }
    delete_ignoring_readonly(path).map_err(|_| error)
}

pub fn hard_link_count(path: &WidePath) -> io::Result<u32> {
    let handle = open(path, 0, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)?;
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.0, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(information.nNumberOfLinks)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VolumeSpace {
    pub total_bytes: u64,
    pub free_bytes: u64,
}

pub fn volume_space(any_path_on_volume: &Path) -> io::Result<VolumeSpace> {
    let mut free_bytes = 0u64;
    let mut total_bytes = 0u64;
    let queried =
        WidePath::new(&volume_root(any_path_on_volume)).with_terminator(|pointer| unsafe {
            GetDiskFreeSpaceExW(pointer, &mut free_bytes, &mut total_bytes, null_mut())
        });
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(VolumeSpace { total_bytes, free_bytes })
}

pub fn volume_root(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute.ancestors().last().map(Path::to_path_buf).unwrap_or(absolute)
}

/// Roots of the local disks, without removable, optical and network drives.
pub fn fixed_drive_roots() -> Vec<PathBuf> {
    const DRIVE_FIXED: u32 = 3;
    const LETTERS: u8 = 26;
    let present = unsafe { GetLogicalDrives() };
    (0..LETTERS)
        .filter(|letter| present & (1 << letter) != 0)
        .map(|letter| format!("{}:\\", char::from(b'A' + letter)))
        .filter(|root| {
            let terminated: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            unsafe { GetDriveTypeW(terminated.as_ptr()) == DRIVE_FIXED }
        })
        .map(PathBuf::from)
        .collect()
}

pub fn empty_recycle_bin(volume_root: &Path) -> io::Result<()> {
    let root: Vec<u16> = volume_root.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = SHERB_NOCONFIRMATION | SHERB_NOPROGRESSUI | SHERB_NOSOUND;
    let result = unsafe { SHEmptyRecycleBinW(null_mut(), root.as_ptr(), flags) };
    const ALREADY_EMPTY: i32 = 0x8000FFFFu32 as i32;
    if result < 0 && result != ALREADY_EMPTY {
        return Err(io::Error::other(format!("SHEmptyRecycleBin failed: 0x{result:08X}")));
    }
    Ok(())
}

/// A scan is a short burst the user is waiting on; without this it queues behind every build.
pub fn raise_current_thread_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
}

const FILETIME_TICKS_PER_SECOND: i64 = 10_000_000;
const FILETIME_UNIX_EPOCH_SECONDS: i64 = 11_644_473_600;

pub fn filetime_to_unix_seconds(filetime: i64) -> i64 {
    filetime / FILETIME_TICKS_PER_SECOND - FILETIME_UNIX_EPOCH_SECONDS
}

pub fn unix_seconds_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
