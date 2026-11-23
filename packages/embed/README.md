# @seuoffice/embed

호스트 웹앱이 문서(docx, xlsx, pptx, pdf, hwp, hwpx)를 seuoffice iframe 안에 열게 해 주는 작은 라이브러리. 문서 바이트와 파일명만 넘기면 된다.

seuoffice 웹 묶음(`@seuoffice/web`)을 호스트가 직접 정적 파일로 서빙하고, 그 주소를 `baseUrl`로 넘긴다. 이 패키지는 그 웹 묶음과 같은 버전을 쓴다. 호스트는 두 패키지를 같은 정확한 버전으로 고정한다.

```sh
# 안정 판 하나를 골라 같은 판의 두 묶음을 릴리스 주소로 설치한다(npm 레지스트리에는 없다)
V=0.12.15
npm install \
  https://github.com/seuput-inc/seuoffice/releases/download/v$V/seuoffice-embed-$V.tgz \
  https://github.com/seuput-inc/seuoffice/releases/download/v$V/seuoffice-web-$V.tgz
```

## 보기

```ts
import { openDocument } from '@seuoffice/embed'

const doc = openDocument(container, {
  baseUrl: `${location.origin}/seuoffice/`, // @seuoffice/web 의 dist/ 를 서빙하는 절대 주소
  bytes, // ArrayBuffer | Uint8Array
  fileName: 'report.docx',
})
await doc.opened // 문서가 화면에 뜨면 끝, 실패하면 SeuofficeEmbedError
doc.destroy()
```

## 보존된 공동 편집 판을 파일로 꺼내기

`renderHistory(container, { baseUrl, fileName, bytes, history })`는 현재 열린 편집기와 별개로 보기 전용 iframe을 만든다. `bytes`는 에포크의 기준 파일이고 `history`는 같은 에포크의 형식·기준 해시·버전과 그 버전까지의 변경이다. 성공하면 `{ fileName, bytes }`를 반환하고 iframe을 제거한다. 이 호출에는 공동 편집 소켓이나 `onSave`가 필요 없다.

호스트가 파일을 영구 보관하거나 연산을 취소하려면 신뢰하는 호스트 작업자에서 이 함수를 실행한다. 사용자의 브라우저가 돌려준 바이트나 보상 연산을 곧바로 저장하지 않는다. [이력 서버와 호스트의 권한·저장 계약](../collab/README.md#이력-파일-꺼내기-새-판-복원-선택-취소)을 따른다. 이력 지원을 선언하지 않은 편집기는 `unsupported`, 이력이 잘못되면 `invalid_history`, 다른 편집을 보존하는 선택 취소가 불가능하면 `conflict`다.

## 다른 출처에서 서빙할 때

웹 묶음을 붙이는 쪽 페이지와 다른 출처(주소의 scheme, 호스트, 포트 중 하나라도 다르면 다른 출처)에서 서빙하면, 묶음의 페이지는 그 호스트가 보낸 `open`을 기본으로 받지 않는다. 받지 않은 호스트에는 `origin_not_allowed`가 돌아가 `opened`가 바로 reject된다(아래 "오류"). 묶음을 서빙하는 쪽이 허용할 호스트 출처를 묶음 루트(`docs/`, `pdf/` 폴더와 같은 자리)의 `embed-origins.json`에 적는다.

```json
{ "origins": ["https://host.example", "https://staging.host.example:8443"] }
```

- 페이지는 시작할 때 이 파일을 같은 출처에서 매번 새로 받는다(`cache: 'no-store'`, 리다이렉트 없음, 5초 안에). 파일을 고치면 다음에 여는 문서부터 바뀐다.
- 항목은 `scheme://host[:port]`와 정확히 같아야 한다. 기본 포트와 대소문자는 정리해서 비교한다. 와일드카드(`*`), 경로, 쿼리, `null`, http(s)가 아닌 주소는 안 된다.
- 파일이 없거나, 받지 못하거나, 상태가 200이 아니거나, JSON이 아니거나, 모양이 틀리면(항목 하나만 틀려도 파일 전체) 자기 출처의 호스트만 받는다. 닫힌 쪽으로 실패한다. 파일이 있는데 모양이 틀린 경우에는 콘솔에 경고가 한 줄 남는다.
- 파일이 오기 전에 호스트의 `open`이 먼저 도착하면 버리지 않고 기다렸다가, 허용 목록이 정해진 뒤에 판단한다. `ready`는 기다리지 않고 바로 나간다.
- 허용 출처는 서빙하는 쪽만 정한다. 붙이는 쪽 페이지의 주소(쿼리, 해시), 메시지, `window.name`, 리퍼러에서는 읽지 않는다.
- 파일은 빌드가 아니라 서빙할 때 놓는다. 그래서 릴리스 묶음(`seuoffice-web-<ver>.tgz`)을 해시로 고정한 채 환경마다 다른 `embed-origins.json`만 얹으면 된다. 예전 빌드 변수 `VITE_SEUOFFICE_EMBED_ORIGINS`는 없어졌다.
- 호스트와 같은 출처에서 서빙하면 파일이 필요 없다.

### iframe `sandbox`와 `allow`

`openDocument`는 iframe에 기본으로 `sandbox="allow-scripts allow-same-origin allow-downloads"`(`DEFAULT_SANDBOX`)를 붙인다. 팝업, 대화상자, 폼 제출, 위쪽 창 이동은 닫혀 있다. 호스트는 `sandbox`로 넓히거나 좁히고, `allow`로 권한 정책을 준다.

```ts
openDocument(container, {
  baseUrl,
  bytes,
  fileName,
  sandbox: 'allow-scripts allow-same-origin allow-downloads allow-modals', // 기본값을 바꿀 때만
  allow: 'clipboard-read; clipboard-write', // 기본은 없음
})
```

- `sandbox`에 `allow-scripts`와 `allow-same-origin`이 하나라도 없으면 `openDocument`가 바로 `invalid_sandbox`로 reject한다. `allow-same-origin`이 없으면 프레임 안 페이지의 출처가 `null`이 되어 호스트가 `ready`를 거르고 `timeout`까지 기다리게 되기 때문이다. 같은 출처가 아니면 페이지가 `embed-origins.json`을 읽는 일도 막힌다.
- 묶음을 호스트와 같은 출처에서 서빙하면 `allow-scripts allow-same-origin` 조합은 격리가 아니다. 프레임 안 스크립트가 자기 `sandbox` 속성을 떼고 다시 불러올 수 있고, 같은 출처라서 호스트의 쿠키와 저장소에도 닿는다. 묶음은 호스트와 다른 출처에서 서빙하기를 권한다. 다른 출처면 이 sandbox는 호스트의 쿠키, 저장소, DOM을 지킨다.

`apps/web/tests/embed-origins.spec.ts`로 크로미움에서 확인한 결과다. 호스트는 `127.0.0.1`, 묶음은 `localhost`로 서로 다른 사이트에서 서빙했다. 앱은 pdf와 hwp로 시험했다(docs, slides, sheets는 이 환경에서 빌드하지 못해 시험하지 않았다).

| 기능                                                               | 기본 sandbox에서                                                                                                                                                        | 필요한 것                                               |
| ------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| 열기: pdf.js 워커, WASM, 글꼴, rhwp WASM                           | 된다. 실패한 요청이 없다                                                                                                                                                | 기본 값이면 충분하다                                    |
| 편집 후 저장(`onSave`)                                             | 된다. 저장은 호스트에서 일어난다                                                                                                                                        | 없음                                                    |
| 페이지가 직접 하는 내려받기                                        | 된다                                                                                                                                                                    | `allow-downloads`(기본에 있다). 빼면 막힌다             |
| `alert`, `confirm`, `prompt`                                       | 브라우저 대화상자는 막힌다. 앱이 묻는 확인, 알림, 입력은 페이지 안 대화상자로 뜬다(머리글의 "이전 구역에 연결", 목차 알림, hwp 스타일 삭제 등, `embed-dialogs.spec.ts`) | 없음. `allow-modals`가 필요 없다                        |
| `window.open`                                                      | 막힌다                                                                                                                                                                  | `allow-popups`                                          |
| `window.print`                                                     | 인쇄 창은 막힌다(`beforeprint`가 일어나지 않는다). pdf 앱은 인쇄를 누르면 "이 화면에서는 인쇄 창을 열 수 없습니다"를 페이지 안에 알린다                                 | 인쇄 창이 필요할 때만 `allow-modals`                    |
| 선택한 글 Ctrl+C(보기), Ctrl+V(고치기)                             | 된다. 브라우저가 직접 하는 일이다                                                                                                                                       | 없음                                                    |
| `navigator.clipboard.writeText`(맥락 메뉴의 링크 복사, AI 답 복사) | `NotAllowedError`                                                                                                                                                       | `allow="clipboard-write"`                               |
| `navigator.clipboard.readText`, `read`(붙여넣기 동작)              | `NotAllowedError`                                                                                                                                                       | `allow="clipboard-read; clipboard-write"`로 읽기가 된다 |
| `allow-forms`                                                      | 시험하지 않았다. SeuOffice는 폼을 제출하지 않는다                                                                                                                       |                                                         |

- 클립보드는 `allow="clipboard-write"`만 주면 쓰기만 되고 읽기는 `NotAllowedError`다. `clipboard-read; clipboard-write`를 주면 둘 다 된다(`clipboard-read` 단독은 시험하지 않았다). 브라우저는 읽기에 사용자 허용도 따로 묻는다. 시험에서는 권한을 미리 줬다. 프레임이 포커스를 가진 상태에서만 `navigator.clipboard`가 동작한다.
- 권장 값은 기본값이다. 붙여넣기 메뉴와 복사 단추가 필요하면 `allow="clipboard-read; clipboard-write"`를 더하고, 인쇄 창이나 외부 링크가 필요하면 `allow-modals allow-popups`를 더한다.
- 고치기와 보기 모두 앱은 `alert`, `confirm`, `prompt` 대신 페이지 안 대화상자를 띄우고, 막힌 인쇄와 열리지 않는 링크도 페이지 안에서 알린다. 이 바꿔치기는 웹 묶음에서만 하고 데스크톱 앱은 운영체제 대화상자를 그대로 쓴다.

## 고치기

호스트가 그 문서의 편집 잠금을 먼저 잡은 다음 `mode: 'edit'`로 연다. 저장은 `onSave`가 맡는다. 호스트가 바이트를 저장하면 resolve하고, 저장하지 못하면 이유와 함께 reject한다.

```ts
const doc = openDocument(container, {
  baseUrl: `${location.origin}/seuoffice/`,
  bytes,
  fileName: 'report.docx',
  mode: 'edit',
  onSave: async ({ fileName, bytes }) => {
    await uploadToServer(fileName, bytes) // 실패하면 throw
  },
  onDirtyChange: (dirty) => setUnsaved(dirty),
})

await doc.save() // 페이지에 저장을 요청한다
```

### 모바일에서 다른 탭으로 갔다 돌아올 때

`onDirtyChange(true)`는 저장할 변경이 있다는 알림이다. 복구할 문서 바이트는 이때가 아니라 `onSave`에 온다. 호스트는 탭이 숨겨졌다는 이유로 iframe을 없애거나 다시 만들지 않는다. 편집 화면을 닫기 전에 변경을 보존해야 한다면 화면이 열려 있을 때 `await doc.save()`를 호출한다. 호스트 저장소에 쓰기를 마친 뒤에만 `onSave`를 resolve한다. 저장이 실패하면 iframe을 유지하고 다시 저장할 수 있게 한다.

v0.12.51의 모의 docx로 크로미움 모바일 에뮬레이션(390 × 844, `autoSave: false`)에서 확인했다. 화면 너비만 바꾸면 `visibilitychange`가 없었다. 다른 탭을 열어 앞에 놓고 돌아오면 호스트와 iframe 모두 `hidden`, `visible`을 차례로 받았다. 편집한 글과 미저장 상태가 남았고 `onSave`는 불리지 않았다. 복귀 뒤 첫 `doc.save()`에서 호스트가 저장을 거절하자 `save_failed`가 돌아왔고 글과 미저장 상태가 유지됐다. 다시 저장하자 미저장 표시가 사라졌고 저장된 docx 본문에서 편집한 글을 확인했다.

이 결과는 같은 iframe이 살아 있는 동안에 한한다. 모바일 운영체제가 탭을 버리거나 앱을 끝낸 뒤의 복구를 보장하지 않는다. 종료 시점의 비동기 저장에 의존하지 않는다.

### 자동 저장과 저장 사유

`autoSave`는 고치기 모드에서만 뜻이 있다. 기본값은 `false`다.

- `false`(기본): 페이지에 자동 저장 단추가 없고 자동 저장 타이머가 돌지 않는다. 같은 출처의 localStorage에 이전 페이지가 남긴 자동 저장 값이 있어도 읽지 않는다. 저장은 사람이 저장 단추나 Ctrl+S로 하거나 호스트가 `doc.save()`를 부를 때만 일어난다.
- `true`: 앱의 자동 저장이 데스크톱처럼 일한다(30초마다, 창이 포커스를 잃을 때, 저장할 변경이 있을 때). 단추가 보이고 켜진 채로 시작한다. 사람이 끌 수 있지만 그 선택은 페이지가 열려 있는 동안만 가고 localStorage에 쓰지 않는다. 자동 저장마다 `onSave`가 한 번 불리므로, 저장마다 새 판이 생기는 저장소라면 판이 자주 생긴다.

`onSave`는 `file.reason`을 받는다.

| reason | 언제                                     |
| ------ | ---------------------------------------- |
| `auto` | 앱의 자동 저장                           |
| `user` | 사람이 저장 단추, 메뉴, Ctrl+S로 한 저장 |
| `host` | 호스트의 `doc.save()`가 시작한 저장      |

저장 하나하나가 자기 `reason`을 들고 간다. 자동 저장과 `doc.save()`가 겹쳐도 각각 제 사유로 `onSave`에 닿는다. 사유를 보내지 않는 옛 페이지의 저장은 `user`로 읽는다.

| 형식 | 자동 저장이 도는 때(`autoSave: true`)                                                | `onSave`에 닿는가                                                                                                            |
| ---- | ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------- |
| docx | 30초, 포커스 잃음. 글 조합 중이거나 표 칸·글상자를 고치는 중이면 건너뛴다            | 닿는다(`auto`)                                                                                                               |
| xlsx | 30초, 포커스 잃음, AI 실행이 셀을 바꾼 뒤. 저장 중이거나 셀을 고치는 중이면 건너뛴다 | 닿는다(`auto`)                                                                                                               |
| pptx | 30초, 포커스 잃음. 글·표 칸을 고치는 중이거나 마우스를 끌고 있으면 건너뛴다          | 닿는다(`auto`). 이전 판은 웹에서 자동 저장 호출을 거절했다                                                                   |
| pdf  | 30초, 포커스 잃음. 읽기 전용이거나 발표 중이면 건너뛴다                              | 웹이 저장하는 편집이면 닿는다. 텍스트 편집·추가, 이미지 편집, 저장된 주석 삭제, 영구 제거는 웹에서 저장하지 못해 닿지 않는다 |
| hwp  | 30초, 포커스 잃음. 마지막 입력 뒤 2초가 지나야 한다. 메모를 고치는 중이면 건너뛴다   | 저장할 수 있는 HWP이면 닿는다(`auto`)                                                                                        |

공동 편집에서도 같은 규칙이다. docx·pdf·pptx는 호스트가 이미 그 판을 가졌으면 자동 저장이 `onSave`를 부르지 않는다.

## 페이지가 SeuOffice 출처에 남기는 것

임베드한 페이지는 문서에서 나온 글이나 파일 이름을 SeuOffice 출처의 localStorage에 남기지 않는다(옵션이 아니라 기본 동작이다). 웹 빌드는 `localStorage`를 갈아 끼워, 아래 "남는 키"만 실제 저장소에 쓰고 나머지 모든 키는 페이지가 열려 있는 동안 메모리에만 둔다. 앞으로 생기는 키도 기본은 메모리다. 남기려면 `packages/web-platform/src/embed-storage.ts`의 `PERSISTED_KEYS`에 넣고 이 표에 행을 더한다.

| 키                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | 내용                                                    | 임베드에서                                                |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------- | --------------------------------------------------------- |
| `genoffice.sheets.findHistory`, `genoffice.sheets.replaceHistory`                                                                                                                                                                                                                                                                                                                                                                                                              | 찾기·바꾸기 창에 입력한 글                              | 메모리만. 이전 값은 읽지 않고 시작할 때 지운다            |
| `aisheets.spell-dictionary`                                                                                                                                                                                                                                                                                                                                                                                                                                                    | "사전에 추가"한 단어                                    | 메모리만. 시작할 때 지운다                                |
| `ai-excel-chat-history`                                                                                                                                                                                                                                                                                                                                                                                                                                                        | 옛 AI 대화(사용자와 모델의 글)                          | 읽지 않는다. 시작할 때 지운다                             |
| `aidocs.list.recent.bullets`, `.numbers`, `.multi`                                                                                                                                                                                                                                                                                                                                                                                                                             | 최근 쓴 목록 서식(글머리·번호의 글자)                   | 메모리만. 시작할 때 지운다                                |
| `ai-slides-custom-shows:<경로>`                                                                                                                                                                                                                                                                                                                                                                                                                                                | 쇼 재구성 이름과 슬라이드 번호. 경로에 파일 이름이 있다 | 메모리만. 시작할 때 지운다                                |
| `ai-slides-guides:<경로>`                                                                                                                                                                                                                                                                                                                                                                                                                                                      | 안내선. 키에 파일 이름이 있다                           | 메모리만. 시작할 때 지운다                                |
| `genoffice-pdf-view-state`                                                                                                                                                                                                                                                                                                                                                                                                                                                     | 파일 경로별 쪽·스크롤·배율                              | 메모리만. 시작할 때 지운다                                |
| `genoffice-pdf-job-pages:<해시>:…`                                                                                                                                                                                                                                                                                                                                                                                                                                             | AI 작업 대상 쪽 번호                                    | 메모리만. 시작할 때 지운다                                |
| `rhwp-document-font-preferences`                                                                                                                                                                                                                                                                                                                                                                                                                                               | HWP 편집기가 문서 지문별로 적는 글꼴 대체               | 시작할 때 지운다                                          |
| `aidocs.autoSave`, `ai-sheets-auto-save`, `ai-slides-auto-save`, `pdfapp.autoSave`, `hwpapp.autoSave`                                                                                                                                                                                                                                                                                                                                                                          | 자동 저장 단추                                          | 읽지도 쓰지도 않는다. `autoSave` 옵션이 정한다            |
| 그 밖의 모든 키(최근 함수 이름, 빠른 실행 목록, 패널 열림, 잉크 프리셋 등)                                                                                                                                                                                                                                                                                                                                                                                                     | 문서와 무관한 화면 설정                                 | 메모리만                                                  |
| 남는 키: `aidocs.ribbonCollapsed`, `ai-sheets-ribbon-collapsed`, `ai-slides-ribbon-collapsed`, `genoffice-pdf-ribbon-collapsed`, `docs-ai-panel-width`, `sheets-ai-panel-width`, `pdf-ai-panel-width`, `hwp-ai-panel-width`, `aidocs.navWidth`, `ai-slides-thumbs-width`, `ai-slides-presenter-side-width`, `genoffice-pdf-sidebar-width`, `genoffice-pdf-bilingual-width`, `hwpapp.memoPanelWidth`, `aidocs.leftPane`, `genoffice-pdf-sidebar-open`, `aidocs.measurementUnit` | 너비·접힘·단위 같은 화면 설정                           | localStorage에 남는다                                     |
| `rhwp-settings`                                                                                                                                                                                                                                                                                                                                                                                                                                                                | 앱이 HWP 편집기 틀에 심는 설정(테마, 복구 초안 끔)      | localStorage에 남는다. 편집기 틀이 실제 저장소에서 읽는다 |

sessionStorage는 쓰지 않는다. 이 표는 localStorage만 다룬다. HWP 편집기 틀이 IndexedDB에 만드는 DB(`rhwpStudio…`)는 이 변경 범위 밖이고 시험하지 않았다.

## AI

`ai`를 넘기면 문서 안 AI 도우미가 켜진다. 보기·고치기 둘 다 된다. 페이지는 토큰을 갖지 않는다. AI 한 턴마다 OpenAI Chat Completions 요청 본문(`gateway-complete`, 결정 0009)을 호스트에 넘기고, 호스트가 자기 토큰을 붙여 게이트웨이를 부른 뒤 응답을 돌려준다. SSE 본문은 그대로 페이지로 흘러간다.

```ts
const doc = openDocument(container, {
  baseUrl: `${location.origin}/seuoffice/`,
  bytes,
  fileName: 'report.docx',
  ai: {
    request: (body, signal) =>
      fetch(`${gatewayUrl}/chat/completions`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
        signal, // 사용자가 멈추거나 doc.destroy()하면 끊긴다
      }),
  },
})
```

- 2xx가 아닌 응답은 상태 번호와 본문이 페이지로 가서 데스크톱과 같은 오류 문구가 된다. 거절할 때는 429 같은 응답을 돌려주면 된다. 본문에 토큰을 싣지 않는다.
- `ai`를 주지 않으면 페이지는 "AI 연결 안 됨"을 보이고 아무것도 보내지 않는다. AI를 쓸 수 없는 사람에게는 넘기지 않는다.
- 요청 수와 토큰 양 제한은 호스트가 한다. 도구를 부를 때마다 요청이 하나 더 온다.
- 응답 머리글은 `Content-Type`과 `Retry-After`만 페이지로 간다. 혼잡 응답에 `Retry-After`를 20초보다 길게 주면 페이지는 다시 보내지 않는다.

## 함께 고치기

`collab`을 넘기면 여러 사람이 같은 문서를 동시에 고친다. 보기·고치기 둘 다 된다. 공동 편집 서버와의 WebSocket은 호스트가 연다. 호스트가 자기 인증으로 연결을 만들고, 이 패키지는 페이지와 그 소켓 사이에서 프레임을 그대로 나른다. 페이지는 문서 주소나 토큰을 모른다.

아래 `collabOpen`은 SDK 필드가 아니라 호스트가 이 문서 열기에 발급한 핸들이다. 호스트 서버는 이 핸들을 사용자·문서와 공동 편집 에포크 하나에 묶는다. `connect`와 `onSave`는 재연결하거나 저장을 재시도해도 같은 핸들을 쓴다.

```ts
const collabOpen = await host.openCollab(documentId)
const open = (collab: boolean) =>
  openDocument(container, {
    baseUrl: `${location.origin}/seuoffice/`,
    bytes,
    fileName: 'report.docx',
    mode: 'edit',
    ...(collab
      ? {
          collab: {
            connect: () => host.connect(collabOpen),
            onStatus: (status) => showPeers(status.peers),
          },
          onSave: (file) => host.commitCollabFile(collabOpen, file),
        }
      : { onSave: async ({ fileName, bytes }) => uploadToServer(fileName, bytes) }),
  })

let doc = open(true)
try {
  await doc.opened
} catch (error) {
  if (!(error instanceof SeuofficeEmbedError) || error.code !== 'collab_unavailable') throw error
  // 이 앱은 아직 함께 고치기를 못 한다: 닫고 편집 잠금을 잡은 뒤 collab 없이 다시 연다
  doc.destroy()
  await takeEditLock(documentId)
  doc = open(false)
  await doc.opened
}
```

- 호스트 흐름은 이렇다. `collab`을 넣어 연다. `opened`가 `collab_unavailable`로 실패하면 그 문서를 `destroy()`하고, 편집 잠금을 잡은 뒤 `collab` 없이 다시 연다. 함께 고치기를 못 하는 앱에는 문서를 보내지 않으므로 그 사람만의 사본이 생기지 않는다.
- 함께 고칠 때는 편집 잠금을 잡지 않는다. 같은 문서를 연 사람은 모두 같은 서버에 붙는다.
- `connect()`는 페이지가 연결을 원할 때마다 불린다. 끊긴 뒤 다시 붙을 때도 불리고, 그때 이전 소켓은 닫힌다. 페이지가 아무리 자주 원해도 `connect()`는 250ms에 한 번까지만 불린다.
- `connect()`는 아직 열리는 중인 소켓도, 이미 열린 소켓도 돌려줄 수 있다. 실패하거나, 소켓에 오류가 나거나, 15초(`connectTimeoutMs`) 안에 열리지 않으면 페이지는 끊긴 것으로 보고 다시 시도한다.
- `onSave`가 받는 `CollabSavedFile`에는 `version`은 있지만 에포크(`epoch`)는 없다. `version`은 한 에포크 안에서만 비교한다. 에포크가 바뀌면 기존 핸들을 새 에포크에 다시 묶지 않는다. 다시 열어 새 핸들을 받는다.
- `host.commitCollabFile`은 호스트가 구현한다. 현재 쓰기 권한, 핸들의 에포크와 현재 에포크의 일치, 같은 에포크에서 저장한 판보다 낮지 않은 `version`을 한 트랜잭션에서 확인하고 저장 판과 파일 포인터를 함께 갱신한다. 핸들이 어느 에포크인지 확정할 수 없거나 이 조건부 갱신을 할 수 없으면 `onSave`를 거절한다. 중계하는 소켓 프레임은 공개 API가 아니므로 해석해 에포크를 알아내지 않는다. [호스트의 파일 저장 계약](../collab/README.md#호스트가-꼭-할-일)을 따른다.
- `doc.collabCapable`은 페이지가 `ready`에서 알린 값이다. 페이지가 답하지 않았거나 문서가 닫혔으면 `false`다.
- `doc.destroy()`는 소켓도 닫는다. `collab`을 주지 않으면 공동 편집 메시지는 하나도 오가지 않는다.

`doc.exportLocal()`은 그 사람 화면에 보이는 그대로의 파일(`{ fileName, bytes }`)을 돌려준다. `collab`을 넘겨 연 문서에만 있다. 공동 편집이 `diverged`나 `failed`로 끝났을 때, 또는 연결이 끊긴 사람이 사본을 갖고 싶을 때 쓴다. 이 파일에는 판 번호가 없고 서버가 확정한 내용과 다를 수 있으므로, 저장된 문서를 자동으로 덮어쓰면 안 된다. 내려받기나 "사본으로 저장"으로 건넨다. `onSave`를 부르지 않고 소켓도 건드리지 않는다. 페이지가 거절하거나 60초(`collab.exportTimeoutMs`) 안에 답하지 않거나 문서가 닫히면 `save_failed`로 reject한다.

## 첫 쪽 그림

`doc.thumbnail()`은 문서의 첫 쪽을 PNG로 돌려준다. 보기와 고치기 모두 된다. `opened`가 끝난 직후에 불러도 된다. 페이지가 문서를 아직 화면에 올리는 중이면 다 올린 뒤에 그린다.

```ts
await doc.opened
const { bytes, width, height } = await doc.thumbnail({ width: 480 })
const url = URL.createObjectURL(new Blob([bytes], { type: 'image/png' }))
```

- `width`는 그림의 가로 픽셀 수다. 주지 않으면 480이다. 64보다 작으면 64로, 1024보다 크면 1024로 그린다. 소수는 반올림하고, 숫자가 아니면 480으로 그린다. 그린 크기는 답의 `width`와 `height`에 있다.
- 세로는 쪽의 비율을 따른다. 세로가 가로의 4배를 넘는 쪽은 위에서부터 4배까지만 담는다.
- 그림에는 투명한 곳이 없다. 배경이 없는 쪽은 흰 바탕에 그린다.
- PNG가 2 MiB(2,097,152바이트)를 넘으면 `thumbnail_too_large`로 reject한다. 더 작은 `width`로 다시 부른다.
- 페이지는 10초 안에 그리지 못하면 `thumbnail_timeout`으로 답한다. 호스트 쪽은 답을 15초까지 기다린다. 이 기능이 없는 옛 웹 묶음은 답하지 않으므로 15초 뒤에 `thumbnail_timeout`이 된다.
- 화면을 찍은 그림이 아니다. 페이지가 첫 쪽을 따로 그리므로 배율, 스크롤 위치, 도구 모음, 선택 표시가 그림에 없다.
- iframe의 `sandbox`와 `allow`는 기본값 그대로 쓴다.

| 형식      | 그림에 담기는 것                                              | 알아 둘 것                                                                                                                                     |
| --------- | ------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| pdf       | 파일의 1쪽                                                    | 저장하지 않은 주석과 편집은 담기지 않는다. 문서가 화면에 올라오지 않으면(암호 입력을 기다리는 PDF) `thumbnail_timeout`이다                     |
| pptx      | 첫 슬라이드. 편집 화면과 같은 코드로 그린다                   | 저장하지 않은 편집도 담긴다. 그림과 글꼴이 다 올라온 뒤에 그린다. 가려진 탭처럼 브라우저가 화면을 그리지 않는 동안에는 `thumbnail_timeout`이다 |
| hwp, hwpx | 1쪽. 인쇄 미리 보기와 같은 그림이다                           | 저장하지 않은 편집도 담긴다. 글자는 기기에 깔린 글꼴로 그리므로 편집 화면과 글자 모양이 다를 수 있다                                           |
| docx      | 1쪽. 편집 화면이 나눈 쪽을 옮겨 그린다                        | 저장하지 않은 편집도 담긴다. 쪽 나누기가 끝난 뒤에 그리므로 1초쯤 걸린다. 종이는 어두운 쪽 보기에서도 희다. 글꼴은 아래 "docx의 글꼴"을 본다   |
| xlsx      | 보이는 시트의 왼쪽 위(A1부터 Z100까지)를 인쇄 배치로 놓은 1쪽 | 저장하지 않은 편집도 담긴다. 아래 "xlsx의 그림"을 본다                                                                                         |

**docx의 글꼴.** 그림은 바깥 자원을 읽지 못한다. 그래서 앱이 싣고 있는 웹 글꼴 가운데 1쪽의 글자가 쓰는 것만 그림에 넣고, 합쳐서 6 MiB까지 넣는다. 넘는 글꼴과 문서에 박힌 글꼴은 기기에 깔린 글꼴로 그려져 편집 화면과 글자 모양이 다를 수 있다. 크로미움 계열 브라우저는 웹 글꼴의 굵은 한글과 한자를 보통 굵기로 그린다. 사파리에서 화면이 좁아 배율이 낮으면 문단 사이가 편집 화면보다 벌어진다.

**xlsx의 그림.** 화면의 격자를 찍지 않고 인쇄할 때의 배치로 그린다. 배율 100%, 행과 열의 머리글, 눈금선(시트가 눈금선을 숨기면 없다)으로 그리고, 용지 크기와 방향, 여백, 머리글과 바닥글은 파일의 쪽 설정을 따른다. 셀의 글자, 글꼴, 채우기, 테두리, 병합, 차트가 담긴다. 조건부 서식, 스파크라인, 셀 안 그림, 틀 고정 선, 필터 단추, 선택 표시는 담기지 않는다. 통합 문서를 다 읽기 전이면 보이는 시트의 셀이 온 뒤에 그리고, 큰 파일에서 10초를 넘기면 `thumbnail_timeout`이다.

`thumbnail()`은 `SeuofficeEmbedError`로 reject하고, `code`는 아래 넷 중 하나다.

| 코드                    | 언제                                                                                                                                                |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `thumbnail_unsupported` | 문서가 열려 있지 않음(`opened` 전, 열기 실패, `destroy()` 뒤). 페이지가 그림을 만들지 못함(까닭은 `message`에 있다). v0.12.48 웹 묶음의 xlsx와 docx |
| `thumbnail_timeout`     | 페이지가 10초 안에 그리지 못함. 또는 페이지가 15초 안에 답하지 않음                                                                                 |
| `thumbnail_too_large`   | PNG가 2 MiB를 넘음                                                                                                                                  |
| `thumbnail_tainted`     | 쪽에 다른 출처의 그림이 있어서 브라우저가 그 캔버스를 읽지 못하게 함                                                                                |

### 메시지

이 패키지를 쓰지 않고 `postMessage`로 페이지와 직접 말하는 호스트는 아래 모양을 쓴다. 다른 메시지처럼 `source: 'seuoffice'`와 `version: 1`이 붙고, 페이지는 `open`을 보낸 창과 출처의 요청만 받는다.

```ts
// 호스트 → 페이지. requestId는 호스트가 정하는 문자열이고 답에 그대로 돌아온다
{ type: 'thumbnail', requestId: string, width?: number }

// 페이지 → 호스트. bytes는 PNG이고 transfer로 넘어온다
{ type: 'thumbnail_done', requestId: string, ok: true, bytes: ArrayBuffer, width: number, height: number }
{ type: 'thumbnail_done', requestId: string, ok: false, code: string, message: string }
```

## 오류

`SeuofficeEmbedError.code`는 `opened`가 reject될 때 이유를 나타내는 고정 값이다. `doc.thumbnail()`의 `thumbnail_*` 코드는 위 "첫 쪽 그림"에 있다.

| 코드                 | 언제                                                                                                   |
| -------------------- | ------------------------------------------------------------------------------------------------------ |
| `unsupported_format` | 어떤 앱도 이 확장자를 열지 못함                                                                        |
| `too_large`          | 50 MB 초과                                                                                             |
| `open_failed`        | 페이지가 문서를 열지 못함                                                                              |
| `timeout`            | 페이지가 `readyTimeoutMs`(기본 30000ms) 안에 `ready`를 알리지 않음                                     |
| `open_timeout`       | 호스트가 바이트를 보낸 뒤 `openTimeoutMs`(기본 60000ms) 안에 페이지가 `opened`도 `error`도 답하지 않음 |
| `origin_not_allowed` | 페이지가 이 호스트의 출처를 허용하지 않음(`embed-origins.json`에 없거나, 파일이 없거나 깨짐)           |
| `invalid_sandbox`    | `sandbox`에 `allow-scripts`나 `allow-same-origin`이 없음. 프레임을 만들지 않고 바로 reject한다         |
| `save_failed`        | 저장하지 못함                                                                                          |
| `collab_unavailable` | `collab`을 넘겼는데 그 앱이 함께 고치기를 못 함                                                        |

`opened`는 항상 정해진다. 열리거나, 위 코드 중 하나로 reject된다. 호스트가 허용 목록에 없으면 페이지가 `origin_not_allowed`만 돌려보내 호스트가 30초 넘게 기다리지 않는다. 이 답에는 문서 내용도 허용 출처 목록도 없고, 코드만 호스트의 출처로 간다. 페이지가 응답하지 못하면(출처가 `null`인 프레임 등) `timeout`이 마지막 안전망이다. `open_timeout` 기본값 60초는 50 MB 상한의 문서를 느린 기기에서 열 때 걸리는 시간을 넉넉히 덮으려고 정했다. 호스트가 `openTimeoutMs`로 바꿀 수 있다.

## 라이선스

FSL-1.1-ALv2(소스 공개, 판마다 2년 뒤 Apache-2.0), GenOffice·이음오피스에서 온 부분은 Apache-2.0. 저장소: https://github.com/seuput-inc/seuoffice
