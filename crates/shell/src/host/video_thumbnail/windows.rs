use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use shell_contract::runtime::Thread;
use windows::core::{IUnknown, PCWSTR};
use windows::Win32::Graphics::Gdi::{BITMAP, BITMAPINFO, BITMAPV5HEADER, BI_RGB, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, DIB_RGB_COLORS, GetObjectW, HBITMAP, HDC, HGDIOBJ, SelectObject};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{ISharedBitmap, IShellItem, IThumbnailCache, LocalThumbnailCache, SHCreateItemFromParsingName, WTS_E_DATAFILEUNAVAILABLE, WTS_E_FAILEDEXTRACTION, WTS_E_FASTEXTRACTIONNOTSUPPORTED, WTS_E_NOSTORAGEPROVIDERTHUMBNAILHANDLER, WTS_SCALETOREQUESTEDSIZE, WTS_SCALEUP};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, DrawIconEx, HICON, ICONINFO, DI_MASK, DI_NORMAL};

struct Apartment(bool);
impl Drop for Apartment { fn drop(&mut self) { if self.0 { unsafe { CoUninitialize() } } } }
struct Dc(HDC);
impl Drop for Dc { fn drop(&mut self) { unsafe { let _ = DeleteDC(self.0); } } }
struct Icon(HICON);
impl Drop for Icon { fn drop(&mut self) { unsafe { let _ = DestroyIcon(self.0); } } }
struct SelectedBitmap { dc: HDC, old: HGDIOBJ, bitmap: HBITMAP }
impl Drop for SelectedBitmap {
    fn drop(&mut self) {
        unsafe { SelectObject(self.dc, self.old); let _ = DeleteObject(HGDIOBJ(self.bitmap.0)); }
    }
}

pub(super) fn video_poster(path: &Path, _ui: &Arc<dyn Thread>) -> Result<Option<Vec<u8>>, String> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) { return Ok(None); }
    wide.push(0);
    unsafe {
        let status = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let initialized = status.is_ok();
        if !initialized && status.0 != 0x80010106u32 as i32 { return Err(format!("Initializing thumbnail COM: {status:?}")); }
        let _apartment = Apartment(initialized);
        let item: IShellItem = match SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) {
            Ok(item) => item,
            Err(error) if matches!(error.code().0 as u32, 0x80070002 | 0x80070003 | 0x80070005 | 0x80070057) => return Ok(None),
            Err(error) => return Err(format!("Creating thumbnail shell item: {error}")),
        };
        let cache: IThumbnailCache = CoCreateInstance(&LocalThumbnailCache, None::<&IUnknown>, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("Creating thumbnail cache: {error}"))?;
        let mut bitmap: Option<ISharedBitmap> = None;
        if let Err(error) = cache.GetThumbnail(&item, 960, WTS_SCALETOREQUESTEDSIZE | WTS_SCALEUP, Some(&mut bitmap), None, None) {
            let code = error.code();
            if [WTS_E_FAILEDEXTRACTION, WTS_E_DATAFILEUNAVAILABLE, WTS_E_FASTEXTRACTIONNOTSUPPORTED, WTS_E_NOSTORAGEPROVIDERTHUMBNAILHANDLER].contains(&code) { return Ok(None); }
            return Err(format!("Obtaining thumbnail: {error}"));
        }
        let Some(bitmap) = bitmap else { return Ok(None); };
        let handle = bitmap.GetSharedBitmap().map_err(|error| format!("Reading thumbnail bitmap: {error}"))?;
        let mut source = BITMAP::default();
        if GetObjectW(HGDIOBJ(handle.0), std::mem::size_of::<BITMAP>() as i32, Some((&mut source as *mut BITMAP).cast())) == 0 {
            return Err("Reading thumbnail bitmap dimensions failed".into());
        }
        let (width, height) = (source.bmWidth, source.bmHeight);
        if width <= 0 || height <= 0 { return Ok(None); }
        if width > 8192 || height > 8192 { return Err("Thumbnail dimensions exceed the safety limit".into()); }
        let size = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).ok_or("Invalid thumbnail size")?;
        let icon = Icon(CreateIconIndirect(&ICONINFO {
            fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: handle, hbmColor: handle,
        }).map_err(|error| format!("Creating thumbnail icon: {error}"))?);
        let dc = Dc(CreateCompatibleDC(None));
        if dc.0.0.is_null() { return Err("Creating thumbnail device context failed".into()); }
        let header = BITMAPV5HEADER {
            bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: width, bV5Height: -height, bV5Planes: 1, bV5BitCount: 32,
            bV5Compression: BI_RGB, bV5RedMask: 0x00ff0000, bV5GreenMask: 0x0000ff00,
            bV5BlueMask: 0x000000ff, bV5AlphaMask: 0xff000000,
            bV5CSType: 1466527264, bV5Intent: 4, ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(dc.0), (&header as *const BITMAPV5HEADER).cast::<BITMAPINFO>(), DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|error| format!("Creating thumbnail DIB: {error}"))?;
        if bits.is_null() { let _ = DeleteObject(HGDIOBJ(dib.0)); return Err("Thumbnail DIB has no pixel buffer".into()); }
        let old = SelectObject(dc.0, HGDIOBJ(dib.0));
        if old.0.is_null() || old.0 as isize == -1 { let _ = DeleteObject(HGDIOBJ(dib.0)); return Err("Selecting thumbnail DIB failed".into()); }
        let _selected = SelectedBitmap { dc: dc.0, old, bitmap: dib };
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), size);
        pixels.fill(0);
        DrawIconEx(dc.0, 0, 0, icon.0, width, height, 0, None, DI_MASK)
            .map_err(|error| format!("Drawing thumbnail mask: {error}"))?;
        let opaque: Vec<bool> = pixels.chunks_exact(4).map(|pixel| pixel.iter().all(|value| *value == 0)).collect();
        pixels.fill(0);
        DrawIconEx(dc.0, 0, 0, icon.0, width, height, 0, None, DI_NORMAL)
            .map_err(|error| format!("Drawing thumbnail icon: {error}"))?;
        bgra_to_rgba(pixels, &opaque);
        let mut png = Vec::new();
        PngEncoder::new(&mut png).write_image(pixels, width as u32, height as u32, ExtendedColorType::Rgba8)
            .map_err(|error| format!("Encoding thumbnail PNG: {error}"))?;
        Ok(Some(png))
    }
}

fn bgra_to_rgba(pixels: &mut [u8], opaque: &[bool]) {
    let has_alpha = pixels.chunks_exact(4).any(|pixel| pixel[3] != 0);
    for (pixel, opaque) in pixels.chunks_exact_mut(4).zip(opaque) {
        pixel.swap(0, 2);
        if !has_alpha { pixel[3] = if *opaque { 255 } else { 0 }; }
        else if pixel[3] != 0 {
            let alpha = u32::from(pixel[3]);
            for channel in &mut pixel[..3] { *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bitmap_conversion_uses_mask_only_when_alpha_is_absent() {
        let mut masked = [3, 5, 7, 0, 4, 6, 8, 0];
        bgra_to_rgba(&mut masked, &[true, false]);
        assert_eq!(masked, [7, 5, 3, 255, 8, 6, 4, 0]);
        let mut alpha = [16, 32, 64, 128];
        bgra_to_rgba(&mut alpha, &[false]);
        assert_eq!(alpha, [128, 64, 32, 128]);
    }
}
