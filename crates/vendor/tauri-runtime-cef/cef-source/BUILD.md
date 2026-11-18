# Private CEF source build contract

This is a build definition and integration checklist, not an available binary or
an authorization to dispatch a build. Do not substitute upstream binaries for
this private ABI. The shell currently downloads and caches upstream binary
archives; that path does not build, authenticate, or provide this extension.

## Sources and provenance

- Upstream CEF repository: <https://github.com/chromiumembedded/cef.git>.
  Exact commit `14c5a089e8452874cb1a556dc3fde26ed2d87723` corresponds to
  CEF `154.0.34+g14c5a08+chromium-154.0.8037.98` and its
  `CHROMIUM_BUILD_COMPATIBILITY.txt` selects Chromium tag `154.0.8037.98`.
- Chromium repository: <https://chromium.googlesource.com/chromium/src.git>.
  That tag resolves to `b859317bf11f6be47f9b7799ec690a0a42a1fb33`.
  Chromium DEPS at that commit determines its dependency revisions and hooks.
- Candidate depot_tools repository:
  <https://chromium.googlesource.com/chromium/tools/depot_tools.git>, revision
  `8a5434051036b32412a2ecb10c213a72e3f3ccb9` (2026-10-02). This is a
  reproducible candidate contemporaneous with the CEF commit, **not** an
  attestation of the toolchain used for upstream binaries. Disable depot_tools
  automatic updates and verify the checked-out revision. Its compatibility and
  all resulting CIPD/package resolutions remain to be verified on each runner.
- Original consumer baseline: Electron `43.8.3`, Chromium
  `150.0.7871.250`. Its `WebContentsObserver::OnWebContentsFocused` and
  `OnWebContentsLostFocus` emit the respective WebContents events. The shell
  consumer waits for its WebContents blur followed by focus or destruction;
  its existing one-second arming behavior must remain unchanged.

CEF source branch 8037, the CEF and Chromium commits, and the depot_tools
revision must be independently verified before applying patches. Upstream build
instructions are <https://chromiumembedded.github.io/cef/automated_build_setup.html>
and <https://chromiumembedded.github.io/cef/branches_and_building.html>.

## Patch and ABI assembly

1. Sync the pinned Chromium dependencies and exact CEF source as prescribed by
   the pinned CEF build tools. Do not allow a moving branch head or a fresh
   `depot_tools` checkout to replace the commits above.
2. Apply every patch in the exact per-tree order encoded by
   `verify-patches.sh`. Chromium-root patches are 0002, 0006, 0011, 0013,
   0015, 0017, 0020, 0024, 0027 and 0030. CEF-root patches are 0001, 0003, 0004,
   0005, 0007, 0008, 0009, 0010, 0012, 0014, 0016, 0018, 0019, 0021, 0022,
   0023, 0025, 0026, 0028, 0029, 0031, 0032 and 0033. Do not apply a Chromium-root patch to the CEF directory.
   Patch 0026's observed-navigation dispatch and callback are source-connected;
   generated ABI, stack application, native behavior and consumers are unverified.
   Patches 0027 and 0028 add source-only pre-revocation process-loss claims;
   their stack application, generated ABI and native execution remain unverified.
   Patch 0029's unprotected document lifetime is source-connected to Rust;
   generated ABI and native execution remain unverified.
   Patches 0030 and 0031 move document loss from navigation admission to actual
   retirement; their native ordering and behaviour remain unverified.
   Patch 0032 provides only one-shot observed-document result acceptance; it
   does not authenticate a CDP target or implement a complete protocol port.
   Patch 0033 captures native CDP frame and loader correlation only; neither
   identifies a V8 execution context or authorizes a CDP command.
   `patches.sha256` pins every complete patch input. The verifier checks those
   hashes before staging or applying any source.
   These are source definitions, not generated ABI or native safety evidence.
   The source verifier must succeed against both pristine pinned trees before
   the single API-generation step. A partially applied tree is not an input
   to the translator, binding generator or native build.
   The revocation linearization point is the synchronous native admission
   callback before frame-tree/navigation mutation, not a later protocol event.
   The source keeps native registration admission independent from exact-context
   evaluation. No browser UI wait on renderer scripts/core/CDP is permitted.
   Completion subscribers and application code run only after admission locks
   have been released. Chromium process exit can physically precede browser
   observation; the process receipt reports the actual RPH exit notification,
   not an assertion that revocation preceded the OS crash.
3. The CEF checkout's last tracked API version is `15400`. Every private extension
   must use the same new, private `15401` revision. After all patches are in
   place, run the pinned `tools/version_manager.py -a` once from the CEF tree.
   That operation invokes the CEF translator and generates the new API hashes;
   preserve the resulting headers, wrappers and version/hash files together.
   Do not independently generate ABI versions per patch. The pinned CEF
   `tools/gclient_hook.py` separately invokes `version_manager.py -u
   --fast-check`, then CEF's Chromium patcher, then GN generation. Arrange
   source sync and private patch application so all extensions are present
   before `-a` and before that hook; verify the hook's extra translator/check
   pass and upstream patcher preserve the private changes and generated hashes.
4. Generate matching `cef-rs` and `cef-dll-sys` bindings from those exact
   generated headers and select API `15401` consistently for compilation and
   initialization. The upstream `154.5.0` bindings and stock CEF binary do not
   contain the new callback. Require the private platform API hash and fail
   closed before browser creation on version/hash mismatch; an upstream archive
   sharing the CEF product version must never satisfy this requirement.
5. The CEF runtime consumer must associate callbacks with the actual browser
   generation and serialize registration/revocation and application effects on
   its runtime owner. A captured callback delivered after revocation must have
   no application effect. The patch only forwards the underlying Chromium
   per-WebContents callback; it does not itself enforce application lifetime.

### Approved navigation and revocation ABI

Private API `15401` adds the browser-UI-thread-only
`LoadApprovedMainFrameURL(url, uint64_t issuance, intent, callback)`.
`CefApprovedNavigationCallback::OnComplete(bool accepted)` resolves once.
The caller must record its opaque intent and nonwrapping issuance before
calling; `DidStartNavigation` and native registration may occur before
completion. A late rejection may cancel only that exact issuance, never its
successor. Issuance is monotonic per exact browser generation, not per URL.
CEF honors its navigation lock by deferring the actual load. It identifies
the controller-created NavigationUIData clone by native ownership and binds
the actual NavigationHandle ID; no URL, frame lookup, HTTP header or
renderer-supplied marker confers authority. Redirects keep that handle.

`OnNativeContextRegistered` includes captured `frame_id`, renderer child ID,
document/context tokens, issuance, opaque intent and world ID. Its consumer
must atomically redeem its own exact pending issuance and retain the original
native identity for every later action. `OnNativeContextWillRevoke` closes
the same admission gate synchronously without executing application code.
`OnNativeContextDidRevoke` only schedules draining browser-independent
retained cleanup. Scoped native guards retain immutable callback arguments
through function exit, including early returns; first-declared guard members
extend destruction scopes through all FTN/RFH member teardown. Nested native
mutation depth delays paired post signals until the outermost scope unwinds.
Failed posting must leave cleanup retained for an explicit runtime
shutdown drain. NavigationUIData clone ownership has a separate short data-only
mutex because URL-loader clones may be destroyed away from the browser UI
thread; approval fields remain UI-thread-owned. No callback or browser/UI wait
may occur while that ownership-map mutex is held.

The patch includes CEF
`NavigationTest.ApprovedNativeIntentReplacementAndReplay` and Chromium
`FrameTreeTest.CefAdmissionPairRetainsPostAfterUnsubscribe` /
`CefAdmissionPrecedesNavigationRequestReplacement` /
`CefAdmissionPostOutlivesFrameAndTreeDestruction` source regressions.
These are uncompiled and unexecuted; they do not establish the full native
safety property or replace the required hosted lifecycle matrix.

All patch SHA-256 values, including shared-source adapters, are pinned in
`patches.sha256`. Changing a patch invalidates that frozen input unit and requires
a new manifest, source-application result, translator output and binding input.

The focus patch alone passed `patch -p1 --dry-run` against the three files
fetched at the exact CEF commit above. This verifies source context only, not
the compiled CEF API or composition with the identity patch. The current
application build/download path and Rust bindings have not yet been
converted to this contract. Translator output, private API hashes,
identity-patch composition, binding generation, runtime hash enforcement,
binary provenance attestation, and hosted integration are not verified. Do
not claim this definition is a shippable artifact.

### Additional generated API 15401 inputs

- `CefClient::OnNativeContextRegistered` receives one immutable
  `CefNativeRendererProcess` in addition to exact evaluation/reply endpoints.
  Its `ObserveExit(CefNativeRendererProcessCallback)` method distinguishes
  actual `RenderProcessExited` from host destruction without an exit receipt.
  `GetObserverIdentity` identifies one process incarnation across documents.
- Every revoke, native traffic, first-layout and document-ready callback carries
  the original native registration issuance. Popup metadata carries the same
  incarnation. A restored V8/document tuple cannot revive an earlier callback.
- `CefRequest::GetNativeResourceOrigin(CefNativeResourceOrigin&)` exposes
  `cef_native_resource_origin_t` only from the exact native factory/request.
  It is not proof of an issuing JavaScript realm. The matching registration
  and native live gate are captured once before application IO.
- `CefRequest::GetNativeResourceDelivery` returns one original-resource
  `CefNativeResourceDelivery`. `TryAccept` shares the captured native document
  admission gate; resource detach cancels it synchronously before deferred
  `CefResourceHandler::Cancel`. Actual response processing/Continue is outside
  that bounded admission. Accepted-before-revoke answers are already admitted;
  cancelled original resources are not continued.
- `CefNativeRendererProcess::PermitsTermination` inspects the retained actual
  RPH, every represented RFH root and immutable supplied browser/document
  identities. Missing/speculative RFHs or worker/keepalive/pending ownership
  veto termination. The shell's partial view map does not confer permission.

## Hosted build and artifact gate

Build separately for macOS arm64 and Windows x64, using the pinned CEF
`automate-git.py` and the official release/minimal-distribution build flow,
with the source patching and one translator step above placed after source sync
and before project generation. Pin all transitive tool/package resolutions,
compiler/SDK versions, GN arguments, and any PGO inputs in the hosted build
record; prove the automation does not reset patched files or update pins.
The upstream automated-build page lists at least 16 GB RAM and 200 GB free
disk separately for [macOS](https://chromiumembedded.github.io/cef/automated_build_setup.html#macos-configuration)
and [Windows](https://chromiumembedded.github.io/cef/automated_build_setup.html#windows-configuration).
That page targets currently supported release branches; these are documented
planning prerequisites, not measured consumption or a guarantee for legacy
branch 8037. The [branch table](https://chromiumembedded.github.io/cef/branches_and_building.html#legacy-release-branches-unsupported)
lists macOS 15.6+, Xcode 26.0/SDK 26, and Windows VS 2022 17.13.4/SDK
10.0.28000.2270 for 8037. The pinned [Chromium toolchain selector](https://raw.githubusercontent.com/chromium/chromium/b859317bf11f6be47f9b7799ec690a0a42a1fb33/build/vs_toolchain.py)
lists VS 2026 first as its packaged toolchain but also detects VS 2022 with
VC143. The pinned [CEF runhooks patch](https://raw.githubusercontent.com/chromiumembedded/cef/14c5a089e8452874cb1a556dc3fde26ed2d87723/patch/patches/runhooks.patch)
lets `GYP_MSVS_VERSION` override that selection, and CEF's automated Windows
example sets it to 2022. Chromium's own pinned instructions instead require
VS 2026 >=18.0.0 and at least 100 GB free. Thus the selection mechanism is
known, but the full 2022 build has not been proven for this exact source and
must not be treated as tested; validate the actual chosen toolchain and SDK.

[GitHub's standard runner specifications](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#supported-runners-and-hardware-resources)
list 14 GB SSD and 7 GB RAM for macOS arm64 in either public or private
repositories, and 14 GB SSD with 16 GB RAM for public Windows x64 or 8 GB RAM
for private Windows x64. These are advertised specifications, not measured
free disk. The existing shell job's 90-minute limit and stock binary cache do
not establish source-build feasibility. Actual build time, source/output peak
sizes and private archive sizes are unmeasured.

The [official CEF index](https://cef-builds.spotifycdn.com/index.json) reports
the exact pinned upstream **compressed binary** standard archives as
307,056,359 bytes for macOS arm64 and 365,396,143 bytes for Windows x64;
minimal archives are 132,210,884 and 172,752,666 bytes respectively. These
numbers are index metadata, not a measurement of this project's custom output,
and binary archives do not contain the Chromium build intermediates needed to
relink `libcef`. The earlier approximately 1 GB workflow comment is not an
observed size for these archives.

The pinned [Chromium macOS build instructions](https://raw.githubusercontent.com/chromium/chromium/b859317bf11f6be47f9b7799ec690a0a42a1fb33/docs/mac_build_instructions.md)
describe `--no-history`, shared Git caches, component builds and compiler
caching for incremental speed; they estimate the optional shared full-history
Git cache alone at about 30 GB. The equivalent pinned Windows guidance gives
the same cache estimate. These techniques do not establish that a clean
release link fits a 14 GB runner or produce the missing prebuilt object graph.
Chromium's pinned guidance says external contributors cannot use its remote
execution for macOS or Windows. Sharding jobs or caching object fragments is
not yet a reproducible, provenance-verified build plan, especially for the
final link; no sound official-artifact-only free-runner route has been shown.

Use distinct per-platform private artifact/cache keys incorporating the full
CEF and Chromium revisions, depot_tools/toolchain and package identities,
ordered patch hashes and target trees, generated API version and platform API hash,
GN/PGO configuration and architecture. Verify an authenticated artifact digest
and private ABI/hash before reuse. Do not populate or read the stock CEF
binary cache as if it contained a patched artifact. No public artifact
publication or distribution change is implied.

Before dispatch: fix and review all patch hashes, source and toolchain pins,
patch application point, complete reproducible build and binding commands,
private artifact authentication and consumer fail-closed gate; provision and
measure capable hosted macOS arm64 and Windows x64 runners. Afterward run
hosted native compile/check/tests and actual page focus/activation/teardown,
ID reuse, and late-delivery regressions. The macOS AppKit harness must run on
the process main thread. None of these checks has been run by this change.
