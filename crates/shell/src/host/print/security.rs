use std::path::Path;
use tauri::Url;

pub(super) fn scripts_enabled(profile: shell_ipc_m::hosts::PrintProfile) -> bool {
    !matches!(profile, shell_ipc_m::hosts::PrintProfile::Static | shell_ipc_m::hosts::PrintProfile::MindmapExport)
}

pub(super) fn allows(request: &str, page: &Url) -> bool {
    let Ok(url) = Url::parse(request) else { return false };
    if url.scheme() == "data" {
        return true;
    }
    if url.scheme() != "file" {
        return false;
    }
    let (Ok(requested), Ok(allowed)) = (url.to_file_path(), page.to_file_path()) else {
        return false;
    };
    same_path(&requested, &allowed)
}

#[cfg(not(windows))]
fn same_path(requested: &Path, allowed: &Path) -> bool {
    requested == allowed
}

#[cfg(windows)]
fn same_path(requested: &Path, allowed: &Path) -> bool {
    requested.to_string_lossy().to_lowercase() == allowed.to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn pdf_export_disables_scripts_but_dialog_keeps_them() {
        use shell_ipc_m::hosts::PrintProfile;
        assert!(!scripts_enabled(PrintProfile::MindmapExport));
        assert!(scripts_enabled(PrintProfile::MindmapPrint));
        assert!(!scripts_enabled(PrintProfile::Static));
        assert!(scripts_enabled(PrintProfile::Scripted));
        assert!(scripts_enabled(PrintProfile::Diagram));
    }

    #[test]
    fn synthetic_page_resource_requests_keep_only_embedded_data() {
        #[cfg(not(windows))]
        let page = Url::parse("file:///tmp/map.html").unwrap();
        #[cfg(windows)]
        let page = Url::parse("file:///C:/tmp/map.html").unwrap();
        // A synthetic page containing an embedded image, a sibling-file image,
        // and a remote stylesheet must not load either external resource.
        let embedded_image = "data:image/png;base64,AA==";
        let sibling_image = page.join("private.png").unwrap();
        let remote_stylesheet = "https://example.invalid/style.css";
        assert!(allows(embedded_image, &page));
        assert!(!allows(sibling_image.as_str(), &page));
        assert!(!allows(remote_stylesheet, &page));
    }
    #[test]
    fn allows_only_data_and_the_exact_page_file() {
        #[cfg(not(windows))]
        let page = Url::parse("file:///tmp/print%20page/map.html").unwrap();
        #[cfg(windows)]
        let page = Url::parse("file:///C:/tmp/print%20page/map.html").unwrap();
        let sibling = page.join("secret.html").unwrap();
        assert!(allows(page.as_str(), &page));
        assert!(allows(&format!("{}#page", page), &page));
        assert!(allows("DATA:image/png;base64,AA==", &page));
        for other in [
            sibling.as_str(),
            "https://example.com/image.png",
            "http://127.0.0.1/",
            "file://otherhost/tmp/print%20page/map.html",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(!allows(other, &page), "{other}");
        }
    }
}
