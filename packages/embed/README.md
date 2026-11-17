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

| 기능                                                               | 기본 sandbox에서                                                                                                                                                       | 필요한 것                                               |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------- |
| 열기: pdf.js 워커, WASM, 글꼴, rhwp WASM                           | 된다. 실패한 요청이 없다                                                                                                                                               | 기본 값이면 충분하다                                    |
| 편집 후 저장(`onSave`)                                             | 된다. 저장은 호스트에서 일어난다                                                                                                                                       | 없음                                                    |
| 페이지가 직접 하는 내려받기                                        | 된다                                                                                                                                                                   | `allow-downloads`(기본에 있다). 빼면 막힌다             |
| `alert`, `confirm`, `prompt`                                       | 막힌다. 대화상자가 뜨지 않는다                                                                                                                                         | `allow-modals`                                          |
| `window.open`                                                      | 막힌다                                                                                                                                                                 | `allow-popups`                                          |
| `window.print`                                                     | 막힌다. `beforeprint`, `afterprint`가 일어나지 않는다. pdf 앱의 인쇄 코드는 `afterprint`를 기다리다 시간 초과로 끝난다(코드로 본 것이고 앱 화면에서는 시험하지 않았다) | `allow-modals`. 주면 `beforeprint`가 일어난다           |
| 선택한 글 Ctrl+C(보기), Ctrl+V(고치기)                             | 된다. 브라우저가 직접 하는 일이다                                                                                                                                      | 없음                                                    |
| `navigator.clipboard.writeText`(맥락 메뉴의 링크 복사, AI 답 복사) | `NotAllowedError`                                                                                                                                                      | `allow="clipboard-write"`                               |
| `navigator.clipboard.readText`, `read`(붙여넣기 동작)              | `NotAllowedError`                                                                                                                                                      | `allow="clipboard-read; clipboard-write"`로 읽기가 된다 |
| `allow-forms`                                                      | 시험하지 않았다. SeuOffice는 폼을 제출하지 않는다                                                                                                                      |                                                         |

- 클립보드는 `allow="clipboard-write"`만 주면 쓰기만 되고 읽기는 `NotAllowedError`다. `clipboard-read; clipboard-write`를 주면 둘 다 된다(`clipboard-read` 단독은 시험하지 않았다). 브라우저는 읽기에 사용자 허용도 따로 묻는다. 시험에서는 권한을 미리 줬다. 프레임이 포커스를 가진 상태에서만 `navigator.clipboard`가 동작한다.
- 권장 값은 기본값이다. 붙여넣기 메뉴와 복사 단추가 필요하면 `allow="clipboard-read; clipboard-write"`를 더하고, 인쇄나 외부 링크가 필요하면 `allow-modals allow-popups`를 더한다.
- 보기 모드에서 `alert`, `confirm`, `window.open`, `window.print`를 부르는 앱 코드는 기본 sandbox에서 아무 일도 하지 못한다. 대화상자는 앱이 페이지 안에서 띄우도록 바꿔야 한다. `window.open`과 인쇄는 호스트가 위 값을 줘야 한다.

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

```ts
const open = (collab: boolean) =>
  openDocument(container, {
    baseUrl: `${location.origin}/seuoffice/`,
    bytes,
    fileName: 'report.docx',
    mode: 'edit',
    ...(collab
      ? {
          collab: {
            connect: () => new WebSocket(`${collabUrl}?doc=${documentId}`),
            onStatus: (status) => showPeers(status.peers),
          },
          onSave: async ({ fileName, bytes, version }) => {
            // 이미 저장된 판보다 낮은 version이면 throw한다
            await uploadToServer(fileName, bytes, version)
          },
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
- `onSave`는 `version`을 함께 받는다. 이미 저장한 판보다 낮은 `version`이 오면 `onSave`는 반드시 reject(throw)해야 한다. 그러지 않으면 늦게 도착한 오래된 저장이 새 저장을 덮어쓰고, 다음 편집은 그 오래된 파일에서 시작한다.
- `doc.collabCapable`은 페이지가 `ready`에서 알린 값이다. 페이지가 답하지 않았거나 문서가 닫혔으면 `false`다.
- `doc.destroy()`는 소켓도 닫는다. `collab`을 주지 않으면 공동 편집 메시지는 하나도 오가지 않는다.

`doc.exportLocal()`은 그 사람 화면에 보이는 그대로의 파일(`{ fileName, bytes }`)을 돌려준다. `collab`을 넘겨 연 문서에만 있다. 공동 편집이 `diverged`나 `failed`로 끝났을 때, 또는 연결이 끊긴 사람이 사본을 갖고 싶을 때 쓴다. 이 파일에는 판 번호가 없고 서버가 확정한 내용과 다를 수 있으므로, 저장된 문서를 자동으로 덮어쓰면 안 된다. 내려받기나 "사본으로 저장"으로 건넨다. `onSave`를 부르지 않고 소켓도 건드리지 않는다. 페이지가 거절하거나 60초(`collab.exportTimeoutMs`) 안에 답하지 않거나 문서가 닫히면 `save_failed`로 reject한다.

## 오류

`SeuofficeEmbedError.code`는 `opened`가 reject될 때 이유를 나타내는 고정 값이다.

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
