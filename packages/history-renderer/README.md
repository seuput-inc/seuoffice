# 브라우저 없는 이력 렌더러

`@seuoffice/history-renderer@0.1.0-rc.0`은 **미게시 로컬 후보**다. 네 형식의 아래 부분집합만 처리한다. 일반 Office 문서나 브라우저 편집기 전체를 대체하지 않는다. 입력은 보유한 원본 파일과 그 원본에서 시작하는 `HistoryRequest`의 UTF-8 JSON 바이트다. 현재 파일에서 과거를 추정하지 않는다.

```ts
import { renderBytes } from '@seuoffice/history-renderer'

const { bytes, fileName, compensation } = await renderBytes(baseBytes, requestJsonBytes)
```

공개 export는 `renderBytes(base: Uint8Array, requestJson: Uint8Array): Promise<HistoryResult>` 하나다. 호스트에서 신뢰되지 않은 요청을 먼저 `JSON.parse`하지 않는다. 결과의 `bytes`는 다시 열 수 있는 파일이다. `undo`를 주면 지정한 연산만 상쇄하는 wire 연산도 반환한다. 호스트가 아직 기록하지 않은 결과이며, 전체 버전을 롤백하지 않는다. `invalid_history`는 잘못된 입력·원본 해시·로그·대상을, `unsupported`는 처리할 수 없는 문서·연산·실행 환경을, `conflict`는 후속 편집을 보존하면서 취소할 수 없음을 뜻한다. 실패에는 부분 파일이나 보상이 없다.

## 브라우저와의 차이

아래에 허용한다고 적지 않은 연산과 구조는 지원 계약 밖이다. 한 변경에 여러 wire 연산이 있으면 모두 처리할 수 있어야 한다. 단순 문서에서도 일반 편집기가 부가 구조를 넣으면 이 후보에서 열리지 않을 수 있다.

| 형식           | 재생·선택 취소                                                                                                                                                                                                                     | 브라우저와 다른 점                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| -------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| DOCX, 스키마 4 | 기본 본문의 한 문단 안에서 표시 없는 텍스트 하나를 삽입·삭제·교체하는 단일 ProseMirror `ReplaceStep`. 기존 단계 실행과 위치 매핑으로 후속 타이핑을 보존한다. 대상 안을 다시 편집했거나 삭제된 자리에 타이핑하면 충돌한다.          | `steps`의 다른 단계, 복합 단계, 문단을 넘는 편집, 서식, 표·목록, `side` 전부(주석·각주·구역 등) 미지원. 본문·일부 속성 외 패키지, 스타일 등은 거부한다. [브라우저 어댑터](../../apps/docs/src/web/collab-adapter.ts)                                                                                                                                                                                                                                                               |
| XLSX, 스키마 3 | `sheet.mutation.set-range-values` 하나가 `sheet-<sheetId>`의 A1:Z1000 안 한 칸에 명확한 숫자·문자열을 쓴다. 빈칸으로의 보상도 재생한다. 같은 탭·주소에 이후 쓰기가 있으면 충돌한다. 기존 wire 검사와 `planCellEditsToXlsx`를 쓴다. | 다중 칸·수식·형식·구조 변경·셀 이동·병합·행열 속성·탭 작업·보기·이름 정의 등 [브라우저 목록](../../apps/sheets/src/web/collab-mutations.ts)의 나머지 전부 미지원. 원본은 제한된 워크시트 XML과 기본 스타일만 허용한다. 빈칸 지우기와 값이 같은 쓰기는 취소 대상으로 삼지 않는다. 브라우저가 표시값만으로 복구하지 않는 일부 문자열도 여기서는 원본 XML 타입으로 복구한다.                                                                                                          |
| PPTX, 스키마 3 | 브라우저 `decodeOp`의 `text` 중 한 슬라이드·한 도형·한 문단·한 run의 비어 있지 않은 단순 본문만 교체·복원한다. 도형 숫자 ID와 creationId 별칭으로 같은 대상을 확인한다. 같은 도형에 이후 쓰기가 있으면 충돌한다.                   | `notes`, `blankSlide`, `duplicateSlide`, `deleteSlides` 전부와 복합 텍스트·그룹·자리표시자·자동 맞춤·새 언어 속성이 필요한 편집 미지원. 브라우저의 선택 취소는 생성된 슬라이드를 지우는 방식이다. 후보의 도형 텍스트 취소는 다른 지원 범위이며, 슬라이드 생성 취소는 지원하지 않는다. 도형 본문은 OOXML에 직접 기록한다.                                                                                                                                                           |
| PDF, 스키마 2  | `puts` 중 `rotations`만 처리한다. 원본 회전에 대한 분기 상태와 `was`를 확인하고 pdf-lib로 페이지 회전을 기록한다. 해당 페이지에 이후 쓰기가 있으면 충돌하고 다른 페이지 편집은 유지한다.                                           | `blankPages`, `markups`, `noteEdits`, `drawings`, `formFields`, `formEdits`, `formWidgetEdits`, `deleted`, `order`, `stampCfg`, `metadata` 및 그 밖의 버킷 미지원. 브라우저도 `annotDeletes`, `textEdits`, `textInserts`, `imageEdits`는 거부한다. 브라우저의 마크업·메타데이터·도장 보상은 지원하지 않는다. 원본은 PDF 1.0~1.7의 고전 xref, 평평한 페이지 트리, 빈 리소스와 제한된 무압축 사각형 본문·Info만 허용한다. 글꼴·이미지·주석·폼·암호화·증분 갱신·압축 스트림은 미지원. |

## 실행 경계와 한도

Node는 형식과 무관하게 바이트 길이만 검사해 파이프로 전달한다. 호출마다 패키지에 고정된 Deno 2.9.6을 시작한다. 게스트가 UTF-8, JSON, ZIP, XML, PDF를 처음 해석하며, 동일한 게스트에서 `replayHistory`와 형식별 저장까지 실행한다. 브라우저, DOM, 수신 포트, 외부 서비스는 사용하지 않는다. 결과 메타데이터와 파일만 Node로 돌아온다. 원본 ZIP의 압축 방식이나 바이트 배열 전체가 유지된다는 보장은 없다.

- 게스트에는 읽기·쓰기·네트워크·환경 변수·자식 프로세스·FFI·시스템 조회·원격 import 권한이 없다. 런타임은 설치된 고정 번들만 시작하며, 권한 요청·원격 모듈·npm·설정 탐색을 끈다. 런타임이 쓰는 임시 디렉터리는 호스트가 회수한다. 전역 `deno`나 다른 엔진으로 대체하지 않는다. 권한 정책은 [Deno 런타임](https://docs.deno.com/runtime/fundamentals/security/)을 신뢰한다. OS 가상 머신이나 전체 RSS 할당량은 아니다.
- 원본·결과 각 2 MiB, 요청 JSON 1 MiB, 변경 128개, 연산 256개, JSON 노드 16,384개, 깊이 32, 키·문자열 합계 256 KiB. `replayHistory`가 epoch·요청 버전·연속 버전·중복 제출·SHA-256·보상 크기를 추가 확인한다.
- OOXML은 단일 classic ZIP의 STORE/DEFLATE만 허용한다. ZIP 항목 256개, 항목당 해제 1 MiB, 전체 해제 4 MiB. 중앙·로컬 헤더, 이름·영역 중복, 실제 해제량, CRC를 확인한다. ZIP64·암호화·서명·추가 필드·주석은 받지 않는다. PDF 프로필은 1 MiB, 128페이지, 260객체 이하이다.
- 호출당 제한 시간 15초, V8 old space 96 MiB, 표준 출력·오류 각각 약 3 MiB. 동시 호출 수·전체 메모리·사용자별 할당량은 호스트가 제한해야 한다. 정상 결과 또한 기존 서비스의 사용자·문서 권한 검사를 대신하지 않는다.

## 설치와 확인

Node 22.12 이상, Linux ARM64 glibc 또는 macOS ARM64가 필요하다. Deno 바이너리는 해당 플랫폼의 `@deno/linux-arm64-glibc@2.9.6` 또는 `@deno/darwin-arm64@2.9.6` 선택 의존성으로 공급한다. 설치할 때 플랫폼용 바이너리와 잠금 파일의 전체 의존성을 캐시에 준비한다. `--omit=optional`은 사용할 수 없다. Ubuntu에서 glibc용을 쓰며 musl·x64 배포를 제공하지 않는다. 실행 중 다운로드는 없다.

```sh
# 저장소에서 로컬 tarball 두 개 생성. OUT은 기존 절대 경로.
npm pack -w @seuoffice/collab --pack-destination "$OUT"
npm pack -w @seuoffice/history-renderer --pack-destination "$OUT"
# 별도 빈 호스트 프로젝트에서 의존성을 미리 캐시한 뒤 실행.
npm install --offline --ignore-scripts "$OUT/seuoffice-collab-0.1.0.tgz" "$OUT/seuoffice-history-renderer-0.1.0-rc.0.tgz"
node node_modules/@seuoffice/history-renderer/examples/retained-host.mjs
```

[호스트 예제](examples/retained-host.mjs)는 합성 DOCX와 독립된 두 편집을 보유하고, 첫 편집의 보상만 3번 버전에 기록한다. 같은 요청을 두 번 보내도 영수증이 같고 다시 연 문서에 두 번째 사람의 `Two?`가 남아야 한다. 예제의 메모리 저장소를 운영용으로 쓰라는 뜻은 아니다. 보유 범위, 인증·권한, 기대 epoch·버전에서의 원자적 추가, 중복 제거, 보상 저장은 기존 `createHistoryServer`와 호스트 저장소의 책임이다.

저장소 시험은 `npm run build -w @seuoffice/history-renderer` 뒤 `npm test -w @seuoffice/history-renderer`로 실행한다. `tests/*`를 독립 설치 폴더에 복사해 Vitest로 실행하면 로컬 소스 대신 포장한 공개 진입점을 검사한다. `capabilities.test.ts`는 동일한 설치본의 실행기에 시험용 게스트를 넣어 환경 자격 표식·읽을 수 있는 파일·실제로 열린 루프백 포트·프로세스 실행이 거부되는지 검사한다. 시험 게스트 경로는 공개 API 인자가 아니다.
