use super::*;

fn allow(effect: &mut dyn FnMut() -> Result<(), NodeError>) -> Result<(), NodeError> {
    effect()
}
fn fixture() -> String {
    crate::node_fs::mkdtemp_random(&join(&[
        &std::env::temp_dir().to_string_lossy(),
        "seuoffice-owned-images-",
    ]))
    .unwrap()
}
fn environment() -> ImageExportEnv<'static> {
    fn id() -> String {
        "owned-image-session".into()
    }
    fn suffix() -> String {
        "abcdef".into()
    }
    ImageExportEnv {
        random_uuid: &id,
        mkdtemp_suffix: &suffix,
    }
}
const PNG: &str = "iVBORw0KGgo=";

#[test]
fn captured_png_pages_publish_only_at_finish_with_original_numbering() {
    let root = fixture();
    let target = join(&[&root, "drawing.png"]);
    std::fs::write(&target, b"existing base file").unwrap();
    let mut sessions = ImageExportSessions::new();
    let id = sessions
        .start_files_authorized(&environment(), 7.0, &target, &mut allow)
        .unwrap();
    sessions
        .write_authorized(7.0, &id, 1.0, Some(PNG), &mut allow)
        .unwrap();
    sessions
        .write_authorized(7.0, &id, 2.0, Some(PNG), &mut allow)
        .unwrap();
    let numbered = vec![
        join(&[&root, "drawing-01.png"]),
        join(&[&root, "drawing-02.png"]),
    ];
    assert!(
        numbered
            .iter()
            .all(|path| !std::path::Path::new(path).exists())
    );
    let done = sessions
        .finish_export_authorized(7.0, &id, true, &mut allow)
        .unwrap()
        .unwrap();
    assert_eq!(
        done,
        FinishedExport {
            dir: root.clone(),
            first: numbered[0].clone()
        }
    );
    for path in numbered {
        assert_eq!(std::fs::read(path).unwrap(), PNG_SIGNATURE);
    }
    assert_eq!(std::fs::read(&target).unwrap(), b"existing base file");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn revoked_png_disposal_cannot_delete_final_or_foreign_private_entries() {
    let root = fixture();
    let target = join(&[&root, "protected.png"]);
    std::fs::write(&target, b"approved original").unwrap();
    let mut sessions = ImageExportSessions::new();
    let id = sessions
        .start_files_authorized(&environment(), 9.0, &target, &mut allow)
        .unwrap();
    sessions
        .write_authorized(9.0, &id, 1.0, Some(PNG), &mut allow)
        .unwrap();
    let stage = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let foreign = stage.join("foreign.txt");
    std::fs::write(&foreign, b"not the captured payload").unwrap();
    let revoked = sessions.finish_export_authorized(9.0, &id, true, &mut |_| {
        Err(plain_error("original owner was revoked"))
    });
    assert!(revoked.is_err());
    drop(sessions);
    assert_eq!(std::fs::read(&target).unwrap(), b"approved original");
    assert_eq!(std::fs::read(foreign).unwrap(), b"not the captured payload");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_later_png_promotion_failure_does_not_remove_the_published_first_page() {
    let root = fixture();
    let target = join(&[&root, "partial.png"]);
    let numbered = [
        join(&[&root, "partial-01.png"]),
        join(&[&root, "partial-02.png"]),
    ];
    std::fs::create_dir(&numbered[1]).unwrap();
    let mut sessions = ImageExportSessions::new();
    let id = sessions
        .start_files_authorized(&environment(), 11.0, &target, &mut allow)
        .unwrap();
    sessions
        .write_authorized(11.0, &id, 1.0, Some(PNG), &mut allow)
        .unwrap();
    sessions
        .write_authorized(11.0, &id, 2.0, Some(PNG), &mut allow)
        .unwrap();
    assert!(
        sessions
            .finish_export_authorized(11.0, &id, true, &mut allow)
            .is_err()
    );
    assert_eq!(std::fs::read(&numbered[0]).unwrap(), PNG_SIGNATURE);
    assert!(std::path::Path::new(&numbered[1]).is_dir());
    std::fs::remove_dir_all(root).unwrap();
}
