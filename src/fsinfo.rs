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

/// What a directory listing says about one entry, without opening it.
///
/// Some files cannot be opened by anyone: the paging file and the hibernation
/// file are locked by the system, and files inside protected folders refuse
/// access. Their names are still listed, and the listing carries the apparent
/// length, which is enough to keep them from vanishing from the measurement.
#[derive(Clone, Copy, Debug)]
pub struct ListedFacts {
    /// Length of the file's contents, as the listing reports it.
    pub apparent_size: u64,
    pub is_dir: bool,
    /// The file keeps its data in holes.
    pub is_sparse: bool,
    /// The file is stored compressed, so it owns fewer clusters than its
    /// length.
    pub is_compressed: bool,
    /// The entry is a reparse point: a link, or a cloud placeholder whose
    /// data lives elsewhere.
    pub is_reparse_point: bool,
    /// Last write time, in milliseconds since the Unix epoch.
    pub modified_unix_ms: Option<i64>,
}

/// Looks one entry up in its parent's directory listing.
///
/// This is the fallback for files that cannot be opened: the listing gives
/// the apparent length and the attributes, but not the cluster allocation,
/// so the caller has to treat the allocation as an estimate.
#[cfg(windows)]
pub fn listed(path: &Path) -> Option<ListedFacts> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_ATTRIBUTE_SPARSE_FILE, FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
    };
    use std::os::windows::ffi::OsStrExt;

    let name = path.file_name()?;
    let parent = path.parent().unwrap_or(Path::new(""));
    let mut raw: Vec<u16> = parent.as_os_str().encode_wide().collect();
    if raw.last() != Some(&(b'\\' as u16)) {
        raw.push(b'\\' as u16);
    }
    raw.extend(name.encode_wide());
    let pattern = extended_wide(raw);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(pattern.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let wanted: Vec<u16> = name.encode_wide().collect();
    let mut found = None;
    loop {
        if listed_name_equals(&data.cFileName, &wanted) {
            let attributes = data.dwFileAttributes;
            let written = ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
                | data.ftLastWriteTime.dwLowDateTime as u64;
            found = Some(ListedFacts {
                apparent_size: ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64,
                is_dir: attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                is_sparse: attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0,
                is_compressed: attributes & FILE_ATTRIBUTE_COMPRESSED != 0,
                is_reparse_point: attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                modified_unix_ms: windows_time_to_unix_ms(written as i64),
            });
            break;
        }
        let more = unsafe { FindNextFileW(handle, &mut data) };
        if more == 0 {
            break;
        }
    }
    unsafe {
        FindClose(handle);
    }
    found
}

/// Whether the listing entry carries exactly the name that was asked for.
#[cfg(windows)]
fn listed_name_equals(stored: &[u16], wanted: &[u16]) -> bool {
    let stored: Vec<u16> = stored.iter().copied().take_while(|c| *c != 0).collect();
    stored == wanted
}

#[cfg(not(windows))]
pub fn listed(_path: &Path) -> Option<ListedFacts> {
    None
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
    extended_wide(raw)
}

/// Adds the `\\?\` prefix when the path needs it, and terminates the string.
#[cfg(windows)]
fn extended_wide(raw: Vec<u16>) -> Vec<u16> {
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

#[cfg(all(test, windows))]
mod tests {
    use super::{listed, read};
    use std::fs;

    #[test]
    fn a_file_is_visible_in_the_directory_listing() {
        let root = std::env::temp_dir().join(format!(
            "space-tracker-listed-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture");
        let file = root.join("listed.bin");
        fs::write(&file, vec![3u8; 8192]).expect("write file");

        let facts = listed(&file).expect("listing");
        assert_eq!(facts.apparent_size, 8192);
        assert!(!facts.is_dir);
        assert!(!facts.is_sparse);
        assert!(!facts.is_compressed);
        assert!(!facts.is_reparse_point);
        assert!(facts.modified_unix_ms.is_some());

        // A directory is listed as a directory.
        let directory = listed(&root).expect("directory listing");
        assert!(directory.is_dir);

        // A path that is not there is not listed.
        assert!(listed(&root.join("missing.bin")).is_none());

        fs::remove_dir_all(&root).expect("remove fixture");
    }

    #[test]
    fn reading_and_listing_agree_about_a_file() {
        let root = std::env::temp_dir().join(format!(
            "space-tracker-agree-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create fixture");
        let file = root.join("agree.bin");
        fs::write(&file, vec![5u8; 4096]).expect("write file");

        let facts = read(&file).expect("read file");
        let listed_facts = listed(&file).expect("listing");
        assert_eq!(facts.apparent_size, listed_facts.apparent_size);

        fs::remove_dir_all(&root).expect("remove fixture");
    }
}
