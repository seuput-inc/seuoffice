# tauri-runtime-cef 3.0.0-alpha.7, vendored

The CEF runtime of the Tauri shell in `crates/shell`. This directory is the crate
as crates.io publishes it, plus the patch below, and is used through
`[patch.crates-io]` in `crates/shell/Cargo.toml` (the shell is its own workspace).
It is not a member of the `crates/` workspace (`exclude = ["vendor", ...]` in
`crates/Cargo.toml`): `cargo fmt`, `cargo clippy` and the workspace tests leave it
as it is.

- **Source**: <https://crates.io/crates/tauri-runtime-cef/3.0.0-alpha.7>,
  repository <https://github.com/tauri-apps/tauri>, path `crates/tauri-runtime-cef`
  at `81bfb7ca8f1a17be1e48c9b0d167c77b2137f9d3` (`.cargo_vcs_info.json`).
- **Archive checksum** (SHA-256 of `tauri-runtime-cef-3.0.0-alpha.7.crate`, as
  `crates/shell/Cargo.lock` recorded it before vendoring):
  `c17121b17ba1bc49b9346ece93a42da70a43abdd0d793493838c4f0d8620ec36`.
- **License**: Apache-2.0 OR MIT (`Cargo.toml`), Copyright 2019-2024 Tauri
  Programme within The Commons Conservancy (source headers).
- **Not from the archive**: the archive carries no licence text. `LICENSE-MIT` and
  `LICENSE-APACHE-2.0` here are the copies the sibling crate `tauri-utils` 2.10.2
  ships. The archive's `.cargo-ok` registry marker is left out.
- **Local changes**: the runtime contains additional shell integrations alongside
  the context-menu patch. Preserve them when re-vendoring.

The bare-window reservation also needs a separate, pinned Tauri accessor patch.
`tauri-source/prepare.sh` checks the crates.io `tauri-3.0.0-alpha.6.crate`
SHA-256 `4b0cad7bd420aa41bdf7beab9a95e02d83c4cab1cb6f7b94fe4fd1e0c605feaa`
and patch SHA-256
`c8f75c9eb064541ea0e5f5d60d87b2c3dda2f53ec566a4509bd71e056927166e`,
then extracts and patches only under `OMP_SCRATCH_DIR`. Pass the path printed on
its last line to Cargo's `patch.crates-io.tauri.path` override. The upstream
Tauri archive retains its Apache-2.0 OR MIT license and notices. This source
preparation has not been compiled or validated as a native build.

## Why

The runtime builds Chrome style browsers and always shows Chrome's own page context
menu, filtered in `src/cef_impl/client/context_menu.rs`. CEF lets a client replace
that menu only from `CefContextMenuHandler::RunContextMenu`, and the runtime has no
hook to it. The shell must replace it: the docs editor draws its own menu in the
renderer, and every other page gets a native menu with spelling suggestions and
cut/copy/paste/select all/view image/copy image/save image as, built by main-core.

## Patches

`patches/0001-context-menu-hook.patch` (`diff -ruN` of the pristine tree against
this one, `a/` = pristine):

1. **Context menu hook**
   - `src/context_menu.rs` (new): `set_context_menu_handler`, `ContextMenuParams`,
     `EditFlags`, `ContextMenuDone`, re-exported from `src/lib.rs` (line 9).
     The handler is process wide and runs on the CEF UI thread with the webview's
     label; returning `true` takes the menu over, `false` keeps the filtered Chrome
     menu. `ContextMenuDone` holds the CEF callback (`RefGuard` is `Send`) and
     cancels on drop; `cancel` from another thread posts to the UI thread.
   - `src/cef_impl/client/context_menu.rs`: `run_context_menu` added to the handler
     (calls `crate::context_menu::run`); the handler struct takes the webview
     `label`.
   - `src/cef_impl/client/mod.rs:254`: passes `self.label.clone()`.

2. **Scheme requester provenance**
   - `src/scheme_request.rs`, `src/cef_impl/request_handler.rs`, and
     `src/webview.rs` attach a native browser lifetime, browser and frame IDs,
     and a snapshot of the currently observed document to custom-scheme request
     extensions. Neither that snapshot nor the navigation epoch proves which
     document issued a request. A delayed request can observe a replacement
     document, so document-scoped authorization needs independent context
     authentication. Browserless requests remain serviceable and carry no
     browser identity. The new source uses the same Apache-2.0 OR MIT license
     as this crate.

3. **Native custom schemes**
   - `src/native_scheme.rs`, `src/runtime.rs`, `src/cef_impl/client/process.rs`,
     and `src/cef_impl/request_context.rs` register explicitly configured
     standard, secure, CORS and Fetch-enabled schemes in browser and helper
     processes, and attach native scheme factories alongside the existing
     localhost factories. The new source uses the same Apache-2.0 OR MIT license.

4. **Asynchronous navigation startup**
   - `src/navigation_event.rs`, `src/webview.rs` and `src/tauri_ext.rs` defer
     navigation-sensitive dispatch until the initial navigation is dispatched
     after Page-domain and document-start setup. Readiness is not a load or
     document-identity guarantee. Protocol events advance observational epochs
     only; exact native RFH pre-mutation callbacks own context revocation.
   - DevTools sends and observer registration provide completion callbacks that
     do not wait for the UI thread. Closing a pending view reports failure.
   - An explicitly started unprotected browser remains on its inert creation
     document until its owner signals readiness, the inert load finishes, and
     Page-domain setup and any document-start script registration acknowledge.
     `activate_unprotected_initial_navigation` only opens that gate; it neither
     navigates nor grants protected IPC authority.
   - Browser focus and blur callbacks carry the `FrameNavigationState` retained
     by their original native browser observer. Its `is_same_browser` identity
     survives document replacement but does not authorize a document or process.
   - `navigate_observed` uses a browser-owned one-shot navigation issuance for
     unprotected documents. The returned observed document is not an IPC
     grant; its native loss subscription distinguishes renderer exit from
     document retirement. These source paths and their custom CEF ABI remain
     unverified and do not establish an exact-document CDP target.

5. **Browser creation and close lifetime**
   - `src/browser_lifetime.rs`, `src/cef_impl/client/life_span.rs`,
     `src/runtime.rs`, `src/webview.rs` and `src/window.rs` retain each creating
     browser's native host until close acknowledgement or attachment. Close
     messages carry both webview and browser identities, and parent windows
     remain alive for unattached browsers that are still closing.

6. **Private native-context source integration**
   - The pinned Chromium/CEF stack in `cef-source` defines RFH DocumentToken,
     V8ContextToken, renderer-process and approved-navigation lineage. Generated
     API 15401 Rust consumers register exact contexts before trusted scripts,
     carry native invocation metadata and deliver replies to captured private
     pipes. Explicit Unprotected, ProtectedInvoke and ProtectedPreload modes
     travel in creation data. Protected owned scripts run synchronously in the
     native context-created phase, not later HTML/CDP injection.
   - These C++ patches and Rust generated-type consumers are source-only.
     The complete current patch stack, generated bindings and native binary
     have not been validated together. Stock protected scheme IPC stays 403.
     No source model or patch application result is native security proof.
   - `NativeContextToken::on_revoke` subscribes to the exact registry entry.
     Native pre-mutation revocation clears authority before deferred subscriber
     delivery. `with_live`, pair/all gates and `NativeContextGatePlan` admit
     captured owners under canonical registry ordering. Prepared gate plans
     allocate before acquisition. A negative-only atomic flag cannot grant.
   - `NativeContextToken::evaluate` uses the captured native evaluator pipe.
     Native V8 Promise callbacks serialize by value and cancel on exact context
     release. Rust completions are queued outside registry locks. No current
     frame execution or CDP label-to-context substitution is used by this API.
     The older CDP challenge model is not evidence of a bound native evaluator.
   - Native popup metadata redeems the complete browser-admitted renderer,
     document, context, world and registration incarnation. A retained BFCache
     context receives a new incarnation; old traffic/release identities remain
     revoked even when native DocumentToken/V8ContextToken are retained.
     OS menu activation samples the actual native focused browser before core dispatch.
     Neither path grants from a saved label, current-frame lookup or page header.
   - `NativeChannelQueue` is prepared outside owner gates. Queueing only retains
     an exact reply. Actual UI-task pipe delivery reacquires the original native
     gate. The ordinary reply API still verifies authority before enqueueing.

7. **Exact response and physical-close handoffs**
   - Every custom-scheme request carries its original
     `NativeSchemeResponseTarget`. `prepare` owns/rewrites the body and CSP
     outside owner gates. `accept` uses the private native original-resource
     `TryAccept` gate, not a delayed Rust Cancel observation. Native
     HandlerProvider detach closes that gate synchronously before scheduling
     ResourceHandler Cancel. Accepted answers are queued by `finish` outside
     all owner gates and retain the original request, never a current view.
   - Chromium 0020 carries factory-created RFH/DocumentToken provenance.
     CEF 0021 captures it once per original request and preserves it across
     redirects. `SchemeRequestProvenance::native_document_owner` redeems only
     that captured native registration before application IO. This is resource
     document authority, not JavaScript issuing-context/IPC authority. Native
     resource acceptance and native document revocation share a short C++ gate.
     These producer/consumer source paths are not generated or compiled proof.
   - `WebviewCefExt::native_browser_close` exposes an exact creation reservation
     before OnAfterCreated/readiness. `Closed` is emitted only by OnBeforeClose;
     `NotCreated` means failure before a native browser existed. Requested close
     and document release are not physical-close acknowledgements.
   - Native window operations retain only the original winit window object.
     UI calls query the actual backend inline without a blocking message round
     trip. Worker calls dispatch to UI. Simple fullscreen reads the AppKit
     backend mode; modal operations use the actual NSWindow/HWND/GTK window.
     These native paths remain uncompiled and unexecuted in this source unit.
   - The in-progress `NativeWindowSubmissionGuard` binds an original window
     reservation to the native winit ID at allocation. Native `Destroyed` is
     observed after its callback unwinds; this is logical native API closure,
     not physical deallocation. `NativeClosed` remains disabled until all
     window-effect ingress and modal teardown paths use the same active-effect
     gate; an outstanding reservation therefore also holds runtime exit.
   - On macOS an original-window `NSWindowWillCloseNotification` observer keeps
     a pre-close effect through a later UI message and ends sheet relationships
     while the notification's NSWindow is valid. The pinned winit-appkit
     0.31.0-beta.3 `window_delegate.rs` emits `Destroyed` from `windowWillClose:`;
     `app_state.rs` may dispatch that event inline or queue it, so terminal
     receipt requires both the original notification completion and native
     event, in either order. Windows and GTK actual-destroy paths touch only
     surviving counterparts through their own scoped handles.
   - Hosted native verification must exercise real owner/sheet close in both
     directions, requested and unexpected native close, inline and queued
     delivery, failed counterpart access, delayed active-effect release,
     close-before-attach and exit. It must assert no native use of a destroyed
     original, no terminal before both required events and drain, and unchanged
     surviving-modal behavior. These are required scenarios, not observed proof.

8. **Renderer-process and session ownership**
   - `NativeRendererProcess` is issued by native registration from the actual
     RenderProcessHost. Its nonreusing observer identity is shared across that
     process's documents. Only RenderProcessExited acknowledges exit.
     HostDestroyed without the actual exit notification reports an error.
   - Each authenticated native document subscribes to its exact process
     incarnation. Native loss revokes only the matching captured registration;
     a successful exit posts `NativeRendererTerminated` with both the revoked
     origin and process identity. The stock browser-only termination callback
     is observational and cannot revoke a successor's contexts.
     The private 0027/0028 source hooks claim the registered RFH and original
     process-host incarnation before ordinary frame revocation, retaining the
     notification until the native mutation unwinds; later exit is idempotent.
   - Native process termination admission inspects every represented native RFH,
     rejects skipped/speculative owners and unknown browser/document roots,
     and vetoes worker, keepalive and pending native ownership. It never grants
     from the shell's partial view map or an observed current PID.
   - `NativeNetworkSession` retains one explicit persistent request context and
     policy for all related Diagram browsers and browserless worker requests.
     Other request contexts do not inherit that policy. Native per-browser
     background-throttling selection is carried before browser creation.
   - All private ABI consumers require the same generated API 15401 unit.
     Source patches, patch-application checks and Rust regression sources do
     not establish ABI compatibility, native behavior or shippable readiness.

## Re-vendoring

Unpack the new release over this directory, keep the two licence files, apply
`patches/0001-context-menu-hook.patch` (`patch -p1`), fix what moved, regenerate the
patch with `diff -ruN` against the pristine copy, and update the version and
checksum above. Check with
`cargo check --manifest-path crates/vendor/tauri-runtime-cef/Cargo.toml --features cef/dox`
and `bash crates/shell/ci/check-local.sh check`. Drop the directory and the
`[patch.crates-io]` entry when upstream has an equivalent hook.
