use super::policy::{instance_family_keys, os_family_keys};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

fn read_at(file: &mut File, offset: u64, length: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0; length];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}
fn u16_at(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?) as usize)
}
fn u32_at(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?) as usize)
}

/// Name and fvar tables only. Collection and table bounds match the desktop scanner.
pub(super) fn keys(path: &Path) -> (Vec<String>, Vec<String>) {
    let mut exact = Vec::new();
    let mut os = Vec::new();
    let Ok(mut file) = File::open(path) else {
        return (exact, os);
    };
    let offsets = if read_at(&mut file, 0, 4).as_deref() == Some(b"ttcf") {
        let count = read_at(&mut file, 8, 4)
            .and_then(|bytes| u32_at(&bytes, 0))
            .unwrap_or(0);
        if count == 0 || count > 64 {
            return (exact, os);
        }
        let Some(dir) = read_at(&mut file, 12, 4 * count) else {
            return (exact, os);
        };
        (0..count)
            .filter_map(|i| u32_at(&dir, 4 * i))
            .collect::<Vec<_>>()
    } else {
        vec![0]
    };
    for offset in offsets {
        let Some(header) = read_at(&mut file, offset as u64, 12) else {
            continue;
        };
        let Some(count) = u16_at(&header, 4).filter(|n| *n > 0 && *n <= 64) else {
            continue;
        };
        let Some(dir) = read_at(&mut file, offset as u64 + 12, count * 16) else {
            continue;
        };
        let mut name = None;
        let mut fvar = None;
        for row in dir.chunks_exact(16) {
            let (Some(pos), Some(len)) = (u32_at(row, 8), u32_at(row, 12)) else {
                continue;
            };
            match &row[..4] {
                b"name" if (6..=1 << 20).contains(&len) => {
                    name = read_at(&mut file, pos as u64, len)
                }
                b"fvar" if (16..=1 << 16).contains(&len) => {
                    fvar = read_at(&mut file, pos as u64, len)
                }
                _ => {}
            }
        }
        let Some(name) = name else { continue };
        let (Some(count), Some(base)) = (u16_at(&name, 2), u16_at(&name, 4)) else {
            continue;
        };
        let mut names = HashMap::<usize, Vec<String>>::new();
        for i in 0..count {
            let at = 6 + i * 12;
            let (Some(platform), Some(encoding), Some(id), Some(len), Some(relative)) = (
                u16_at(&name, at),
                u16_at(&name, at + 2),
                u16_at(&name, at + 6),
                u16_at(&name, at + 8),
                u16_at(&name, at + 10),
            ) else {
                break;
            };
            let Some(bytes) = base
                .checked_add(relative)
                .and_then(|start| start.checked_add(len).and_then(|end| name.get(start..end)))
            else {
                continue;
            };
            let text = if platform == 0 || platform == 3 {
                if bytes.len() % 2 != 0 {
                    continue;
                }
                String::from_utf16_lossy(
                    &bytes
                        .chunks_exact(2)
                        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>(),
                )
            } else if platform == 1 && encoding == 0 {
                bytes.iter().map(|b| char::from(*b)).collect()
            } else {
                continue;
            };
            if !text.is_empty() {
                let values = names.entry(id).or_default();
                if !values.contains(&text) {
                    values.push(text);
                }
            }
        }
        let families = names
            .get(&1)
            .into_iter()
            .flatten()
            .chain(names.get(&16).into_iter().flatten())
            .collect::<Vec<_>>();
        if families.is_empty() {
            continue;
        }
        for family in &families {
            let key = main_core::pdf::font_locate::norm(family);
            if !key.is_empty() && !exact.contains(&key) {
                exact.push(key);
            }
            for key in os_family_keys(family) {
                if !os.contains(&key) {
                    os.push(key);
                }
            }
        }
        if let Some(fvar) = fvar
            && let (Some(offset), Some(axes), Some(axis_size), Some(count), Some(size)) = (
                u16_at(&fvar, 4),
                u16_at(&fvar, 8),
                u16_at(&fvar, 10),
                u16_at(&fvar, 12),
                u16_at(&fvar, 14),
            )
            && axes > 0
            && axes <= 64
            && axis_size >= 20
            && count > 0
            && count <= 256
            && size >= 4 + 4 * axes
            && let Some(base) = axes
                .checked_mul(axis_size)
                .and_then(|n| offset.checked_add(n))
            && count
                .checked_mul(size)
                .and_then(|n| base.checked_add(n))
                .is_some_and(|end| end <= fvar.len())
        {
                            let bases = names
                                .get(&16)
                                .map(|v| v.iter().collect::<Vec<_>>())
                                .unwrap_or_else(|| families.clone());
                            for i in 0..count {
                                if let Some(instance) = u16_at(&fvar, base + i * size)
                                    .and_then(|id| names.get(&id))
                                    .and_then(|v| v.first())
                                {
                                    for family in &bases {
                                        for key in instance_family_keys(family, instance) {
                                            if !os.contains(&key) {
                                                os.push(key);
                                            }
                                        }
                                    }
                                }
                            }
        }
    }
    (exact, os)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_bundled_family_and_skips_truncated_collection() {
        let path = std::env::temp_dir().join(format!("seu-font-family-{}.ttf", std::process::id()));
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packages/ui/src/fonts/Carlito-Regular.ttf"
        ));
        std::fs::write(&path, bytes).unwrap();
        let (exact, _) = keys(&path);
        assert!(exact.contains(&main_core::pdf::font_locate::norm("Carlito")));
        std::fs::write(&path, b"ttcf\0\0\0\0\0\0\0\x02").unwrap();
        assert_eq!(keys(&path), (Vec::new(), Vec::new()));
        std::fs::remove_file(path).unwrap();
    }
}
