//! Single-handle metadata reads.
//!
//! `std::fs::Metadata` reports a file's apparent length and, on Windows, the
//! identity of the file record behind it. It does not report how much space
//! the file occupies on the volume, and that is the number that decides
//! whether deleting something frees space.
//!
//! This module opens a path once and reads every fact the scanner needs from
//! that single handle: apparent size, allocated size, link count, file record
//! identity, last write time and the reparse point flag.

use std::io;
use std::path::Path;

use crate::model::FileId;

/// What one path looks like on the volume.
#[derive(Clone, Copy, Debug)]
pub struct FileFacts {
    /// Length of the file's contents.
    pub apparent_size: u64,
    /// Space the file occupies on the volume, in bytes, as the filesystem
    /// reports it: whole clusters, less for a compressed or sparse file.
    pub allocated_size: u64,
    /// Number of hard links to the same file record.
    pub link_count: u32,
    /// Identity of the file record, when the platform can report one.
    pub file_id: Option<FileId>,
    /// Last write time, in milliseconds since the Unix epoch.
    pub modified_unix_ms: Option<i64>,
    pub is_dir: bool,
    pub is_reparse_point: bool,
    /// The file stores its data in holes: its logical length is larger than
    /// the clusters it owns.
    pub is_sparse: bool,
    /// The file is a placeholder whose data lives elsewhere — a cloud file
    /// that the filesystem recalls on access.
    pub is_placeholder: bool,
}

#[cfg(windows)]
pub fn read(path: &Path) -> io::Result<FileFacts> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let facts = read_from_handle(handle);
    unsafe {
        CloseHandle(handle);
    }
    facts
}

#[cfg(windows)]
fn read_from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<FileFacts> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_BASIC_INFO, FILE_STANDARD_INFO, FileBasicInfo, FileStandardInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    };

    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let mut identity: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };

    // Each of these can fail on its own. A failure is not the same as a
    // zero: a file whose allocation the filesystem reports as zero really
    // does occupy nothing, and that is a different answer from "unknown".
    let basic_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileBasicInfo,
            &mut basic as *mut FILE_BASIC_INFO as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } != 0;
    let standard_ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            &mut standard as *mut FILE_STANDARD_INFO as *mut core::ffi::c_void,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    } != 0;
    let identity_ok = unsafe { GetFileInformationByHandle(handle, &mut identity) } != 0;

    let attributes = identity.dwFileAttributes;
    let apparent_size = if identity_ok {
        ((identity.nFileSizeHigh as u64) << 32) | identity.nFileSizeLow as u64
    } else {
        0
    };
    // A sparse file or a cloud placeholder owns no clusters at all. The
    // filesystem reports that as an allocation of zero, and reading it as
    // "unknown, assume the logical size" would count the whole file as if
    // it sat on the volume.
    let allocated_size = if standard_ok {
        standard.AllocationSize.max(0) as u64
    } else {
        apparent_size
    };
    let link_count = if identity_ok && identity.nNumberOfLinks > 0 {
        identity.nNumberOfLinks
    } else if standard_ok {
        standard.NumberOfLinks
    } else {
        1
    };
    let file_index = ((identity.nFileIndexHigh as u64) << 32) | identity.nFileIndexLow as u64;

    Ok(FileFacts {
        apparent_size,
        allocated_size,
        link_count,
        file_id: identity_ok.then_some(FileId {
            volume_serial: identity.dwVolumeSerialNumber,
            file_index,
        }),
        modified_unix_ms: basic_ok.then(|| windows_time_to_unix_ms(basic.LastWriteTime)).flatten(),
        is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0 || standard.Directory,
        is_reparse_point: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
        is_sparse: attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0,
        is_placeholder: attributes
            & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN)
            != 0,
    })
}

/// Windows timestamps count 100-nanosecond intervals since 1601-01-01.
#[cfg(windows)]
const WINDOWS_TO_UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

#[cfg(windows)]
fn windows_time_to_unix_ms(ticks: i64) -> Option<i64> {
    if ticks <= 0 || ticks < WINDOWS_TO_UNIX_EPOCH_TICKS {
        return None;
    }
    Some((ticks - WINDOWS_TO_UNIX_EPOCH_TICKS) / 10_000)
}

#[cfg(windows)]
fn is_ascii_letter(value: u16) -> bool {
    (value >= b'A' as u16 && value <= b'Z' as u16)
        || (value >= b'a' as u16 && value <= b'z' as u16)
}

/// Builds a NUL-terminated wide path that the Win32 file APIs accept,
/// including paths longer than `MAX_PATH` and UNC shares.
#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    let mut raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    for character in &mut raw {
        if *character == b'/' as u16 {
            *character = b'\\' as u16;
        }
    }

    const QUESTION: u16 = b'?' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    const COLON: u16 = b':' as u16;

    let mut wide: Vec<u16> = Vec::with_capacity(raw.len() + 8);
    if raw.starts_with(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]) {
        wide.extend_from_slice(&raw);
    } else if raw.starts_with(&[BACKSLASH, BACKSLASH]) {
        wide.extend_from_slice(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]);
        wide.extend_from_slice(&[b'U' as u16, b'N' as u16, b'C' as u16, BACKSLASH]);
        wide.extend_from_slice(&raw[2..]);
    } else if raw.first() == Some(&BACKSLASH)
        || (raw.len() >= 2 && raw[1] == COLON && is_ascii_letter(raw[0]))
    {
        wide.extend_from_slice(&[BACKSLASH, BACKSLASH, QUESTION, BACKSLASH]);
        wide.extend_from_slice(&raw);
    } else {
        wide.extend_from_slice(&raw);
    }
    wide.push(0);
    wide
}

#[cfg(not(windows))]
pub fn read(path: &Path) -> io::Result<FileFacts> {
    use std::time::UNIX_EPOCH;

    let metadata = std::fs::symlink_metadata(path)?;
    let modified_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    Ok(FileFacts {
        apparent_size: metadata.len(),
        allocated_size: metadata.len(),
        link_count: 1,
        file_id: None,
        modified_unix_ms,
        is_dir: metadata.is_dir(),
        is_reparse_point: metadata.file_type().is_symlink(),
        is_sparse: false,
        is_placeholder: false,
    })
}
