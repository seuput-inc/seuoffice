//! Handle-relative, no-follow reads of installed diagram assets.
//! Each step uses the captured directory object, even if it is later renamed;
//! this does not assert that its current pathname remains beneath the root.
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path};

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Open every component beneath an absolute root without following links, then
/// read the exact bounded length from the same regular-file handle.
pub(super) fn read(root: &Path, relative: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    if !root.is_absolute() {
        return Err(invalid("asset root must be absolute"));
    }
    // Backslash and colon are rejected on every platform so Windows prefixes,
    // alternate streams and separators cannot acquire different meanings later.
    let text = relative.as_os_str().to_string_lossy();
    if text.is_empty() || text.contains('\\') || text.contains(':') || text.contains('\0') {
        return Err(invalid("invalid relative asset path"));
    }
    let mut components = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => components.push(name),
            _ => return Err(invalid("asset path must contain only relative components")),
        }
    }
    if components.is_empty() {
        return Err(invalid("empty asset path"));
    }
    let mut file = native::open(root, &components)?;
    read_file(&mut file, max_bytes)
}

fn read_file(file: &mut File, max_bytes: u64) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("asset is not a regular file"));
    }
    let len = metadata.len();
    if len > max_bytes || len > usize::MAX as u64 {
        return Err(invalid("asset exceeds size limit"));
    }
    read_exact_length(file, len as usize)
}

fn read_exact_length(reader: &mut impl Read, len: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    let mut extra = [0];
    if reader.read(&mut extra)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "asset grew while reading",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
#[path = "drawio_assets_unix.rs"]
mod native;
#[cfg(windows)]
#[path = "drawio_assets_windows.rs"]
mod native;

#[cfg(test)]
#[path = "drawio_assets_tests.rs"]
mod tests;
