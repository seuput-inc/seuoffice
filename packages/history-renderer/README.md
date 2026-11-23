# 브라우저 없는 이력 렌더러

`@seuoffice/history-renderer`는 **미게시 로컬 후보**다. 입력은 보유한 원본과 그 원본에서 시작하는 `HistoryRequest`의 UTF-8 JSON 바이트다. 현재 파일에서 과거를 추정하지 않는다. 네 형식 모두 아래에 적은 편집만 처리하며 브라우저 편집기 전체를 대체하지 않는다.

```ts
import { renderBytes } from '@seuoffice/history-renderer'

const { bytes, fileName, compensation } = await renderBytes(baseBytes, requestJsonBytes)
```

공개 export는 `renderBytes(base: Uint8Array, requestJson: Uint8Array): Promise<HistoryResult>` 하나다. 호스트에서 신뢰되지 않은 요청을 먼저 `JSON.parse`하지 않는다. 결과의 `bytes`는 다시 열 수 있는 파일이다. `undo`를 주면 지정한 연산만 상쇄하는 wire 연산도 반환한다. 호스트가 아직 기록하지 않은 결과이며, 전체 버전을 롤백하지 않는다. `invalid_history`는 잘못된 입력·원본 해시·로그·대상을, `unsupported`는 처리할 수 없는 문서·연산·실행 환경을, `conflict`는 후속 편집을 보존하면서 취소할 수 없음을 뜻한다. 실패에는 부분 파일이나 보상이 없다.

## 브라우저와의 차이

아래에 허용한다고 적지 않은 연산과 구조는 지원 계약 밖이다. 한 변경에 여러 wire 연산이 있으면 모두 처리할 수 있어야 한다. 단순 문서에서도 일반 편집기가 부가 구조를 넣으면 이 후보에서 열리지 않을 수 있다.

| 형식           | 재생·선택 취소                                                                                                                                                                                                                                                   | 지원하지 않는 첫 경계                                                                                                                                                                                                                                                                                                |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| DOCX, 스키마 4 | 한 문단 안에서 `ReplaceStep`과 `AddMarkStep`·`RemoveMarkStep`을 복합 적용한다. 굵게·기울임·밑줄·취소선, 글꼴·색·크기·강조·음영·위아래 첨자·문자 간격 등 직접 서식을 기록한다. 동료의 독립 편집은 위치를 매핑하며, 이전 서식을 복원하지 못하는 역연산은 거부한다. | 문단 간 교체·표·필드·링크·추적 변경·측정이나 복합 문자 서식 해석이 필요한 경우, 문단 밖의 `side`는 받지 않는다. [브라우저 어댑터](../../apps/docs/src/web/collab-adapter.ts)                                                                                                                                         |
| XLSX, 스키마 3 | `sheet.mutation.set-range-values`로 A1:Z1000 안의 여러 칸에 숫자·문자열을 쓰거나 비운다. 보기의 눈금선과 오른쪽에서 왼쪽 쓰기도 재생한다. 셀 보상은 원본 타입과 서식을 보존하고 후속 쓰기가 겹치면 충돌한다.                                                     | `planCellEditsToXlsx`는 수식 재계산·참조 이동 엔진이 아니다. 수식·병합·스타일 편집, 행열 삽입·이동·크기, 탭·이름·보기의 나머지 [허용 목록](../../apps/sheets/src/web/collab-mutations.ts)은 지원하지 않는다. 브라우저의 단일 리터럴 셀 취소와 달리 안전하게 되돌릴 수 있는 여러 칸도 허용한다.                       |
| PPTX, 스키마 3 | 실제 `openDeck`·`applyOp`·`deckFile`로 `blankSlide`, `duplicateSlide`, `deleteSlides`, `notes`, `text`를 재생한다. 생성 취소는 그 슬라이드만 삭제하며 동료가 남긴 메모가 있으면 충돌한다. 같은 문단·런 배치를 유지하는 도형 텍스트도 복원한다.                   | 그룹·자동 맞춤·런 배치가 변하는 텍스트는 서식을 보장할 수 없다. 차트 내장 통합문서·OLE·압축 글꼴·SVG·EMF 등 중첩 입력은 열지 않는다. 메모·삭제 자체의 선택 취소는 제공하지 않는다. [브라우저 모델](../../apps/slides/src/web/collab-adapter.ts)                                                                      |
| PDF, 스키마 2  | 메타데이터·회전·`stampCfg`·미저장 `markups`를 기록하며, 브라우저와 같은 `capturePdfInverse`로 대상 편집을 선택 취소한다. 빈 페이지·순서·삭제도 재생한다. 고전 xref와 pdf-lib의 압축 xref·객체 스트림을 검증한다.                                                 | `noteEdits`, `drawings`, `formFields`, `formEdits`, `formWidgetEdits`는 지원하지 않는다. 브라우저도 `annotDeletes`, `textEdits`, `textInserts`, `imageEdits`는 거부한다. 일반 사진·내장 글꼴·임의의 주석·XMP·암호화·증분 갱신·중첩 페이지 트리는 받지 않는다. [브라우저 wire](../../apps/pdf/src/web/collab-wire.ts) |

PDF 도장은 워터마크·머리말·꼬리말·쪽 번호를 최종 표시 순서의 페이지에 기록한다. DOM 비트맵 대신 WinAnsi 텍스트(Helvetica) 또는 완성형 한글과 ASCII가 섞인 줄의 고정 Pretendard 윤곽을 사용한다. 호스트 글꼴과 픽셀 일치, 윤곽의 검색·복사는 보장하지 않는다. 한글 윤곽 줄은 128자, WinAnsi 줄은 1,024자 이하이며 페이지당 스트림 16개, 스트림당 2,048바이트 제한도 적용된다. 이모지·결합형·지원하지 않는 글꼴 대체는 추정하지 않는다.

미저장 주석은 형광펜·밑줄·취소선·물결 밑줄의 quad, 색, 제목과 실제 PDF 외형을 쓴다. 최종 주석 최대 128개, 주석마다 quad 16개·좌표 절댓값 14,400·외형 2,048바이트 이하다. 다른 ID는 유지한다. 같은 ID의 후속 편집이나 대상 페이지 회전·삭제와 충돌하면 보상을 기록하지 않는다. 저장된 주석을 미저장 ID로 역추정하지 않는다.

## 실행 경계와 한도

Node는 형식과 무관하게 바이트 길이만 검사해 파이프로 전달한다. 호출마다 패키지에 고정된 Deno 2.9.6을 시작한다. 게스트가 UTF-8, JSON, ZIP, XML, PDF를 처음 해석하며, 동일한 게스트에서 `replayHistory`와 형식별 저장까지 실행한다. 브라우저, DOM, 수신 포트, 외부 서비스는 사용하지 않는다. 결과 메타데이터와 파일만 Node로 돌아온다. 원본 ZIP의 압축 방식이나 바이트 배열 전체가 유지된다는 보장은 없다.

- 게스트에는 읽기·쓰기·네트워크·환경 변수·자식 프로세스·FFI·시스템 조회·원격 import 권한이 없다. 런타임은 설치된 고정 번들만 시작하며, 권한 요청·원격 모듈·npm·설정 탐색을 끈다. 런타임이 쓰는 임시 디렉터리는 호스트가 회수한다. 전역 `deno`나 다른 엔진으로 대체하지 않는다. 권한 정책은 [Deno 런타임](https://docs.deno.com/runtime/fundamentals/security/)을 신뢰한다. OS 가상 머신이나 전체 RSS 할당량은 아니다.
- 원본·결과 각 2 MiB, 요청 JSON 1 MiB, 변경 128개, 연산 256개, JSON 노드 16,384개, 깊이 32, 키·문자열 합계 256 KiB. `replayHistory`가 epoch·요청 버전·연속 버전·중복 제출·SHA-256·보상 크기를 추가 확인한다.
- OOXML은 단일 classic ZIP의 STORE/DEFLATE만 허용한다. ZIP 항목 256개, 항목당 해제 1 MiB, 전체 해제 4 MiB. 중앙·로컬 헤더, 이름·영역 중복, 실제 해제량, CRC를 확인한다. ZIP64·암호화·서명·추가 필드·주석은 받지 않는다. PDF 프로필은 1 MiB, 128페이지, 260객체 이하이다.
- DOCX·PPTX는 빌드가 공식 `office_wasm.wasm`의 실제 import 목록을 검사한 뒤 바이트를 고정 JS 모듈에 담는다. 게스트가 이 모듈을 읽으므로 WASM 경로를 열 권한은 필요하지 않다. WASI 환경 개수는 0이고 파일 열기·프로세스 실행·소켓 import는 없다. 허용 함수는 난수, 시계, 빈 환경, 출력, 오류 종료, 엔진 호출 중의 콜백과 정렬 비교다. WASM 선형 메모리는 128 MiB, V8 old space는 96 MiB, 호출당 제한 시간은 15초, 표준 출력·오류는 각각 약 3 MiB다. 동시 호출 수·전체 RSS·사용자별 할당량은 호스트가 제한해야 한다.

릴레이가 보관할 수 있는 문서 50 MiB와 이력 32 MiB는 이 렌더러의 입력 계약이 아니다. 한도를 넘는 원본·요청을 자르거나 중간 버전을 건너뛰어 출력하지 않는다. 50 MiB·32 MiB 처리량이나 메모리 안전 여유를 측정하지 않았으며 이 후보에서 한도를 올리지 않는다.

## 설치와 확인

manifest의 Node 요구는 `>=22.12`이고 glibc 최소 버전은 선언하지 않는다. Linux ARM64 glibc 또는 macOS ARM64용 `@deno/linux-arm64-glibc@2.9.6`·`@deno/darwin-arm64@2.9.6`을 설치 시점에 준비한다. `--omit=optional`은 사용할 수 없고 실행 중 다운로드는 없다. Debian 12 ARM64(Node 22.23.4·glibc 2.36)에서 이 후보의 포장본으로 네트워크 없이 DOCX 서식과 PDF 도장·주석을 재생하고 보상 파일을 다시 열었다. `4803a96a`의 네 형식 포장본은 Node 24.18.1·glibc 2.39에서 확인했다. Alpine musl·x64 바이너리는 제공하지 않는다.

워크스페이스의 개발용 `collab`은 `0.1.0`이지만 소비자가 받는 GitHub Releases의 번호와는 다르다. 예를 들어 실제 `v0.12.51` tarball 안의 `collab`은 `0.12.51`이고 `createHistoryServer`가 없어 사용할 수 없다. 아래 명령은 현재 체크아웃으로 **두 패키지를 같은 버전**에 포장하고 렌더러의 `dependencies["@seuoffice/collab"]`도 그 버전으로 고정한다. 소스 manifest·태그·릴리스를 바꾸지 않는다.

```sh
# 먼저 npm ci --ignore-scripts. 이어서 같은 소스의 공식 엔진과 고지를 준비한다.
# CI 기본 경로는 npm run build:wasm 및 node tools/gen-third-party-notices.mjs의 출력이다.
# 기존 빌드를 재사용할 때만 두 파일의 경로를 지정한다.
export SEUOFFICE_WASM=/path/to/office_wasm.wasm
export SEUOFFICE_WASM_NOTICES=/path/to/THIRD-PARTY-NOTICES.txt
node packages/history-renderer/pack-pair.mjs 0.12.52-rc.2 "$OUT"
# 별도 호스트에서 두 tarball 및 플랫폼용 npm 의존성을 미리 준비한 뒤 설치한다.
npm install --offline --ignore-scripts "$OUT/seuoffice-collab-0.12.52-rc.2.tgz" "$OUT/seuoffice-history-renderer-0.12.52-rc.2.tgz"
npm ls @seuoffice/collab @seuoffice/history-renderer
node node_modules/@seuoffice/history-renderer/examples/retained-host.mjs
```

`0.12.52-rc.2`는 로컬 조합을 식별하는 번호이지 게시한 판이 아니다. 공식 자산은 렌더러와 호환되는 `collab`을 **동일 커밋·판 번호**로 포함한 태그에서만 제공한다. [판 내기](../../docs/seuoffice/판-내기.md)를 참조한다. 이 후보는 출시 작업과 배포 목록을 바꾸지 않는다.

[호스트 예제](examples/retained-host.mjs)는 합성 DOCX와 독립된 두 편집을 보유하고, 첫 편집의 보상만 3번 버전에 기록한다. 같은 요청을 두 번 보내도 영수증이 같고 다시 연 문서에 두 번째 사람의 `Two?`가 남아야 한다. 예제의 메모리 저장소를 운영용으로 쓰라는 뜻은 아니다. 보유 범위, 인증·권한, 기대 epoch·버전에서의 원자적 추가, 중복 제거, 보상 저장은 기존 `createHistoryServer`와 호스트 저장소의 책임이다.

저장소의 형식 대조 시험과 [브라우저 시험](../../apps/web/tests/collab-history-formats.spec.ts)은 [동일한 원본 생성 함수·wire·영수증·재열기 검증](../../apps/web/tests/collab-history-cases.ts)을 사용한다. 독립 설치본 검증은 `node packages/history-renderer/tests/pack-tests.mjs "$CONSUMER/tests"`로 신뢰된 시험·원본 생성기·재열기 도구만 묶고, 소비자 디렉터리에서 Vitest를 실행한다. 실제 서버와 렌더러는 이 번들에 포함하지 않으므로 설치된 두 tarball이 사용된다. 시험 측 `SEUOFFICE_WASM`은 독립 재열기용이며 제품 게스트는 패키지에 든 WASM만 연다. `capabilities.test.ts`는 같은 설치본 실행기로 합성 자격 표식·파일·루프백·프로세스 접근 거부와 WASM 상한을 확인한다. 공개 API에는 실행할 모듈 경로를 받는 인자가 없다.
