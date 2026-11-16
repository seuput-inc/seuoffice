use super::registry::FontSources;
use main_core::slides::font_scan::{ScanResult, ScanTask, scan_font_dirs};
use std::path::{Path, PathBuf};

/// All inputs originate in native shell configuration, never the JS callback.
pub(super) struct FontPaths {
    pub(super) home: PathBuf,
    pub(super) user: Option<PathBuf>,
    pub(super) template: Option<PathBuf>,
    pub(super) platform: main_core::pdf::font_locate::Platform,
}

pub(super) fn discover(config: &FontPaths) -> FontSources {
    use main_core::pdf::font_locate::Platform;
    let home = &config.home;
    let mut flat: Vec<(PathBuf, bool)> = match config.platform {
        Platform::Darwin => vec![
            ("/System/Library/Fonts".into(), false),
            ("/System/Library/Fonts/Supplemental".into(), false),
            ("/Library/Fonts".into(), false),
            (home.join("Library/Fonts"), false),
        ],
        Platform::Win32 => vec![
            ("C:\\Windows\\Fonts".into(), false),
            (home.join("AppData/Local/Microsoft/Windows/Fonts"), false),
        ],
        Platform::Other => vec![
            ("/usr/share/fonts".into(), false),
            ("/usr/local/share/fonts".into(), false),
            (home.join(".fonts"), false),
        ],
    };
    if let Some(user) = &config.user {
        flat.push((user.clone(), true));
    }
    if config.platform == Platform::Darwin {
        for app in ["Microsoft PowerPoint", "Microsoft Word", "Microsoft Excel"] {
            flat.push((
                PathBuf::from(format!("/Applications/{app}.app/Contents/Resources/DFonts")),
                true,
            ));
        }
        for root in main_core::pdf::font_locate::apple_font_asset_roots("/System/Library/AssetsV2")
        {
            if let Ok(entries) = std::fs::read_dir(&root) {
                let mut names: Vec<_> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .collect();
                names.sort();
                for name in names {
                    flat.push((Path::new(&root).join(name).join("AssetData"), true));
                }
            }
        }
        flat.push(("/System/Library/PrivateFrameworks/FontServices.framework/Versions/A/Resources/Fonts/Subsets".into(), true));
    }
    let mut tasks: Vec<ScanTask> = flat
        .iter()
        .map(|(dir, _)| ScanTask::Flat {
            dir: dir.to_string_lossy().into_owned(),
        })
        .collect();
    let cloud = match config.platform {
        Platform::Darwin => Some(home.join("Library/Group Containers/UBF8T346G9.Office/FontCache")),
        Platform::Win32 => Some(home.join("AppData/Local/Microsoft/FontCache")),
        Platform::Other => None,
    };
    if let Some(base) = cloud {
        tasks.push(ScanTask::Cloud {
            base: base.to_string_lossy().into_owned(),
            sub: "CloudFonts".into(),
        });
    }
    let mut sources = FontSources::default();
    for (task, result) in tasks.iter().zip(scan_font_dirs(&tasks, 3000.0)) {
        match (task, result) {
            (ScanTask::Flat { dir }, Some(ScanResult::Flat { files })) => {
                let private = flat
                    .iter()
                    .find(|(path, _)| path.as_path() == Path::new(dir))
                    .is_some_and(|(_, private)| *private);
                sources.files.extend(
                    files
                        .into_iter()
                        .map(|name| (Path::new(dir).join(name), private)),
                );
            }
            (ScanTask::Cloud { .. }, Some(ScanResult::Cloud { roots })) => {
                for root in roots {
                    for (family, paths) in root.families {
                        sources.families.push((
                            family,
                            paths.into_iter().map(PathBuf::from).collect(),
                            false,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(dir) = &config.template {
        for (family, file, weight) in
            main_core::sheets::template_fonts::TEMPLATE_FONT_FACE_REGISTRATIONS
        {
            let path = dir.join(file);
            if weight.contains(' ') {
                sources
                    .weight_ranges
                    .insert(path.clone(), weight.to_owned());
            }
            sources.families.push((family.to_owned(), vec![path], true));
        }
    }
    sources
}
