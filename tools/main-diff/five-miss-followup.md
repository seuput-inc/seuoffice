# 재개 batch0의 다섯 미검출 분류

## 고정 원본과 판정

- 실패 실행: `43604410368` / attempt1, 정의 `97ffeb5ec856c6a05399eae70525ede3568057ba`, 제품 `375b08bd0e1f7e8432e6514468cc9d25be01b100`.
- `b4-pdf-store` residue2/4는25행 중23 CAUGHT·2 MISSED, residue0/4는25행 중24 CAUGHT·1 MISSED다. `b5-mcp` residue1/36는24행 중22 CAUGHT·2 MISSED다. 세 producer는 마지막 정상 빌드까지 끝났고 감사기가 실제 미검출을 거부했다. 이 결과를 성공으로 고치지 않는다.
- 원본 artifact는 PDF2/4 `14118029228`, PDF0/4 `14117914218`, MCP1/36 `14120326223`이다. 중간 변이 전체 report는 덮어써졌으므로 존재한다고 주장하지 않는다.
- MCP residue13/36의 종료143은 runner shutdown 로그가 있는 별도 불완전 실행이다. 시간 초과나 아래 미검출과 같은 원인으로 단정하지 않는다.

## 소비자 사례가 빠진 네 비동등 변이

| 원본 ID                | 변경                                            | 기존 누락 경계                                               | 뒤에 추가한 사례                                                                |
| ---------------------- | ----------------------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| `b4-pdf-store.json#54` | provider 기록의 시간이 `<`인 조건을 `<=`로 변경 | 같은 시각이지만 provider/model이 다른 목적지 기록            | `migrateDocument` case19: 블록 없이 같은 시각의 목적지 metadata를 보존한다      |
| `b4-pdf-store.json#70` | summary 개수 `==3`을 `>=3`으로 변경             | 네 항목 모두 문자열인 배열                                   | `asInsights` case14: 네 문자열 summary를 거부한다                               |
| `b4-pdf-store.json#96` | 다른 PID의 생존 판정을 반대로 변경              | 기존 dead-PID 잠금이 시간 경과로도 회수되어 같은 결과로 수렴 | `writeJsonAtomic` case9: 존재할 수 없는 PID와 미래 mtime으로 나이 우회를 막는다 |
| `b5-mcp.json#649`      | 완전한 절대 경로 조건을 제거                    | 상대 경로가 실제 존재하는 파일에 도달하는 경우               | `runtimes.scenario` case44: 실제 파일에 도달해도 상대 경로는 승인하지 않는다    |

기존 사례의 순서·입력·활성 함수·변이 본문은 그대로 두고 위 네 사례만 각 배열 끝에 더한다. `#96`의 정상 TS는 즉시 잠금을 회수하지만 반대 판정의 변이는 잠금 획득 시간 제한까지 기다릴 것으로 소스에서 추론한다. 로컬에서 그 Rust 변이를 실행하지 않았다.

## `b5-mcp.json#613`의 동등한 교체식

원본은 `children.find(...).is_none_or(|s| s.io.gone)`를 `is_some_and(|s| s.io.gone)`로 바꾼다. 두 식은 `Some(false)`와 `Some(true)`에서 같고 `None`에서만 다르다.

제품375의 `shell/mcp_client/local.rs::await_exit` 호출자는 `on_connect_settled`의 시작 실패와 `stop_process` 두 곳이다. 자식 slot은 connect 전에 만들어지고, transport 닫힘은 `with_client`가 SDK의 Settled·Closed 이벤트를 모두 동기 처리한 뒤 `maybe_drop_slot`에서 slot을 지운다. 시작 실패의 재진입은 그 삭제 이전이다. PID 없는 error도 gone을 먼저 세운 후 transport 종료를 처리하므로 그 경로의 await는 `Some(gone=true)`다.

실행 중 stop은 `running.take()`가 자식을 가진 때만 await에 도달한다. transport가 이미 닫힌 자식은 `on_client_closed`가 running을 비우고, 그 뒤 slot이 삭제된다. 이후 stop은 running이 없어 await에 들어가지 않는다. `ProcHost::close_transport` 자체는 slot을 삭제하지 않는다. 외부 호스트 이벤트는 한 단계의 이벤트 정착 도중 재진입하지 않는 공개 계약이다.

따라서 이 공개 호스트·이벤트 모델에서 await 진입 시 slot 존재 불변식이 유지되고 두 식은 같은 값을 낸다. 이는 고정 소스의 제어흐름 증명이며 TLC·Lean 또는 실제 변이 성공으로 표현하지 않는다. 불가능한 None 상태를 가짜 호스트에 주입해 소비자 사례를 만들지 않는다. 원본 `#613`의 MISSED는 그대로 보존하며, 같은 소비자 결정의 비동등 후속 변이를 쓰려면 원본 index·객체 해시와 새 해시의 명시 계보가 필요하다.

별도 metadata 후속은 같은 원본 index613·catalog 개수810·file·selector·find를 유지하고 name을 `local stop finishes before the child is gone`, replace를 `.is_none_or(|s| !s.io.gone)`로 바꾼다. 같은 소비자 결정에서 실제 자식이 아직 살아 있을 때 stop이 너무 일찍 끝나는 비동등 결함이다. 원본 객체 해시 `063d2fd41a158c31acd820778b11dfcae7a47c5eb6930eb915fd49802e4fb373`에서 후속 `916bf663a9cc68d4842464682a3b8f5ae698c00810ee0588850545bc3e699c2a`로의 계보는 `mutation-613-lineage.json`에 원본·후속 객체 전체와 함께 남긴다.

이를 위한 `local.scenario` case62는 정상 시작 뒤 close·타이머를 보내지 않고 stop의 pending 상태를 유한하게 관찰한다. 가짜 None 상태나 끝나지 않는 await를 만들지 않는다. 새 후속을 CAUGHT로 인수하려면 이 정상 TS pending과 실제 mutant 조기 완료의 차이를 정확 제품·정의의 hosted 표적 proof로 확인해야 한다. 원본 MISSED를 소급 성공으로 바꾸지 않는다.

## 관측한 TS oracle과 남은 검증

정확 제품 WASM을 시작한 TS probe는 실제 group 함수·fixture 준비·출력/파일 snapshot 코드를 사용하되 native Rust addon을 부르지 않았다. 모의 문서만 썼다.

- `migrateDocument` case19: `{aiReadPages:0, translationBlocks:0, languages:[]}`. 목적지 provider `destination`/model `destination-model`/savedAt5를 입력 바이트 그대로 보존했다.
- `asInsights` case14: `null`, 파일 없음.
- `writeJsonAtomic` case9: 반환 undefined, `entry.json`은 `{"new":true}`, lock·임시 파일 없음.
- `runtimes.scenario` case44: `runtime-missing/not-found`, 실제 fixture 파일은 그대로다.
- `local.scenario` case62: boot는 완료, stop `s`는 `pending`, 상태 idle. 아직 자식이 닫히지 않았다는 계약을 관찰했다.

TS probe 검사는 `20261114-162314-dc5c`, `20261114-162314-8a9b`, `20261114-162526-c1a9`가 통과했다. 이 결과는 새 정상 Rust baseline이나 원본 변이의 CAUGHT 증거가 아니다. 후속 정확 제품 SHA에서 정상 선택 함수 전체 baseline과 원본 control·대상 변이의 실제 DIFF, 파일 복구를 정식 hosted 좁은 경로로 확인해야 한다. 기존 run·제품·정의 ref는 바꾸거나 재dispatch하지 않는다.
