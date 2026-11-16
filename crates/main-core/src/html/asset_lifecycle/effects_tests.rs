//! Hosted native regressions. The caller must provide an isolated scratch root.
use super::*;
use crate::{html::asset_lifecycle as html, markdown::asset_lifecycle as markdown};
#[cfg(windows)]
use std::path::Path;
use std::{cell::Cell, fs, path::PathBuf};

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("SEUOFFICE_NATIVE_TEST_SCRATCH").expect(
            "set SEUOFFICE_NATIVE_TEST_SCRATCH to an isolated hosted-run scratch directory",
        );
        let mut random = [0u8; 12];
        getrandom::fill(&mut random).unwrap();
        let random = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let root = PathBuf::from(base).join(format!("asset-{name}-{random}"));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
    fn text(&self, relative: &str) -> String {
        self.path(relative).to_str().unwrap().to_owned()
    }
    fn file(&self, relative: &str, bytes: &[u8]) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        #[cfg(windows)]
        fn writable(path: &Path) {
            if let Ok(meta) = fs::symlink_metadata(path) {
                if meta.is_dir() && !meta.file_type().is_symlink() {
                    if let Ok(entries) = fs::read_dir(path) {
                        for entry in entries.flatten() {
                            writable(&entry.path());
                        }
                    }
                } else if meta.is_file() && meta.permissions().readonly() {
                    let mut permissions = meta.permissions();
                    permissions.set_readonly(false);
                    let _ = fs::set_permissions(path, permissions);
                }
            }
        }
        #[cfg(windows)]
        writable(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn allow(effect: &mut dyn FnMut() -> R<()>) -> R<()> {
    effect()
}
fn manifest(root: &Scratch, document: &str) -> PathBuf {
    root.path(document)
        .parent()
        .unwrap()
        .join("assets")
        .join(html::ASSET_MANIFEST_FILENAME)
}

#[test]
fn refused_prepare_does_not_create_or_publish_assets() {
    let root = Scratch::new("refused");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target")).unwrap();
    let denied = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut |_| Err(refused("request ended")),
    )
    .unwrap_err();
    assert!(denied.message.contains("request ended"));
    assert!(!root.path("target/assets").exists());
}

#[test]
fn exclusive_asset_publication_keeps_existing_collision_bytes() {
    let root = Scratch::new("collision");
    root.file("source/image.png", b"original");
    root.file("target/assets/image.png", b"other writer");
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    assert_eq!(
        fs::read(root.path("target/assets/image.png")).unwrap(),
        b"other writer"
    );
    assert_eq!(plan.created.len(), 1);
    assert_ne!(plan.created[0].name, "image.png");
    assert_eq!(
        fs::read(root.path("target/assets").join(&plan.created[0].name)).unwrap(),
        b"original"
    );
}

#[test]
fn collision_after_staging_keeps_other_writer_and_replans_name() {
    let root = Scratch::new("late-collision");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let collision = root.path("target/assets/image.png");
    let mut effects_seen = 0;
    let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
        effects_seen += 1;
        if effects_seen == 2 {
            fs::write(&collision, b"other writer").unwrap();
        }
        effect()
    };
    let prepared = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut admission,
    )
    .unwrap();
    assert_eq!(fs::read(&collision).unwrap(), b"other writer");
    assert_eq!(prepared.created.len(), 1);
    assert_ne!(prepared.created[0].name, "image.png");
    assert_eq!(
        fs::read(root.path("target/assets").join(&prepared.created[0].name)).unwrap(),
        b"original"
    );
}

#[cfg(unix)]
#[test]
fn pinned_source_changed_before_stage_is_not_published() {
    let root = Scratch::new("source-changed");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let source_root = fs::canonicalize(root.path("source")).unwrap();
    let source = source_root.join("image.png").to_str().unwrap().to_owned();
    let pinned = PinnedSource::capture(source_root.to_str().unwrap(), &source).unwrap();
    let mut session = Session::open(&root.text("target/doc.html"), None).unwrap();
    let mut changed = false;
    let mut admit = |effect: &mut dyn FnMut() -> R<()>| {
        if !changed {
            changed = true;
            fs::write(&source, b"changed during admission").unwrap();
        }
        effect()
    };
    let mut effects = Effects::new(&mut admit);
    let error = session
        .copy_pinned(&pinned, "image.png", &mut effects)
        .unwrap_err();
    assert!(error.message.contains("asset source changed"));
    assert!(!root.path("target/assets/image.png").exists());
}

#[cfg(unix)]
#[test]
fn pinned_source_never_follows_replaced_components() {
    use std::os::unix::fs::symlink;
    let root = Scratch::new("source-symlink");
    root.file("source/images/a.png", b"allowed");
    root.file("outside/a.png", b"private");
    fs::rename(root.path("source/images"), root.path("source/original")).unwrap();
    symlink(root.path("outside"), root.path("source/images")).unwrap();
    let source_root = fs::canonicalize(root.path("source")).unwrap();
    let source = source_root.join("images/a.png");
    assert!(native::pin_source(source_root.to_str().unwrap(), source.to_str().unwrap()).is_err());
}

#[cfg(unix)]
#[test]
fn pinned_source_refuses_changed_content() {
    let root = Scratch::new("source-version");
    root.file("source/image.png", b"original");
    let source_root = fs::canonicalize(root.path("source")).unwrap();
    let source_path = source_root.join("image.png");
    let source =
        native::pin_source(source_root.to_str().unwrap(), source_path.to_str().unwrap()).unwrap();
    fs::write(root.path("source/image.png"), b"changed").unwrap();
    assert!(source.validate().is_err());
}

#[cfg(unix)]
fn quarantine_public_image(
    public: &native::Directory,
    private: &native::Directory,
) -> native::Quarantine {
    match native::Quarantine::move_from(public, "image.png", private, "payload") {
        Ok(receipt) => receipt,
        Err(failure) => panic!(
            "quarantine move failed: {}; inverse retained: {}",
            failure.error.message,
            failure.retained.is_some()
        ),
    }
}

#[cfg(unix)]
#[test]
fn quarantine_conflict_keeps_both_objects_and_retains_inverse() {
    let root = Scratch::new("quarantine-conflict");
    root.file("public/image.png", b"original");
    fs::create_dir_all(root.path("private")).unwrap();
    let public = native::Directory::open(&root.text("public"), true).unwrap();
    let private = native::Directory::open(&root.text("private"), true).unwrap();
    let original = public.read("image.png").unwrap().unwrap();
    let receipt = quarantine_public_image(&public, &private);
    assert!(receipt.same_object(&original).unwrap());
    root.file("public/image.png", b"replacement");
    let receipt = match receipt.restore() {
        Err(failure) if failure.1.code == Some("EEXIST") => failure.0,
        _ => panic!("restore must retain its receipt on conflict"),
    };
    assert_eq!(
        fs::read(root.path("public/image.png")).unwrap(),
        b"replacement"
    );
    assert_eq!(fs::read(root.path("private/payload")).unwrap(), b"original");
    fs::remove_file(root.path("public/image.png")).unwrap();
    assert!(receipt.restore().is_ok());
    assert_eq!(
        fs::read(root.path("public/image.png")).unwrap(),
        b"original"
    );
}

#[cfg(unix)]
#[test]
fn quarantine_detects_public_replacement_and_restores_it() {
    let root = Scratch::new("quarantine-replacement");
    root.file("public/image.png", b"original");
    fs::create_dir_all(root.path("private")).unwrap();
    let public = native::Directory::open(&root.text("public"), true).unwrap();
    let private = native::Directory::open(&root.text("private"), true).unwrap();
    let original = public.read("image.png").unwrap().unwrap();
    fs::rename(
        root.path("public/image.png"),
        root.path("public/original.png"),
    )
    .unwrap();
    root.file("public/image.png", b"replacement");
    let receipt = quarantine_public_image(&public, &private);
    assert!(!receipt.same_object(&original).unwrap());
    assert!(receipt.restore().is_ok());
    assert_eq!(
        fs::read(root.path("public/image.png")).unwrap(),
        b"replacement"
    );
    assert_eq!(
        fs::read(root.path("public/original.png")).unwrap(),
        b"original"
    );
}

#[cfg(unix)]
#[test]
fn quarantine_restores_replacement_symlink_without_touching_target() {
    use std::os::unix::fs::symlink;
    let root = Scratch::new("quarantine-symlink");
    root.file("public/image.png", b"original");
    root.file("outside", b"unrelated");
    fs::create_dir_all(root.path("private")).unwrap();
    let public = native::Directory::open(&root.text("public"), true).unwrap();
    let private = native::Directory::open(&root.text("private"), true).unwrap();
    let original = public.read("image.png").unwrap().unwrap();
    fs::rename(
        root.path("public/image.png"),
        root.path("public/original.png"),
    )
    .unwrap();
    symlink(root.path("outside"), root.path("public/image.png")).unwrap();
    let receipt = quarantine_public_image(&public, &private);
    assert!(!receipt.same_object(&original).unwrap());
    assert!(receipt.restore().is_ok());
    assert!(
        fs::symlink_metadata(root.path("public/image.png"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(root.path("outside")).unwrap(), b"unrelated");
}

#[cfg(unix)]
#[test]
fn save_as_pins_all_sources_before_first_mutation_for_both_editors() {
    let root = Scratch::new("all-sources");
    root.file("source/first.png", b"first");
    root.file("source/second.png", b"second");
    fs::create_dir_all(root.path("html-target")).unwrap();
    fs::create_dir_all(root.path("md-target")).unwrap();
    let second = root.path("source/second.png");
    let run = |markdown_mode: bool| {
        fs::write(&second, b"second").unwrap();
        let mut changed = false;
        let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
            if !changed {
                changed = true;
                fs::write(&second, b"modified later").unwrap();
            }
            effect()
        };
        let error = if markdown_mode {
            markdown::prepare_assets_for_save_as_authorized(
                &root.text("source/doc.md"),
                &root.text("md-target/doc.md"),
                "![a](first.png) ![b](second.png)",
                &[],
                &mut admission,
            )
            .unwrap_err()
        } else {
            html::prepare_assets_for_save_as_authorized(
                &root.text("source/doc.html"),
                &root.text("html-target/doc.html"),
                "<img src=\"first.png\"><img src=\"second.png\">",
                &[],
                &mut admission,
            )
            .unwrap_err()
        };
        assert!(error.message.contains("asset source changed"));
    };
    run(false);
    run(true);
}

#[test]
fn rollback_preserves_a_changed_prepared_asset() {
    let root = Scratch::new("changed");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target")).unwrap();
    let plan = markdown::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.md"),
        &root.text("target/doc.md"),
        "![x](image.png)",
        &[],
        &mut allow,
    )
    .unwrap();
    let asset = root.path("target/assets").join(&plan.created[0].name);
    fs::write(&asset, b"changed externally").unwrap();
    markdown::rollback_prepared_save_as_assets_authorized(&plan, &mut allow).unwrap();
    assert_eq!(fs::read(asset).unwrap(), b"changed externally");
}

#[test]
fn rollback_preserves_an_asset_referenced_by_another_document() {
    let root = Scratch::new("referenced");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target")).unwrap();
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    let path = manifest(&root, "target/doc.html");
    let mut saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    saved["documents"]["other.html"] = serde_json::json!([plan.created[0].name]);
    fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
    html::rollback_prepared_save_as_assets_authorized(&plan, &mut allow).unwrap();
    assert_eq!(
        fs::read(root.path("target/assets").join(&plan.created[0].name)).unwrap(),
        b"original"
    );
}

#[test]
fn revoked_rollback_does_not_delete_and_reports_retained_asset() {
    let root = Scratch::new("rollback-revoked");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target")).unwrap();
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    let error = html::rollback_prepared_save_as_assets_authorized(&plan, &mut |_| {
        Err(refused("request ended"))
    })
    .unwrap_err();
    assert!(error.message.contains("request ended"));
    assert!(error.message.contains(&plan.created[0].name));
    assert_eq!(
        fs::read(root.path("target/assets").join(&plan.created[0].name)).unwrap(),
        b"original"
    );
}

#[test]
fn source_pending_cleanup_only_resolves_the_starting_snapshot() {
    let root = Scratch::new("pending");
    root.file("source/first.png", b"first");
    root.file("source/later.png", b"later");
    fs::create_dir_all(root.path("target")).unwrap();
    let first = markdown::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.md"),
        &root.text("target/doc.md"),
        "![x](first.png)",
        &[],
        &mut allow,
    )
    .unwrap();
    let starting = markdown::pending_owned_assets_for_document(&root.text("target/doc.md"));
    let later = markdown::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.md"),
        &root.text("target/doc.md"),
        "![x](later.png)",
        &[],
        &mut allow,
    )
    .unwrap();
    let result = markdown::resolve_source_pending_after_save_as_authorized(
        &root.text("target/doc.md"),
        &starting,
        &mut allow,
    )
    .unwrap();
    assert!(result.deleted.contains(&first.created[0].name));
    assert!(
        !root
            .path("target/assets")
            .join(&first.created[0].name)
            .exists()
    );
    assert!(
        root.path("target/assets")
            .join(&later.created[0].name)
            .exists()
    );
    assert_eq!(
        markdown::pending_owned_assets_for_document(&root.text("target/doc.md")),
        vec![later.created[0].name.clone()]
    );
}

#[cfg(unix)]
#[test]
fn original_parent_symlink_alias_is_allowed_but_final_asset_symlink_is_not() {
    use std::os::unix::fs::symlink;
    let root = Scratch::new("aliases");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("real")).unwrap();
    symlink(root.path("real"), root.path("alias")).unwrap();
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("alias/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    assert!(
        root.path("real/assets")
            .join(&plan.created[0].name)
            .exists()
    );
    drop(plan);
    fs::create_dir_all(root.path("linked-target")).unwrap();
    symlink(root.path("real/assets"), root.path("linked-target/assets")).unwrap();
    assert!(
        html::prepare_assets_for_save_as_authorized(
            &root.text("source/doc.html"),
            &root.text("linked-target/doc.html"),
            "<img src=\"image.png\">",
            &[],
            &mut allow,
        )
        .is_err()
    );
}

#[test]
fn manifest_fallback_opens_under_admission_and_writes_after_it_returns() {
    let root = Scratch::new("manifest-fallback");
    root.file("target/assets/manifest.json", b"before");
    let document = root.text("target/doc.html");
    let path = root.text("target/assets/manifest.json");
    let truncated_inside = Cell::new(false);
    let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
        let result = effect();
        if fs::read(&path).ok().as_deref() == Some(b"") {
            truncated_inside.set(true);
        }
        result
    };
    let mut effects = Effects::new(&mut admission);
    let mut session = Session::open(&document, None).unwrap();
    assert_eq!(session.read_manifest(&path).unwrap().unwrap(), b"before");
    let expected = session.manifest.as_ref().unwrap().1.clone();
    let directory = session.directory.as_ref().unwrap().clone();
    let stage = session.stage(&mut effects).unwrap();
    let mut writer = ManifestWriter {
        directory,
        name: "manifest.json".into(),
        path: path.clone(),
        expected,
        rescue: "manifest.json.rescue".into(),
        rescue_expected: Ok(None),
        stage,
        effects: &mut effects,
        diagnostics: Vec::new(),
    };
    writer.write_fallback(b"after").unwrap().unwrap();
    writer.remove_stage().unwrap();
    assert!(truncated_inside.get());
    assert_eq!(fs::read(&path).unwrap(), b"after");
}

#[test]
fn cleanup_only_refusal_does_not_reclassify_an_already_published_manifest() {
    let root = Scratch::new("postcommit-cleanup");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let document = root.text("target/doc.html");
    let path = root.text("target/assets/manifest.json");
    let rescue = rescue_path_for(&path);
    fs::write(&rescue, b"obsolete").unwrap();
    let published = Cell::new(false);
    let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
        if published.get() {
            return Err(refused("request ended after commit"));
        }
        let result = effect();
        if fs::read(&path).ok().as_deref() == Some(b"published") {
            published.set(true);
        }
        result
    };
    let mut effects = Effects::new(&mut admission);
    let mut session = Session::open(&document, None).unwrap();
    assert!(session.read_manifest(&path).unwrap().is_none());
    session
        .write_manifest(&path, b"published", &mut effects)
        .unwrap();
    effects.finish(Ok(())).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"published");
    assert_eq!(fs::read(&rescue).unwrap(), b"obsolete");
    assert!(
        session
            .diagnostics()
            .iter()
            .any(|message| message.contains("obsolete asset rescue retained")
                && message.contains("request ended after commit"))
    );
}

#[test]
fn stale_rescue_directory_does_not_block_manifest_publication() {
    let root = Scratch::new("postcommit-rescue-directory");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let document = root.text("target/doc.html");
    let path = root.text("target/assets/manifest.json");
    let rescue = rescue_path_for(&path);
    fs::create_dir(&rescue).unwrap();
    let mut admission = allow;
    let mut effects = Effects::new(&mut admission);
    let mut session = Session::open(&document, None).unwrap();
    assert!(session.read_manifest(&path).unwrap().is_none());
    session
        .write_manifest(&path, b"published", &mut effects)
        .unwrap();
    effects.finish(Ok(())).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"published");
    assert!(fs::metadata(&rescue).unwrap().is_dir());
    assert!(
        session
            .diagnostics()
            .iter()
            .any(|message| message.contains("obsolete asset rescue retained")
                && message.contains(&rescue))
    );
}

#[cfg(windows)]
#[test]
fn native_copyfile_stage_preserves_source_readonly_attribute() {
    let root = Scratch::new("copyfile-attributes");
    root.file("source/image.png", b"original");
    let source = root.path("source/image.png");
    let mut permissions = fs::metadata(&source).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(source, permissions).unwrap();
    fs::create_dir_all(root.path("target")).unwrap();
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    assert!(
        fs::metadata(root.path("target/assets").join(&plan.created[0].name))
            .unwrap()
            .permissions()
            .readonly()
    );
}

#[cfg(windows)]
#[test]
fn native_copy_preserves_empty_file() {
    let root = Scratch::new("copyfile-empty");
    root.file("source/image.png", b"");
    fs::create_dir_all(root.path("target")).unwrap();
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    assert_eq!(
        fs::read(root.path("target/assets").join(&plan.created[0].name)).unwrap(),
        b""
    );
}

#[cfg(windows)]
#[test]
fn native_copy_accepts_existing_nonmutating_writer() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let root = Scratch::new("copyfile-existing-writer");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target")).unwrap();
    let _writer = fs::OpenOptions::new()
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(root.path("source/image.png"))
        .unwrap();
    let plan = markdown::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.md"),
        &root.text("target/doc.md"),
        "![x](image.png)",
        &[],
        &mut allow,
    )
    .unwrap();
    assert_eq!(
        fs::read(root.path("target/assets").join(&plan.created[0].name)).unwrap(),
        b"original"
    );
}

#[cfg(windows)]
#[test]
fn native_copy_preserves_alternate_data_stream() {
    let root = Scratch::new("copyfile-stream");
    root.file("source/image.png", b"original");
    let source = root.text("source/image.png");
    fs::write(format!("{source}:metadata"), b"alternate stream").unwrap();
    fs::create_dir_all(root.path("target")).unwrap();
    let plan = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut allow,
    )
    .unwrap();
    let target = root.path("target/assets").join(&plan.created[0].name);
    assert_eq!(
        fs::read(format!("{}:metadata", target.to_str().unwrap())).unwrap(),
        b"alternate stream"
    );
}

#[cfg(windows)]
#[test]
fn native_copy_refuses_source_replaced_after_capture() {
    let root = Scratch::new("copyfile-replaced");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let source = root.path("source/image.png");
    let replacement = root.path("source/old.png");
    let mut changed = false;
    let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
        if !changed {
            changed = true;
            fs::rename(&source, &replacement).unwrap();
            fs::write(&source, b"attacker").unwrap();
        }
        effect()
    };
    let error = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut admission,
    )
    .unwrap_err();
    assert!(error.message.contains("asset source"));
    assert!(!root.path("target/assets/image.png").exists());
}

#[cfg(windows)]
#[test]
fn native_copy_refuses_same_size_source_mutation_after_capture() {
    let root = Scratch::new("copyfile-same-size");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let source = root.path("source/image.png");
    let mut changed = false;
    let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
        if !changed {
            changed = true;
            fs::write(&source, b"modified").unwrap();
        }
        effect()
    };
    let error = html::prepare_assets_for_save_as_authorized(
        &root.text("source/doc.html"),
        &root.text("target/doc.html"),
        "<img src=\"image.png\">",
        &[],
        &mut admission,
    )
    .unwrap_err();
    assert!(error.message.contains("asset source"));
    assert!(!root.path("target/assets/image.png").exists());
}

#[cfg(unix)]
#[test]
fn manifest_symlink_is_refused_without_modifying_its_target() {
    use std::os::unix::fs::symlink;
    let root = Scratch::new("manifest-link");
    root.file("source/image.png", b"original");
    root.file("outside.json", b"unrelated");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    symlink(
        root.path("outside.json"),
        manifest(&root, "target/doc.html"),
    )
    .unwrap();
    assert!(
        html::prepare_assets_for_save_as_authorized(
            &root.text("source/doc.html"),
            &root.text("target/doc.html"),
            "<img src=\"image.png\">",
            &[],
            &mut allow,
        )
        .is_err()
    );
    assert_eq!(fs::read(root.path("outside.json")).unwrap(), b"unrelated");
    assert!(!root.path("target/assets/image.png").exists());
}

#[cfg(unix)]
#[test]
fn replaced_assets_directory_is_not_redirected_into_a_new_namespace() {
    let root = Scratch::new("directory-replaced");
    root.file("source/image.png", b"original");
    fs::create_dir_all(root.path("target/assets")).unwrap();
    let count = Cell::new(0);
    let mut admission = |effect: &mut dyn FnMut() -> R<()>| {
        count.set(count.get() + 1);
        if count.get() == 2 {
            fs::rename(
                root.path("target/assets"),
                root.path("target/previous-assets"),
            )
            .unwrap();
            fs::create_dir(root.path("target/assets")).unwrap();
            fs::write(root.path("target/assets/sentinel"), b"unrelated").unwrap();
        }
        effect()
    };
    assert!(
        html::prepare_assets_for_save_as_authorized(
            &root.text("source/doc.html"),
            &root.text("target/doc.html"),
            "<img src=\"image.png\">",
            &[],
            &mut admission,
        )
        .is_err()
    );
    assert_eq!(
        fs::read(root.path("target/assets/sentinel")).unwrap(),
        b"unrelated"
    );
    assert!(!root.path("target/assets/image.png").exists());
}

#[test]
fn private_payload_drop_after_refused_publish_preserves_final_output() {
    let root = Scratch::new("private-refused");
    root.file("target/page.png", b"existing final");
    let output = CapturedAssetOutput::capture(&root.text("target/page.png")).unwrap();
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"new private bytes").unwrap();
    assert!(
        payload
            .publish(&output, true, &mut |_| Err(refused("request ended")))
            .is_err()
    );
    drop(payload);
    assert_eq!(
        fs::read(root.path("target/page.png")).unwrap(),
        b"existing final"
    );
    for entry in fs::read_dir(root.path("target")).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".seuoffice-assets-stage-")
        {
            assert!(fs::read_dir(entry.path()).unwrap().next().is_none());
        }
    }
}

#[test]
fn private_payload_cleanup_never_deletes_a_published_final_output() {
    let root = Scratch::new("private-published");
    fs::create_dir_all(root.path("target")).unwrap();
    let output = CapturedAssetOutput::capture(&root.text("target/page.png")).unwrap();
    let sibling = output.with_name("page-2.png").unwrap();
    assert!(output.with_name("../escape.png").is_err());
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"published bytes").unwrap();
    payload.publish(&sibling, true, &mut allow).unwrap();
    drop(payload);
    assert_eq!(
        fs::read(root.path("target/page-2.png")).unwrap(),
        b"published bytes"
    );
    assert!(!root.path("target/page.png").exists());
}

#[test]
fn published_handle_keeps_the_published_object_after_path_replacement() {
    use std::io::Read;
    let root = Scratch::new("private-published-handle");
    fs::create_dir_all(root.path("target")).unwrap();
    let output = CapturedAssetOutput::capture(&root.text("target/page.docx")).unwrap();
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"published document").unwrap();
    let mut published = payload
        .publish_with_file(&output, true, &mut allow)
        .unwrap();
    drop(payload);
    fs::rename(
        root.path("target/page.docx"),
        root.path("target/previous.docx"),
    )
    .unwrap();
    fs::write(root.path("target/page.docx"), b"replacement document").unwrap();
    let mut bytes = Vec::new();
    published.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"published document");
    assert_eq!(
        fs::read(root.path("target/page.docx")).unwrap(),
        b"replacement document"
    );
}

#[test]
fn admission_error_after_publication_does_not_delete_the_output() {
    let root = Scratch::new("private-published-denied");
    fs::create_dir_all(root.path("target")).unwrap();
    let output = CapturedAssetOutput::capture(&root.text("target/page.docx")).unwrap();
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"published document").unwrap();
    let result = payload.publish_with_file(&output, true, &mut |effect| {
        effect()?;
        Err(refused("request ended after publication"))
    });
    assert!(result.is_err());
    drop(payload);
    assert_eq!(
        fs::read(root.path("target/page.docx")).unwrap(),
        b"published document"
    );
}

#[test]
fn denied_handle_publication_preserves_existing_output() {
    let root = Scratch::new("private-handle-denied");
    root.file("target/page.docx", b"existing document");
    let output = CapturedAssetOutput::capture(&root.text("target/page.docx")).unwrap();
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"private document").unwrap();
    assert!(
        payload
            .publish_with_file(&output, true, &mut |_| Err(refused("request ended")))
            .is_err()
    );
    drop(payload);
    assert_eq!(
        fs::read(root.path("target/page.docx")).unwrap(),
        b"existing document"
    );
}

#[cfg(unix)]
#[test]
fn private_payload_cleanup_cannot_follow_a_payload_moved_to_a_public_path() {
    let root = Scratch::new("private-moved");
    fs::create_dir_all(root.path("target")).unwrap();
    let output = CapturedAssetOutput::capture(&root.text("target/page.png")).unwrap();
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"original owned payload").unwrap();
    let stage = fs::read_dir(root.path("target"))
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".seuoffice-assets-stage-")
        })
        .unwrap()
        .path();
    fs::rename(stage.join("payload"), root.path("target/public.png")).unwrap();
    fs::write(stage.join("payload"), b"replacement").unwrap();
    drop(payload);
    assert_eq!(
        fs::read(root.path("target/public.png")).unwrap(),
        b"original owned payload"
    );
    assert_eq!(fs::read(stage.join("payload")).unwrap(), b"replacement");
}

#[cfg(unix)]
#[test]
fn private_payload_content_witness_rejects_change_before_publication() {
    let root = Scratch::new("private-changed");
    root.file("target/page.png", b"existing final");
    let output = CapturedAssetOutput::capture(&root.text("target/page.png")).unwrap();
    let mut payload = output.create_payload(&mut allow).unwrap();
    payload.write(b"original owned payload").unwrap();
    let stage = fs::read_dir(root.path("target"))
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".seuoffice-assets-stage-")
        })
        .unwrap()
        .path();
    let error = payload
        .publish(&output, true, &mut |effect| {
            fs::write(stage.join("payload"), b"changed payload bytes").unwrap();
            effect()
        })
        .unwrap_err();
    assert!(error.message.contains("changed before publication"));
    drop(payload);
    assert_eq!(
        fs::read(root.path("target/page.png")).unwrap(),
        b"existing final"
    );
}
