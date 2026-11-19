# Generate native caption

## Original contract

`apps/shell/src/main/generate/generate-window.ts:211-250` creates one initially hidden window, disables minimize/maximize/fullscreen, and uses a hidden native overlay only outside macOS. Theme changes update that overlay. Native close must still enter the normal close event pipeline.

`apps/shell/src/renderer/src/generate/GenerateWindow.tsx:142-144` contains only a heading in the title band. There are no links, form controls, or buttons there. `generate.css:74-85` fixes that band at 44 CSS pixels and marks it draggable. The document body starts below it. The CEF viewport must not be reduced or translated.

## Boundary and ownership

The shell companion's `configure(&WebviewWindow, &GenerateWindowOptions)` and `update(&WebviewWindow, &TitleBarOverlay)` run on UI. `GenerateWindows` owns the live window and marshals calls to UI. macOS configuration keeps the existing native frame. An overlay update on a platform without an overlay returns an error, not fake success.

The platform runtime module exposes `CaptionStyle { background: [u8;3], symbols: [u8;3], height: u32 }` and unsafe `configure(native: usize, style: CaptionStyle)` / `update(native: usize, style: CaptionStyle)`, both returning `Result<(),String>`. The borrowed handle is an HWND on Windows or GtkApplicationWindow pointer on Linux. The caller must keep it live on its owning UI thread. Height is exactly 44 DIP.

Windows keeps an ordinary native BUTTON with owner-drawn close glyph and a localized name from the system close menu. The standard button retains its native accessibility provider and action. Its command posts WM_CLOSE. HWND subclasses route only the original header to native caption dragging and system menu, leave resize hit tests intact, prevent double-click maximize, and follow monitor DPI. They do not move or resize any CEF window. Root destruction removes the subclasses and registry entry. The existing CEF external message pump uses the winit UI thread; a child on another thread is an explicit configuration error.

Linux keeps the original CEF origin, extent, and DOM pixels. GTK owns the native close control and header drag gesture. XFixes clips the CEF host's bounding region only at that close control. Its input region excludes the 44-DIP header, which contains no original interactive children. The body input coordinates do not change. Native GDK move and window-menu operations handle the header; double click does not maximize. CefX11Host registers before configuration, reapplies shapes after layout, and unregisters before destroying its X11 window.

Linux requires the already-supported native X11 CEF host, XFixes region support, and X Shape 1.1 input regions. Missing support returns an error during configuration. There is no Wayland or CSS fallback. libXext.so.6 is dynamically loaded and kept alive until the owned display connection closes. The XShapeQueryVersion ABI is taken from [libXext-1.3.6 shape.h](https://gitlab.freedesktop.org/xorg/lib/libxext/-/raw/libXext-1.3.6/include/X11/extensions/shape.h). No additional Rust dependency is introduced.

## Regression source and hosted proof

Source fixtures cover exact palettes, signed desktop coordinates, monitor-DPI sizing, unchanged title/body clipping geometry, and window-scoped GTK palette rules. The ignored Windows synthetic native fixture additionally checks unchanged root/child geometry, native header hit testing, nonempty close name, an actual accessibility object, dark-theme update, disabled double-click maximization, deferred WM_CLOSE, teardown, and update-after-destroy rejection. It creates only synthetic windows.

No Rust build, check, test, formatter, native window, or input automation ran locally. These fixtures are unexecuted source, not proof of native parity. Main's hosted proof must include the actual generated CEF window on Windows and X11, first-show and resized viewport screenshots, localized close accessibility/action, click and drag routing, body coordinates at y=44, 100/150/200-percent DPI, light/dark changes, focus/inactive visuals, repeated creation/teardown, unavailable-extension failure, and close veto/retry. Linux needs native-server verification that the GTK control is visible through the close-only clip and that no document pixels move.
