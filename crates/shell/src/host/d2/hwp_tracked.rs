//! `apps/hwp/src/main/tracked-changes.ts`, using the port of SheetJS CFB.

use main_core::hwp::{cfb, hwp5_file};
use main_core::packages::compound_file::assert_compound_file;
use shell_ipc_d2::hwp::TrackedChanges;

const TRACKED: u32 = 0x4000;

fn has_view_text(path: &str) -> bool {
    path.to_ascii_lowercase().contains("/viewtext/")
}

fn tracked_container(file: &[u8]) -> Result<Option<cfb::Container>, ()> {
    assert_compound_file(file).map_err(|_| ())?;
    let container = cfb::read(file).map_err(|_| ())?;
    let Some(header) = cfb::find(&container, hwp5_file::FILE_HEADER)
        .and_then(|index| container.file_index[index].content.as_deref()) else {
        return Ok(None);
    };
    if header.len() < 40 || !header.starts_with(hwp5_file::SIGNATURE.as_bytes())
        || hwp5_file::read_u32(header, 36).map_err(|_| ())? & TRACKED == 0
        || !container.full_paths.iter().any(|path| has_view_text(path))
    {
        return Ok(None);
    }
    Ok(Some(container))
}

pub(super) fn tracked(file: &[u8]) -> TrackedChanges {
    match tracked_container(file) {
        Ok(Some(_)) => TrackedChanges::Tracked,
        Ok(None) => TrackedChanges::None,
        Err(()) => TrackedChanges::Unreadable,
    }
}

pub(super) fn without(file: &[u8]) -> Option<Vec<u8>> {
    let mut container = tracked_container(file).ok()??;
    let mut bodies = Vec::new();
    for index in 0..hwp5_file::MAX_SECTIONS {
        let path = hwp5_file::section(index);
        let Some(content) = cfb::find(&container, &path)
            .and_then(|at| container.file_index[at].content.as_ref()) else { break; };
        bodies.push((path, content.clone()));
    }
    if bodies.is_empty() { return None; }
    for index in (0..container.full_paths.len()).rev() {
        if has_view_text(&container.full_paths[index]) {
            container.full_paths.remove(index);
            container.file_index.remove(index);
        }
    }
    // Equal nonnegative links make the CFB writer rebuild the directory tree.
    for entry in &mut container.file_index { entry.l = 0; entry.r = 0; entry.c = 0; }
    let header = cfb::find(&container, hwp5_file::FILE_HEADER)?;
    let header = container.file_index[header].content.as_mut()?;
    let flags = hwp5_file::read_u32(header, 36).ok()?;
    hwp5_file::write_u32(header, flags & !TRACKED, 36).ok()?;
    let written = hwp5_file::write_container(&mut container).ok()?;
    let check = hwp5_file::open_hwp(&written).ok()??;
    if hwp5_file::section_count(&check) != bodies.len() { return None; }
    for (path, body) in bodies {
        let current = cfb::find(&check.cfb, &path)
            .and_then(|index| check.cfb.file_index[index].content.as_ref())?;
        if *current != body { return None; }
    }
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document() -> Vec<u8> {
        let mut header = vec![0; 40];
        header[..hwp5_file::SIGNATURE.len()].copy_from_slice(hwp5_file::SIGNATURE.as_bytes());
        hwp5_file::write_u32(&mut header, TRACKED, 36).unwrap();
        let mut container = cfb::Container { full_paths: Vec::new(), file_index: Vec::new() };
        for (path, kind, content) in [
            ("Root Entry/", 5, None),
            ("Root Entry/FileHeader", 2, Some(header)),
            ("Root Entry/BodyText/", 1, None),
            ("Root Entry/BodyText/Section0", 2, Some(b"current text".to_vec())),
            ("Root Entry/ViewText/", 1, None),
            ("Root Entry/ViewText/Section0", 2, Some(b"old and deleted".to_vec())),
        ] {
            let mut entry = cfb::Entry::unused(path.rsplit('/').find(|part| !part.is_empty()).unwrap());
            entry.kind = kind;
            entry.content = content;
            container.full_paths.push(path.into());
            container.file_index.push(entry);
        }
        hwp5_file::write_container(&mut container).unwrap()
    }

    #[test]
    fn stripping_tracked_changes_preserves_current_body() {
        let input = document();
        assert!(matches!(tracked(&input), TrackedChanges::Tracked));
        let output = without(&input).unwrap();
        assert!(matches!(tracked(&output), TrackedChanges::None));
        let opened = hwp5_file::open_hwp(&output).unwrap().unwrap();
        assert_eq!(hwp5_file::stream_of(&opened, &hwp5_file::section(0)).unwrap().unwrap(), b"current text");
    }

    #[test]
    fn non_compound_input_is_unreadable_and_never_rewritten() {
        assert!(matches!(tracked(b"not an HWP"), TrackedChanges::Unreadable));
        assert!(without(b"not an HWP").is_none());
    }
}
