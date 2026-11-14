# 중심 프로세스 결함 감도 후속

## 글로벌 PAX 메타데이터 사이의 파일 이름

원본 제품은 `30f303cde7b496bf640aadffcb0b70bf7fb2ad21`이며, 전용 실행 `43568242384`의 `b5-mcp` 3번 조각은 다음 결과를 남겼다. 이 결과는 후속 수정으로 덮지 않는다.

```text
MISSED extract a global pax header is a file  (80 checks, 80 equal, 0 differences, 0 listed, 1 functions)
```

- 원본 목록: `tools/main-diff/faults/b5-mcp.json`, 0부터 세는 위치 `203`.
- 변이 객체의 SHA256: `0544ee04bf0d2ce6697e4a18b9a8ae67f47f3113c07dc6aa9e050958f720568d`. 객체를 읽어 `JSON.stringify`한 UTF-8 바이트의 값이다.
- 원본 목록 파일의 SHA256: `a45058cef79809d51c1eda0116ccb3086bd05df0838f4757e820db0c4fb8cc25`.
- 원본 변이: `runtime_archive.rs`의 메타데이터 guard에서 `b'g'`만 뺀다. 이름·대상·find·replace·함수 목록·원본 위치는 후속에서도 바꾸지 않는다.

이 변이는 모든 입력에서 동등하지 않다. 정상 `g` 경로는 메타데이터를 읽고 다음 헤더로 넘어가며 pending `pax`, `long_name`, `long_link`를 보존한다. 변이는 일반 항목 경로로 내려가 이 상태를 소비하고 지운다. 또한 파일을 쓰는 kind guard보다 먼저 항목 이름을 검증한다.

후속 제품은 `f5065c06a52b7ebbe4100da637140da4beb67182`의 기존 80개 사례 뒤에 두 개만 더한다. 기존 사례를 지우거나 번호를 옮기지 않는다.

| 새 위치 | 입력                                                           | 소비자가 관측하는 동작                                       |
| ------- | -------------------------------------------------------------- | ------------------------------------------------------------ |
| 80      | local PAX의 `kept.txt` 경로, global PAX, 일반 파일 `short.txt` | 메타데이터를 지나도 실제 파일은 `kept.txt`로 풀린다.         |
| 81      | 이름이 `../ignored-global`인 global PAX, 정상 파일             | 추출하지 않는 메타데이터의 이름은 무시하고 정상 파일을 푼다. |

사례는 기존 `TarEntry[]`, checksum·padding·gzip 생성기와 실제 `extractRuntimeArchive`를 사용한다. 가짜 엔진이나 예외 억제를 추가하지 않는다. TS 오라클 확인과 Rust 정상 baseline·같은 원본 변이의 검출은 별개의 근거다. Rust 증명은 저장소 규칙에 따라 hosted 러너에서 수행하고 정확 제품·정의·실행을 함께 기록한다. 이 두 사례의 좁은 성공을 전체 6,121개 결함 성공으로 확대하지 않는다.

## 폐기된 preload 비교 selector

`tools/main-diff/faults/b2.json`의 원본 위치 `14`, `channel-key`는 같은 채널 문자열 변이를 유지한다. `name`, `file`, `find`, `replace`, 목록의 170개 항목과 유효한 `folder-api.PROJECT_FOLDER_CHANNELS` 한 사례는 바뀌지 않는다.

`44d208ab30dde8b4137508688ae1be4e5be13fb4`가 preload bridge를 stage 3 소유로 옮기며 Rust 구현과 비교 등록을 함께 없앴지만 추가 selector `project-folder-bridge.projectFolderBridge`를 이 항목에 남겼다. 후속 metadata 수정은 그 미등록 selector만 뺀다. TS 공개 factory의 이름 변경이나 살아 있는 비교 사례 삭제가 아니다.

- 원본 변이 객체 SHA256: `103b1b0860269e2de376d46d6eb19516d7a55053e90c078412fb45b33a1c1d2f`.
- selector 정리 뒤 객체 SHA256: `2d59dfea3027e8fbbbf0015067de5b2fb9de158ea3ebf5d77ed7fef5a9009417`.
- 해시는 위와 같이 `JSON.stringify`한 객체의 UTF-8 바이트에서 계산한다. 실제 변이의 위치·내용은 같고 선택 metadata만 다르다.

고정 원본 제품과 실행의 catalog는 바꾸지 않는다. 원본 `channel-key`가 실제 `DIFF`로 잡혔는지는 해당 원본 산출물에서 별도로 감사한다. 새 검증 계획은 모든 selector가 정확 제품의 실제 함수 목록에 등록돼 있는지 검사하며 ghost selector를 예외로 허용하지 않는다.

## 실패한 동시 ChatGPT refresh의 공유

원본 제품 `375b08bd0e1f7e8432e6514468cc9d25be01b100`, 정의 `97ffeb5ec856c6a05399eae70525ede3568057ba`, 실행 `43604410368` attempt1의 job `132476063748`은 원본 `b5-provider-chatgpt.json#42`를 MISSED로 남겼다. 65개 비교는 모두 같았다. 원본 결과와 변이 본문을 바꾸지 않는다.

- 원본 변이 객체 SHA256: `10a70b21f074fa7e861bd20ddd46ff4ace7c40e8115c108a68187f2903914b65`.
- 대상은 `chatgpt_connection.rs`의 실행 중 refresh 재사용 guard다. 원본 위치·이름·file·find·replace·selector와 catalog 84개 항목을 유지한다.
- 기존 유일한 병렬 case39는 성공한 refresh다. 공유를 제거해도 첫 실행이 새 토큰을 저장하면 뒤의 직렬 실행은 새 토큰을 재사용한다.

변이가 전 입력에서 동등한 것은 아니다. 만료 토큰의 첫 refresh가 일시 실패하면 세션이 그대로 남는다. 공유가 없으면 다른 동시 호출이 다시 refresh하여 다른 결과를 받는다.

후속 case65는 기존 65개 뒤에만 추가한다. 명시 clock `1700000000000`, 만료 세션, 세 동시 accessToken 호출에 첫 HTTP400 `invalid_request`와 다음 성공 응답을 둔다. 정상 TS 오라클은 요청 한 번과 같은 network 오류 세 개, 세션 불변을 관측했다. 제품 `5423afa6f9c59568665eb0eeedc3b514b8a843fa`의 표적 실행 `43635135096`은 같은 원본 변이의 추가 요청과 서로 다른 호출 결과를 실제 전체 wire로 확인했다.

## 인증 뒤의 ChatGPT stream 조기 종료

같은 원본 job은 `b5-provider-chatgpt.json#66`도 MISSED로 남겼다. 82개 비교는 모두 같았다.

- 원본 변이 객체 SHA256: `0d488f4225f891d4f1fd09488a50d58ee626787e779dd58e6a6cd851f50f9b2a`.
- 대상은 `chatgpt_responses.rs`의 조기 종료 오류에서 `the stream ended early`를 `the stream ended`로 바꾸는 원본 변이다.
- 기존 cut-stream case46/47의 세션 만료 시각은 `1700003600000`인데, main-diff context clock은 `1794452400000`이다. 명시 connection clock이 없어 정상 TS도 Responses 호출 전에 OAuth token 요청과 network 오류로 끝났다. stream delta는 없었다.

후속 case82는 기존 82개 뒤에만 추가한다. 명시 connection clock으로 기존 fresh 세션을 실제 유효하게 두고, 완료 event 없이 닫히는 tool-argument SSE를 소비한다. 정상 TS 오라클은 Responses 요청 한 번, delta `cut at tools`, 오류 `ChatGPT stopped while sending tool arguments: the stream ended early`를 관측했다.

기존 147개 사례·공통 clock·TS/Rust 구현·원본 catalog·판정식은 그대로다. 표적 실행 `43635135096` attempt1은 정의 `f6f8731c9b11a8518817a262f6a074bd4e14b4f2`와 제품 `5423afa6f9c59568665eb0eeedc3b514b8a843fa`에서 정상 229개 비교, 양성 대조 두 번과 원본 42·66의 실제 typed DIFF 및 복구를 전수 감사해 성공했다. 성공은 이 두 후속 감도 범위이며 원본 MISSED나 전체 batch의 성공으로 소급하지 않는다.

## 추가 소비 경계와 후속 제품

2026-11-14 20:23 UTC에 고정한 실행 `43604410368`의 완료 산출물에서 비대조 MISSED 21개, BUILD 2개와 별도의 양성 대조 실패를 분리했다. 이 snapshot에서 엄격히 인수한 것은 완결 job 118개와 고유 원본 ID 2,574개뿐이다. 미완료 조각이나 실패 job의 CAUGHT 일부를 완결 집합으로 올리지 않는다.

다음 제품은 기존 `5423afa6f9c59568665eb0eeedc3b514b8a843fa`의 두 소비 사례와 증거 보존 제품 `4b9d9b51bd92f9e05d83a72baae7b0be46be0649`를 합친 별도 가지에서 준비한다. 기존 제품·정의·실행과 batch1 보류는 바꾸지 않는다. 증거 보존 실행 `43633226850`의 성공은 정상 942개 비교, 원본 12개 record와 중간 545번 원자료 보존 범위이며 전체 복구 성공이 아니다.

| 목록                   | 원본 위치  | 놓친 소비 경계                                                           | 추가 사례                                          |
| ---------------------- | ---------- | ------------------------------------------------------------------------ | -------------------------------------------------- |
| `b5-provider-chatgpt`  | 7          | 첫 요청 뒤 다음 event turn의 요청은 서버가 닫혀 전달되지 않음            | callback 23: 같은 turn의 두 정상 요청              |
| 같은 목록              | 21         | HTTP429가 retry에 소비돼 실제 client 오류 분기에 미도달                  | refresh 23: 20초 상한보다 긴 Retry-After           |
| 같은 목록              | 63, 69     | 만료 세션 때문에 Responses의 token 상한 stop에 미도달                    | stream 83: 명시 connection clock과 실제 incomplete |
| 같은 목록              | 64, 65     | 만료 세션 때문에 텍스트 전 두 오류 종류에 미도달                         | stream 84, 85                                      |
| 같은 목록              | 67         | 만료 세션 때문에 완료 event 없는 종료에 미도달                           | 기존 후속 stream 82를 유지                         |
| 같은 목록              | 71, 75, 83 | 텍스트 뒤 구조 오류, 모델 검증, 실제 요청 instructions 순서에 미도달     | stream 86, 87과 chat 23                            |
| `b5-agent-user-skills` | 5          | Codex home의 링크와 없는 자식 조합이 없음                                | sources 5: 실제 디렉터리 링크                      |
| 같은 목록              | 19, 23     | 미종결 인용부호 앞의 plain 문자, 뒤쪽 공백만 있는 scalar가 없음          | parse 15, render 15                                |
| 같은 목록              | 38, 46     | 상위 source 없는 plugin 충돌, non-builtin과 agentOnly 목록의 교차가 없음 | pick 4, folder 11                                  |
| 같은 목록              | 75, 78     | project 조기 반환 뒤 파싱 예외, install 중 실제 이름 변경이 없음         | service 49, 50                                     |
| 같은 목록              | 83, 84     | overwrite 거부, 실제 파일 수 제한과 뒤의 같은 이름 거부가 분리되지 않음  | service 51, 52                                     |
| 같은 목록              | 109        | emptyBody가 true인 preview가 없음                                        | preview 5                                          |
| `b2-cli`               | 93         | launcher 옆 bundle 자체가 끊긴 링크인 조합이 없음                        | install과 inspect의 29                             |

`requestBatches`는 같은 event turn에 여러 실제 요청 callback을 전달하는 명시 fixture다. 기존 `requests`의 개별 turn 예약과 입력이 없을 때의 동작은 유지한다. Rust도 한 묶음의 Request event를 모두 전달한 뒤 async 작업을 poll한다. 정상 callback은 첫 요청에 HTTP200, 두 번째 요청에 HTTP404를 답한다. 응답을 가짜로 만들거나 실제 OAuth handler를 바꾸지 않는다.

Codex home의 `$links`는 해당 FnSpec의 마지막 인자로만 받는다. 양쪽 adapter가 부모 폴더와 실제 디렉터리 링크를 만든 뒤 기존 공개 함수를 호출한다. 전역 FileSpec이나 서비스 구현은 바꾸지 않는다. 링크 생성 오류는 정상 결과로 숨기지 않는다.

세 비교 group의 기존 1,205개 사례는 입력과 순서가 모두 같음을 검사했다. 제품 375 뒤에 추가된 22개 중 앞선 두 사례를 제외한 새 20개의 정상 TS 소비 결과를 별도로 확인했다. 이때 쓰인 기존 로컬 WASM의 SHA256은 `4759268735a30c7ae599f64be83c06c8d405f599bd66eb395c1d9c09834abe56`이며, 같은 엔진 소스를 가진 7363 후보에서 만든 기존 자료다. hosted 표준 WASM의 `3f4e957…2615`와 같은 바이트라고 주장하지 않는다.

TS 오라클과 사례 계보는 새 Rust 감도 성공이 아니다. 새 정상 Rust 비교, 원본 비대조 21개와 별도 교정 변이의 실제 DIFF, 원자료·복구·완전성은 정확 후속 제품과 소비 정의를 고정한 뒤 hosted에서 검증해야 한다. 원본 MISSED, BUILD, STALE와 운영 실패의 판정은 소급하지 않는다.
