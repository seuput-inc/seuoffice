# crates/shell

3단계의 껍데기다. 안정판 Tauri 2.12.5가 운영체제의 웹뷰(macOS WKWebView, Windows WebView2)로 띄우는 실행 파일 하나이고, `main-core`를 같은 프로세스에 링크한다. 근거는 결정 0025(Rust 전환), 0029(껍데기 선택), 0030(중심 프로세스 `main-core`)이고, 모양은 `docs/seuoffice/3단계-셸-계약.md`가 정한다. 이 폴더는 자기 작업 공간과 잠금 파일을 가진다(`crates/Cargo.toml`이 `exclude`). 웹뷰는 운영체제가 주므로 Mac과 Windows 러너에서만 짓는다(`.github/workflows/seuoffice-shell.yml`). 웹뷰와 셸 사이의 층은 `src/native/`다: 웹뷰마다 Rust가 새기는 신원(`BrowserId`, `NavigationId`, `DocumentId`)과 문서마다 브리지가 등록하는 비밀 값이 IPC 권한의 근거다.

## 지금 되는 것과 안 되는 것

|                                                                                  | 상태                                                                                                     |
| -------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| 셸 뼈대: 스레드 셋, 서비스, 채널 표 시작 검사, 실패하면 오류를 찍고 끝남         | 됨                                                                                                       |
| IPC 경계: `ipc_invoke`, `ipc_send`, `ipc_listen`, 동기 스킴 `seuoffice-ipc`      | 됨(러너에서 처음 돈다)                                                                                   |
| 앱 스킴: 호스트마다 스킴 하나로 렌더러 묶음을 내줌                               | 됨                                                                                                       |
| preload 묶기와 브리지(`ipcRenderer`, `contextBridge`, `webUtils`, `webFrame`)    | 됨. `node crates/shell/bridge/preloads.mjs`가 19개를 만든다                                              |
| 공용 호스트: 대화 상자, OS 셸, 클립보드, 웹 콘텐츠, 셸 창                        | 됨                                                                                                       |
| 묶음 W 전용 호스트                                                               | **로그만 찍는 대역**이다. 표는 `src/host/w.rs`                                                           |
| 창·탭 계층(`WindowsHost`)과 `TabHooks`                                           | 대역. 진짜 구현은 다음 단위다                                                                            |
| 다른 묶음의 호스트(`src/host/{crash,d1,d2,menu,net,os,print,secrets,update}.rs`) | 파일만 있고 `install`이 비어 있다. 맡은 묶음이 채운다                                                    |
| 셸 창을 띄우는 일                                                                | 셸 렌더러(`apps/shell/out/renderer`)를 한 창에 띄우기만 한다. 탭 관리는 `tabs` 처리기를 이을 때 들어온다 |

## 짓기와 점검

러너 밖에서는 짓지 않는다. `check-local.sh`는 Mac과 Windows에서만 돈다(WebView 헤더와 OS API가 있어야 한다):

```sh
node crates/shell/bridge/preloads.mjs        # src/preload.rs가 묶음을 포함하므로 먼저
crates/shell/ci/check-local.sh check
crates/shell/ci/check-local.sh clippy --no-deps --all-targets -- -D warnings
crates/shell/ci/check-local.sh test          # 순수한 상태 기계의 시험(`native::`)과 호스트 시험이 돈다
```

러너에서는 워크플로가 같은 순서로 지은 뒤 측정까지 돈다. Mac은 `npx tauri build --bundles app`, Windows는 `cargo build --release`다. 내려받을 런타임이 없으므로 `.app`과 exe에 웹뷰를 따로 넣지 않는다. 작업 단위마다의 빠른 점검은 워크플로를 `gate=check`로 부르는 `check` 일이다(`cargo check`만 돌려 몇 분 안에 끝나고, `ci/slice-diagnostics.mjs`가 맡은 파일의 진단 수를 센다). 잠금 파일은 Ubuntu 러너의 `lock` 일로 한 번 만들어 가져온다(워크플로의 `gate=lock`).

## 환경 변수와 인자

|                               | 뜻                                                                                                                                                                           |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `SEUOFFICE_SHELL_PARTIAL=1`   | 채널 표가 덜 찼어도 시작한다. 없으면 `Completeness::Complete`라서 처리기가 없는 채널이 하나라도 있으면 시작하지 않고 목록을 찍는다. 묶음이 다 차기 전에는 이것이 있어야 뜬다 |
| `SEUOFFICE_USER_DATA`         | userData 폴더를 바꾼다(Electron 판의 시험 몰이꾼과 같다). 웹뷰 저장소는 Windows에서 그 아래 `webview2/`에, macOS에서는 폴더 경로에서 만든 데이터 저장소 식별자에 둔다 |
| `SEUOFFICE_APPS_DIR`          | `apps` 폴더. 기본은 빌드한 체크아웃이다. 렌더러는 `<apps>/<앱>/out/renderer`에서 읽는다                                                                                      |
| `--measure-dialog <out.json>` | 아래 측정                                                                                                                                                                    |
| `SEUOFFICE_SHELL_TRACE=1`     | 시각이 붙은 IPC 길 기록(`src/trace.rs`)을 stderr에 남긴다. `--measure-dialog`은 늘 켠다                                                                                      |
| `CI`                          | 시작 실패 때 대화 상자를 띄우지 않는다                                                                                                                                       |

userData는 Electron 판과 같은 자리다(결정 0013, 0019): 기본이 `appData/seuoffice`, 디버그 판은 Electron의 포장하지 않은 실행처럼 `appData/seuoffice-Dev`.

### 실제 앱 자동 시험 (`--e2e <scenario.json> --out <dir>`)

G3 관문의 실행기다(`src/e2e/`). 앱은 평소처럼 전부 시작하고(`SEUOFFICE_SHELL_PARTIAL`이 있으면 거부), 첫 셸 창이 서면 시나리오의 단계를 차례로 실제 앱에서 돌려 `<out>/e2e.jsonl`(단계마다 한 줄)과 산출물(PNG, PDF)을 쓴다. 통과 여부는 실행기가 아니라 검사기가 정한다. 열린 포트, 소켓, 파이프는 없다. 입력은 시나리오 파일뿐이다.

- 거부(종료 2): `SEUOFFICE_USER_DATA`가 없거나, 그 폴더에 워크플로가 만든 `.seuoffice-e2e-profile` 파일이 없을 때. 사람의 설치본, 프로필, 키체인은 대상이 될 수 없다. `--probe`, `--measure-dialog`와 같이 쓰는 것도 거부한다.
- 종료 코드: 0 모든 단계를 돌았다, 3 어떤 단계를 돌릴 수 없었다(거기서 멈춘다. 코어 스레드가 90초 동안 답하지 않는 것도 여기에 든다), 124 시나리오의 `deadlineMs`(기본 600000, 프로세스 시작부터 센다)를 넘겼다. 멈춘 실행은 실행 로그의 `e2e:`, `ipc:`, `core:`, `uiwait:`, `window:`, `attach:` 줄에 마지막 모습이 남는다. 앱이 서는 때부터 코어 스레드와 UI 스레드를 1초마다 불러 10초 넘게 답이 없으면 그 줄을 남기고, macOS에서는 `<out>/hang-core-late.txt`, `hang-ui-late.txt`, `hang-core-stalled.txt`, `hang-deadline.txt`에 모든 스레드의 `sample`이 남는다.
- `waitReady`는 `open`이 이름 붙인 파일의 탭이 활성이 되고 그 문서가 render-ready에 이른 것을 기다린다(`newDocument` 뒤에는 앞의 탭이 아닌 탭). 값에 활성 탭의 `kind`, `view`, `title`, `file`이 들어간다. 편집기의 제어 인터페이스가 끝내 준비되지 않으면(`control`이 `not_ready`나 `absent`) `page`에 그 페이지의 마지막 리소스 요청, 화면에 보이는 글, 기록된 오류가 들어간다.
- `type`, `shortcut`, `ime`, `click`은 페이지가 이미 포커스를 가지고 있으면 포커스를 다시 주지 않는다. 윈도에서 다시 주는 것(WebView2 `MoveFocus`)은 페이지 안의 포커스를 옮길 수 있고, 포커스를 잃으면 편집을 끝내는 편집기(마인드맵 토픽, 슬라이드 글 상자)는 앞 단계에서 시작한 편집을 잃게 된다.
- `focusBefore`, `focusAfter`의 `window`는 그 페이지의 창이 뜬 뒤로 포커스를 얻고(`gained`) 잃은(`lost`) 횟수다. 두 값 사이에 횟수가 늘었으면 그 사이에 포커스가 오갔다는 뜻이고, 포커스를 잃을 때 편집을 끝내는 편집기는 그때 편집을 잃는다. 실행 로그의 `focus:` 줄은 어느 뷰가 포커스를 얻고 잃었는지, 셸이 누구에게 포커스를 주었는지, 창의 `Focused` 이벤트가 창의 활성화였는지를 남긴다(처음 40줄, 그 뒤로는 100줄에 하나). `asset:` 줄은 문서 자산 스킴의 요청과 답을 남긴다.
- 윈도에서 셸 창의 첫 내비게이션이 관찰자를 붙이기 전에 시작되면(`attach: … holding the navigation`) 정책이 선 뒤에 다시 보내는데(`replaying the held navigation`), S129에서 그렇게 다시 보낸 두 번 모두 내비게이션이 시작되지 않아 창이 빈 채로 남았다(`the shell window was not ready within 120s`). 지금은 취소된 내비게이션의 끝이 보고될 때까지 아무것도 시작되지 않았으면 한 번 더 보내고 stderr에 `replaying it once more`를 남긴다. `nav:` 줄은 WebView2가 알린 내비게이션의 시작(취소 여부)과 끝을 남긴다.
- `shortcut`의 `closeTab`은 키를 보낸 뒤 그 탭이 활성 탭이 아니게 될 때까지 10초를 기다리고, 그대로면 실패한다. `waitReady`는 이미 열려 있던 같은 파일의 탭도 받으므로, 닫히지 않은 탭은 뒤의 "다시 열고 읽기"를 닫힌 적 없는 페이지의 읽기로 만든다(윈도에서는 Ctrl+W가 아무것도 닫지 않아 S121까지 모든 실행이 그랬다). `exportPdf` 뒤에는 앱이 내보낸 PDF를 새 탭으로 열기 때문에, 문서를 보여야 하는 `screenshot`은 `exportPdf` 앞에 둔다.
- 시나리오 문자열 안의 `${FIXTURES}`, `${WORK}`는 환경 변수 `SEUOFFICE_E2E_FIXTURES`, `SEUOFFICE_E2E_WORK`로 바뀐다.
- 대화 상자: `dialog` 단계가 다음 네이티브 대화 상자의 답(`save`, `dontSave`, `cancel`, `path:<절대경로>`)을 넣는다. 훅은 `host/dialog/owned.rs`와 `Dialogs::error`에 있고 `--e2e`가 아니면 아무 일도 하지 않는다. 답 없는 대화 상자는 15초 뒤 취소되며 `e2e-dialogs.jsonl`에 남는다.
- `query`는 활성 편집기 웹뷰에서 `PageOps::evaluate`로 식을 돌린다(Promise는 기다린다). 새 IPC 길은 없다.
- `click`은 최상위 문서에서 CSS 선택자가 가리키는 요소를 보이는 곳으로 스크롤하고, 페이지가 알려 주는 사각형(`getBoundingClientRect`)의 한 점(`fx`, `fy`, 기본 0.5)에 OS 입력으로 왼쪽 클릭을 한다(macOS는 `NSEvent` 마우스 이벤트, 윈도는 `SetCursorPos`와 `SendInput`). 교차 출처 프레임(draw.io)이나 캔버스도 사용자의 클릭을 받는다. 값에 그 점의 CSS 좌표, 창 좌표, 페이지가 그 점에서 찾은 요소(`hit`)가 들어간다.
- `type`은 글자를 하나씩 보내고, 페이지의 `keydown` 기록(최상위와 같은 출처 iframe)에 방금 보낸 키가 없으면 같은 키를 다시 보낸다(최대 4번). 교차 출처 iframe(draw.io)이 있는 페이지에서는 그 단계에서 기록이 한 번이라도 보인 뒤에만 그렇게 한다(키가 프레임 안으로 갔는지 사라졌는지 알 수 없어서). 값의 `resent`(다시 보낸 횟수)와 `unconfirmed`(끝내 확인하지 못한 키 수)에 남는다. 윈도에서 주입한 키가 페이지에 닿기 전에 사라지는 것(`diagram`이 `dagram`으로)을 가리려는 것이 아니라, 입력 주입의 손실을 문서의 결과와 따로 보이게 하려는 것이다. 값의 `inserted`는 그 단계 동안 페이지의 `beforeinput` 이벤트가 실어 온 글이다(창의 캡처 단계에서 기록하므로 편집기 코드보다 먼저 본다. 글이 없는 이벤트는 `<insertParagraph>`처럼 입력 종류로 남는다). 여기에 있는데 문서에 없는 글자는 편집기가 버린 것이고, 여기에도 없는 글자는 키 이벤트에서 글이 나오지 않은 것이다. `keydown`을 직접 처리하고 기본 동작을 막는 편집기는 여기에 아무것도 남기지 않는다.
- 윈도의 `ime` 단계는 한국어 언어 팩(한글 IME)이 이미지에 깔려 있어야 한다.

## 파일 지도

```
bridge/        preload 쪽. wire.ts, electron.ts(Transport 인터페이스, 계약), tauri.ts(Tauri 전송), electron-entry.ts(preload가 `electron`으로 받는 것), scheme-urls.ts(WebView2의 `http://<스킴>.localhost/…` 주소와 논리 주소 `<스킴>://…`를 IPC 문자열 경계에서 서로 바꾸고 `window.__SEUOFFICE_SCHEME_FORM__`을 정함. 화면이 직접 만드는 주소는 `@seuoffice/electron-utils/custom-scheme-url`), preloads.mjs
frontend/      Tauri의 frontendDist(자리 채움)와 측정 페이지 measure.html
src/main.rs    스레드, 서비스, 시작
src/ipc/       명령 셋(mod.rs), 뷰마다 출력 흐름(outlet.rs), 동기 스킴(sync.rs)
src/views.rs   뷰 표: 웹뷰 이름표 → Sender(ViewId, WindowId, Surface)
src/app_scheme.rs  앱 스킴, 주소 바꾸기
src/preload.rs     묶은 preload를 바이너리에 넣음
src/paths.rs       app.getPath
src/host/      호스트들(dialog, os_shell, clipboard, contents, window, w, unbuilt 와 다른 묶음의 빈 파일)
src/measure.rs --measure-dialog
src/e2e/       --e2e 실제 앱 자동 시험 실행기(시나리오 해석, 단계, 대화 상자 응답, 맥/윈도 원시 입력)
ci/            로컬 점검, 맡은 파일별 진단 세기(slice-diagnostics.mjs)
```

## IPC (계약 2절의 실제 모양)

- 명령 `ipc_invoke`의 본문은 프레임 `{id, channel, args}`(raw)다. 명령은 받았다는 표시만 답하고, 처리기의 답은 그 뷰의 출력 흐름에 `{reply: id, value | error}` 프레임으로 간다. `ipc_send`는 `{channel, args}`이고 답이 없다.
- 출력 흐름은 Tauri `ipc::Channel`이다. 브리지가 페이지가 시작될 때 `ipc_listen`으로 건넨다. 그 전에 나가는 프레임은 `TauriOutlet`이 기다려 두었다가(최대 1024개) 흐름이 열리면 순서대로 보낸다. 같은 뷰가 `ipc_listen`을 다시 부르면 새 문서로 보고 이전 문서의 호출을 취소한다.
- 브리지는 명령을 한 줄로 세운다(앞 명령의 받았다는 표시를 기다린 뒤 다음을 보낸다). 동시에 보낸 요청은 Tauri의 HTTP 길에서 순서가 뒤바뀔 수 있는데, 처리기는 뷰마다 보낸 순서를 지켜야 해서다. 대가는 호출마다 받았다는 표시 한 번의 왕복이다.
- 동기 호출은 **`seuoffice-ipc` 스킴으로 가는 동기 XHR**이다. macOS는 `seuoffice-ipc://localhost/<채널>`, Windows는 `http://seuoffice-ipc.localhost/<채널>`이고, 문서의 비밀 값이 `x-seuoffice-doc` 머리글로 따라간다. 같은 스킴이 표에 없는 예약 채널 셋을 답한다: `webutils:dropped-paths`, `webframe:set-zoom-factor`, `webframe:get-zoom-factor`.
- 보낸 이는 호출한 웹뷰의 이름표로 찾는다(`ViewTable`). 브리지가 없는 웹뷰의 메시지는 거절하고, 채널의 `surfaces`에 없는 웹뷰는 `Dispatcher`가 거절한다. 브리지가 있는 웹뷰는 자기 앱 스킴 밖으로 이동하지 못하고(`on_navigation`) `window.open`도 못 연다.
- 처리기의 panic은 `catch_unwind`로 오류 답이 된다. 이를 위해 `Cargo.toml`의 release 프로필이 `panic = "unwind"`다.

## 렌더러 주소

Electron 판은 `genoffice-app://<host>/<파일>`로 내줬고 처리기들(`main_core`의 `renderer_url`)이 지금도 그 이름으로 페이지를 부른다. 웹뷰는 호스트마다 스킴을 하나씩 두는 모양이 필요하므로 `genoffice-app-<host>`를 쓴다. 주소는 운영체제마다 다르다: macOS는 `genoffice-app-<host>://localhost/<파일>`, Windows는 `http://genoffice-app-<host>.localhost/<파일>`이다. 호스트마다 출처가 따로라서 localStorage와 IndexedDB가 호스트별로 갈린다(Electron도 그랬다). `app_scheme::webview_url`과 `electron_url`이 두 모양을 바꾼다. 처리기 쪽은 바뀌지 않는다. 호스트는 `main_core`의 `RENDERER_HOSTS` 열하나에 셸 자신의 `shell`이 더해진 것이고, 폴더는 `<apps>/<앱>/out/renderer`(`help`는 셸 것)다. 포장한 셸은 `<resources>/renderer/<host>`를 먼저 본다(묶음 설정은 묶음 U).

## preload

`bridge/preloads.mjs`가 `PRELOADS`(`channels.rs`)에서 19개를 읽어 preload마다 IIFE를 `out/preloads/<Surface>.js`로 만든다. `electron`은 `bridge/electron-entry.ts`로 바꾸고, `process`는 작은 대역(`process-shim.ts`)이다. 묶은 뒤 표에 적힌 전역이 모두 묶음 안에 있고 `electron`을 더는 부르지 않는지 확인한다. 셸은 웹뷰를 만들 때 그 `Surface`의 묶음을 초기화 스크립트로 싣는다(주 프레임만).

계약과 다른 점:

- Tauri 내부 객체를 전역에서 **지우지는 못한다**. Tauri 자신의 `invoke`가 호출마다 `window.__TAURI_INTERNALS__`를 읽기 때문이다(`scripts/core.js`). 브리지는 참조를 클로저에 잡고 속성을 열거되지 않게 고정한다. 웹뷰에는 capability가 없어 플러그인 명령은 막혀 있고, 셸의 명령 셋은 `Dispatcher`의 surface 검사가 지킨다.
- `webFrame`은 확대 배율만 이어졌다. `isWordMisspelled`와 `getWordSuggestions`(sheets의 `nativeSpell`)는 macOS에서 `NSSpellChecker`로 답하고, Windows에서는 WebView2에 닿는 철자 엔진이 없어 `false`와 `[]`를 답한다.
- `webUtils.getPathForFile`은 창의 끌어 놓기 이벤트가 준 경로를 `dataTransfer.files` 순서로 짝짓는다. 끌어 놓기 이벤트가 DOM 이벤트보다 먼저 오는지는 러너에서 처음 본다.

## 호스트

| 호스트                            | 구현                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| --------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Dialogs`                         | 파일 대화상자는 `rfd`. 메시지 상자(`dialog.showMessageBox`)는 `host/message_box.rs`: macOS `NSAlert`(부모 창이 있으면 시트), Windows `TaskDialogIndirect`(버튼 N개, `defaultId`, `cancelId`, 체크 상자, 아이콘), 그 밖의 OS는 `rfd`(버튼 1~3개, 체크 상자와 4개 이상은 취소 버튼으로 답하며 로그를 찍는다). 모두 UI 스레드에서.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `OsShell`                         | `open`, `trash`, 탐색기·Finder 열기                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `Clipboard`                       | `clipboard-rs`, UI 스레드에서                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `Contents`                        | 캡처와 PDF는 CDP(`Page.captureScreenshot`, `Page.printToPDF`). Electron 옵션을 CDP 인자로 바꾼다. `print`는 Tauri의 `print()`로 대화 상자를 열고 결과는 모른다                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `MainWindow`                      | 셸 창으로 표시한 뷰(없으면 첫 `Shell` 뷰)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `Arc<Mutex<SharedRecoveryState>>` | 프로세스에 하나, 임의 run id                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| 묶음 W 전용                       | `src/host/w.rs`가 `shell-ipc-w`가 `need`하는 호스트를 모두 넣는다. 진짜: `ClockHost`, 그리고 `ProjectsHostRef` 가운데 난수(id용이고 비밀값용이 아님), 홈 폴더, 숫자 서식(영어·한국어), `console.warn`. 나머지는 `unbuilt.rs`의 대역이다(처음 부를 때 한 번 로그): `QuickLookHost`, `QuickLookWindowHost`, `QuickLookEnginesHost`, `DocPickerWindowHost`, `DocPickerSourcesHost`, `DocsTabsHost`, `SheetsTabsHost`, `OpenFilesHost`, `FileParseHost`, `UiLanguageHost`, `HomeFilesHost`, `FileSearchHost`, `FolderWatchHost`, `HomeAppHost`, `WindowsHostRef`, `ProjectsHostRef`(창 이벤트, 타이머, 날짜·시간 서식, Windows 목록 실행. 날짜는 지역 시간대가 아니라 UTC), `ProjectFilesHost`, `LegacyProjectStoreHost`. `FolderTree`는 main-core가 맡아 대역이 없다. 반환값을 지어낼 수 없는 호출(창, 뷰, 열린 문서 목록)은 panic하고 `Dispatcher`가 오류 답으로 바꾼다 |

`dyn TabHooks`는 코어 스레드의 `Provided`에 들어가야 해서(`tabs::register`가 거기서 첫 것을 꺼낸다) `host::w::register_hooks`가 묶음 W보다 먼저 도는 묶음으로 추가한다. 진짜 구현이 생기면 그 자리에서 바꾼다.

## 측정 (`--measure-dialog <out.json>`)

계약 3절의 "알려진 차이"가 묻는 것이다. 모달 대화 상자가 떠 있는 동안 코어 스레드는 `run_on(ui, …)`에서 선다. 그동안 다른 창의 IPC 답이 얼마나 늦는지 잰다. 측정 페이지(`frontend/measure.html`)는 진짜 브리지가 있는 뷰에서 열리고, 같은 앱 스킴 길(`genoffice-app://shell/measure.html`)로 내준다.

- `idle`: 등록된 값싼 채널(`quick-look:get-settings`)을 순서대로 200번 부른 왕복(ms).
- `echo`: 1 KB와 10 MB를 진짜 프레임으로 되받는 왕복. `measure:echo`는 표에 없는 시험용 채널이고, 명령 → 코어 스레드 → 출력 흐름의 같은 길을 지난다. 시제품의 값은 Mac 1.3 / 64.6 ms, Windows 1.4 / 55.8 ms(결정 0029)였다.
- `sync`: 동기 XHR 100번.
- `dialog.ui`와 `dialog.core`: 20 ms마다 한 번씩 쉬지 않고 호출을 보내다가, 1초 뒤 코어 스레드를 2초 세운다. 호출을 **보낸 때**로 `before`(세우기 전), `during`(2초 동안), `after`(그 뒤 1.5초)로 나누어 `p50`, `p95`, `max`를 적는다. `ui`는 코어 스레드가 `run_on(ui, …)`로 기다리고 UI 스레드 일도 2초 잡고 있다(모달 대화 상자가 코어 스레드에 하는 일의 최악 경우. 진짜 대화 상자는 UI 스레드의 이벤트를 계속 돌리지만 이 일은 돌리지 않는다). `core`는 코어 스레드만 세우고 UI 스레드는 돈다. 두 값의 차이가 "코어가 서는 탓"과 "UI도 서는 탓"을 가른다.
- `environment`: OS, 아키텍처, 판, 프로필. 페이지의 `userAgent`도 들어 있다.

2초 뒤에도 답이 안 온 호출이 있으면 `unanswered`로 센다. 멈추면 셸이 스스로 `{error}`와 기록을 적고 끝낸다(아래 "기록", 러너가 걸리지 않게).

## 첫 러너 실행에서 확인할 것

1. **빌드**: Mac `.app`과 Windows `target/release`가 만들어지고, 운영체제 웹뷰(WKWebView, WebView2)를 쓰므로 따로 복사할 런타임이 없다.
2. **시작**: `shell: N handlers registered`가 찍히고(`PARTIAL`이 필요 없는 측정 모드는 `AsRegistered`), 측정 창이 뜬다. `genoffice-app-shell.localhost`가 파일 처리기로 내주는 HTML에 Tauri와 preload 스크립트가 들어가는지.
3. **`ipc_listen`의 `Channel`**: 첫 프레임이 와서 `measureIpc.invoke`가 풀린다.
4. **동기 XHR**: `http://seuoffice-ipc.localhost/…`로 교차 출처 요청이 되고(`Access-Control-Allow-Origin: *`), `sync.error`가 없다.
5. **측정값**: `idle`과 `echo`가 시제품 값과 비슷한지, 직렬화한 명령의 값이 얼마나 느린지. `dialog`의 `during`이 2초 가까이 되는 것은 설계대로다. 받아들일 수 없는 쪽은 `ui`와 `core` 가운데 어느 쪽 값이 큰지로 가른다.
6. 팝업 거부와 `on_navigation`이 페이지를 막지 않는지(측정 페이지는 앱 스킴 밖으로 나가지 않는다).

러너에서 처음 보는 것이라 로컬에서 확인하지 못한 API는 다음이다: `Window::add_child`(탭 뷰, 아직 부르지 않음), WebView2의 CDP 결과(`Page.printToPDF`, `Page.captureScreenshot`)의 `data` 모양, macOS `NSPrintOperation`의 PDF 쪽수와 여백, 끌어 놓기 이벤트와 DOM `drop`의 순서, 앱 스킴 HTML에 주입되는 초기화 스크립트가 `<head>` 맨 앞인지, WKWebView에서 사용자 스킴으로 가는 동기 XHR.

## 기록(trace)

`SEUOFFICE_SHELL_TRACE=1`이거나 `--measure-dialog`이면 `src/trace.rs`가 `[+  123.4ms] 꼬리표: 글` 줄을 stderr와 600줄 고리에 남긴다. 꼬리표는 다음과 같다.

- `window`: 창을 연 주소, 브리지 종류, 초기화 스크립트 크기.
- `scheme`: 앱 스킴 요청과 답.
- `page`, `frame`: 페이지 시작과 끝, 이동 실패, 렌더러 종료.
- `console`: 그 웹뷰의 console. 브리지가 `trace::enabled()`일 때만 전달한다.
- `ipc`, `sync`: 명령이 닿은 것(뷰, 채널, 인자 수, 바이트).
- `outlet`: 출력 흐름이 열린 것과 보낸 프레임.
- `measure`: 측정 처리기.
- `engine`: 엔진 호스트 페이지가 스스로 알리는 것(`engine-host/boot.ts`): 시작, 오류, CSP 위반, `ipc` 스킴에 닿는지의 시험, 준비 신호. IPC를 거치지 않고 자기 스킴의 `GET /__engine-host-log?m=…`로 오므로 IPC가 막힌 페이지도 이유를 남긴다.

브리지(`bridge/tauri.ts`)와 측정 페이지는 단계마다 console.log로 알린다(`[bridge +ms] …`, `[measure +ms] …`). preload가 돈 주소와 `__TAURI_INTERNALS__` 속성, `ipc_listen`을 보낸 때와 받았다는 표시가 온 때, 흐름의 첫 프레임, 첫 invoke와 첫 답, 5초마다 지금 단계가 나온다. 처리 길의 줄(`ipc`, `outlet`, `measure`, `sync`)은 처음 40줄만 모두 적고 이후는 100번에 한 번만 적어 측정을 늦추지 않는다. 센 값은 `traceCounts`에 남는다.

측정이 멈추면 셸이 `dialog.json`에 `{error, environment, traceCounts, trace: [...]}`를 적고 끝낸다. 40초 안에 아무 명령도 닿지 않았거나(preload가 안 돌았거나, 페이지가 안 떴거나, Tauri의 invoke가 페이지를 못 떠남), 150초 안에 보고가 없을 때다. `trace`의 마지막 줄들이 멈춘 곳이다. 읽는 법:

- `window … script N bytes`가 0이거나 없다: 브리지를 싣지 않았다.
- `scheme … measure.html -> 200`이 없다: 페이지를 받지 못했다. `404`면 폴더가 틀렸고, `frame … NAVIGATION FAILED`면 스킴 처리기가 이름을 받지 못한 것이다.
- `console … page …; measureIpc undefined`: 페이지는 떴으나 preload가 주입되지 않았다. `[bridge …] loaded …`가 없어도 같은 뜻이다.
- `[bridge …] ipc_listen #1 sent`만 있고 `ipc: … listen`이 없다: 명령이 페이지를 떠나지 못한다(CORS, ACL, invoke key). console에 `[shell bridge] ipc_listen failed`가 뒤따른다.
- `ipc: listen`은 있고 `outlet: stream attached`가 없다: `ipc_listen` 처리에서 막혔다.
- `outlet: … bytes sent`는 있고 `[bridge] stream frame #1`이 없다: `Channel`의 `eval`·fetch 길이 페이지에 닿지 못한다.
- `scheme … engine-host/main.js -> 200`까지만 있고 `engine: … engine_host_ready acknowledged`가 없다: 엔진 호스트가 준비되지 않았다. 측정 페이지는 준비된 뒤에 열리므로 `error`도 그렇게 말하고, `cannot start`의 오류도 페이지가 마지막으로 알린 것(`the engine page reported: …`)을 싣는다. `engine:` 줄이 이유다. `csp blocked connect-src …`나 `ipc probe …: TypeError`는 페이지의 CSP가 Tauri의 `ipc` 스킴을 막은 것이라 `connect-src`에 `ipc:`와 `http://ipc.localhost`가 있어야 하고, `engine:` 줄이 하나도 없으면 `boot.js`가 돌지 않았다.

## 묶음 W 트레이트가 바뀌었을 때

`host/unbuilt.rs`는 `shell-ipc-w`의 호스트 트레이트마다 대역을 하나씩 구현한다. 트레이트가 바뀌면 `ci/check-local.sh check`가 어긋난 메서드를 rustc 오류로 알려 준다. 새 호스트가 생기면 `host/w.rs`의 `install_hosts`에 넣고, `the_batches_register_over_the_stubs`(묶음 여섯이 `AsRegistered`로 `MissingHost` 없이 등록되는지)가 빠진 것을 잡는다.
