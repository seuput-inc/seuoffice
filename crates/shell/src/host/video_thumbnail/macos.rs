use block2::RcBlock;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{MainThreadMarker, class, msg_send};
use objc2_app_kit::NSScreen;
use objc2_foundation::{NSData, NSError, NSSize, NSString, NSURL};
use shell_contract::runtime::Thread;
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[link(name = "QuickLookThumbnailing", kind = "framework")]
unsafe extern "C" {
    static QLThumbnailErrorDomain: *const NSString;
}

pub(super) fn video_poster(path: &Path, ui: &Arc<dyn Thread>) -> Result<Option<Vec<u8>>, String> {
    let scale = crate::host::on_ui(ui, || {
        let marker =
            MainThreadMarker::new().ok_or("Thumbnail screen lookup was not on the UI thread")?;
        let screen = NSScreen::screens(marker)
            .firstObject()
            .ok_or("No screen is available for thumbnail generation")?;
        Ok::<_, &str>(screen.backingScaleFactor())
    })
    .map_err(str::to_owned)?;
    let url = NSURL::from_path(path, false, None).ok_or("Invalid video path")?;
    let (send, receive) = mpsc::channel();
    autoreleasepool(|_| unsafe {
        let size = NSSize::new(960.0, 540.0);
        let request_class = AnyClass::get(c"QLThumbnailGenerationRequest")
            .ok_or("Quick Look thumbnailing is unavailable")?;
        let request: Allocated<AnyObject> = msg_send![request_class, alloc];
        let request: Option<Retained<AnyObject>> = msg_send![request, initWithFileAtURL: &*url, size: size, scale: scale, representationTypes: 7usize];
        let request = request.ok_or("Quick Look request initialization failed")?;
        let completion = RcBlock::new(move |thumbnail: *mut AnyObject, error: *mut NSError| {
            let result = autoreleasepool(|_| {
                if !error.is_null() {
                    let error = unsafe { &*error };
                    let code = error.code();
                    let domain = unsafe { QLThumbnailErrorDomain.as_ref() };
                    return if code == 0
                        && domain.is_some_and(|domain| error.domain().isEqualToString(domain))
                    {
                        Ok(None)
                    } else {
                        Err(format!("Quick Look thumbnail failed (code {code})"))
                    };
                }
                if thumbnail.is_null() {
                    return Ok(None);
                }
                unsafe { encode_thumbnail(thumbnail, size) }
            });
            let _ = send.send(result);
        });
        let generator_class = AnyClass::get(c"QLThumbnailGenerator")
            .ok_or("Quick Look thumbnailing is unavailable")?;
        let generator: Option<Retained<AnyObject>> = msg_send![generator_class, sharedGenerator];
        let generator = generator.ok_or("Quick Look thumbnail generator is unavailable")?;
        let _: () = msg_send![&*generator, generateBestRepresentationForRequest: &*request, completionHandler: &*completion];
        Ok::<(), String>(())
    })?;
    receive
        .recv_timeout(Duration::from_secs(120))
        .map_err(|_| "Quick Look thumbnail service did not respond".to_owned())?
}

unsafe fn encode_thumbnail(
    thumbnail: *mut AnyObject,
    size: NSSize,
) -> Result<Option<Vec<u8>>, String> {
    unsafe {
        let cg_image: *mut core::ffi::c_void = msg_send![thumbnail, CGImage];
        if cg_image.is_null() {
            return Ok(None);
        }
        let image: Allocated<AnyObject> = msg_send![class!(NSImage), alloc];
        let image: Option<Retained<AnyObject>> =
            msg_send![image, initWithCGImage: cg_image, size: size];
        let image = image.ok_or("Cannot create thumbnail image")?;
        let raw: *mut core::ffi::c_void = msg_send![&*image, CGImageForProposedRect: core::ptr::null_mut::<core::ffi::c_void>(), context: core::ptr::null_mut::<AnyObject>(), hints: core::ptr::null_mut::<AnyObject>()];
        if raw.is_null() {
            return Ok(None);
        }
        let bitmap: Allocated<AnyObject> = msg_send![class!(NSBitmapImageRep), alloc];
        let bitmap: Option<Retained<AnyObject>> = msg_send![bitmap, initWithCGImage: raw];
        let bitmap = bitmap.ok_or("Cannot create thumbnail bitmap")?;
        let _: () = msg_send![&*bitmap, setSize: size];
        let properties: Retained<AnyObject> = msg_send![class!(NSDictionary), new];
        let png: Option<Retained<NSData>> =
            msg_send![&*bitmap, representationUsingType: 4usize, properties: &*properties];
        let bytes = png.ok_or("Cannot encode thumbnail PNG")?.to_vec();
        if bytes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(bytes))
        }
    }
}
