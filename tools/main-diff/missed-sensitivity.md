# 미검출 소비자 경계의 좁은 증명

## 범위

이 검증 정의 가지의 `seuoffice-rust.yml`은 제품 `8cfb4bf91bcf958d12b8899f3e1b31c6ad9d6961`과 별도 정의 SHA를 정확히 checkout하는 수동 전용 경로다. 명시 모드는 `main-ledger-consumer-suite`다. 이 CI 가지를 제품·plan·main에 합치지 않는다. 원본 `b5-agent-apps.json`의 index360·379·380·381·467·468·469·547·548만 고른다. 기존 정의499의 singleton과 그 실행43621673462, 정의77의 다섯 표적 및 정의97의 재개 실행은 불변이다. 전체 중심 baseline·6,121개 결함·489개 엔진 결함·전체 재개 합격이 아니다.

새 workflow 파일을 기본 가지 등록 없이 수동 실행할 수 있다고 가정하지 않는다. GitHub의 [workflow_dispatch 문서](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch)에 따라 기존 등록 경로를 쓴다. 이 ref의 명시 입력은 `product_sha`뿐이며 다른 행렬·모드 입력을 받지 않는다.

원본 실행43604410368의 아홉 행은 장부 반응의 typed 원자료가 부족해 거부된 그대로 남긴다. 소비자 검사는 guard를 지운360의 기존 unsaved-file case6, UTF-8 길이로 바꾼379·467의 새 전체 페이지 case21, 반환 offset을 지운380·468과 slice 시작을 바꾼381·469·547 및 끝을 없앤548의 기존 ASCII case0이다. 원본 catalog·index·변이 객체·장부·제품 소스는 바꾸지 않는다. 표적545는 이 suite에 넣지 않는다.

## 고정 선택

각 case는 원본 catalog 바이트 해시, index, 원본·제품 변이 객체 해시, control0 객체 해시, 선택 함수 전체 목록, 추가 소비자 case index와 실제 TS wire를 기록한다. 제품의 함수 registry가 제공하는 사례 수와 single 여부도 정상 baseline의 기대 목록과 같아야 한다.

길이 `L`인 원본 catalog에 `N=4*ceil(L/4)`를 쓰고 `FAULT_SHARD=원본index/N`을 원본 `faults.mjs`에 준다. `N>=L`이므로 선택되는 것은 원본 control0과 그 원본 index 두 개뿐이다. index 재번호화, catalog 복사·부분 배열, 가짜 입력은 없다. `N`이4의 배수라 기존 `%4` 계보도 유지된다.

단일 Linux matrix의 설정 상한은8개 job, 각 job120분이며 실제 target은9개다. 각 원본 index는 한 번만 실행하고 positive control0만 job마다 반복한다. 정규 무료 hosted runner와 GitHub queue를 쓰며 다른 실행을 취소하거나 우선순위를 강제하지 않는다. 제품별 suite 전용 concurrency는 `cancel-in-progress=false`이고 기존 singleton·다섯 표적·원본·재개 group과 다르다. 수동 입력은 정확 `product_sha` 하나이며 manifest와 다른 값은 product checkout 전 거부한다.

## 정상 비교와 실제 변이

1. 정의·제품 checkout의 정확 HEAD와 부모 제품375 대비 허용 변경 목록·전체 바이트 해시를 대조한다. 원본 mutator 바이트 해시는 `5ee0052330c9b76197beb4a4056a1bffdfc2c063f98440f608ebca94644adc5b`를 유지한다.
2. 표준 제품 WASM을 준비하고 그 실제 해시를 job identity에 남긴다. native 정상 빌드와 모든 변이는 `MAIN_DIFF_PROFILE=node-faults`, `CARGO_INCREMENTAL=1`이다. 최적화·panic·overflow 설정은 바꾸지 않는다.
3. 각 control과 target의 모든 사례를 실제 제품 registry와 원본 TS/Rust 비교기로 정상 비교한다. diagram·mindmap 표적은33건, epub 표적은31건으로 반복 control을 포함해 총291회다. group loader 격리를 유지하고 실제 엔진을 시작한다. 실제 장부 전체가 manifest와 같아야 하며 기존 case3 tuple의 정확 재현·장부 밖0차이·STALE0·FLOOR0·정상 종료가 필수다.
4. 원본 mutator로 control0과 target만 실행한다. producer가 마지막 정상 빌드까지 성공한 때만 완결 감사를 한다. `faults.json`은 원본 고정 폴더에서, 마지막 `report.json`은 실제 `MAIN_DIFF_OUT`에서 읽는다. 실패 때도 raw 파일을 보존하되 completion을 만들지 않는다.
5. control과 target의 실제 DIFF·CAUGHT, 두 행 전체 성공 summary, 정확 이름·파일·selector·index·해시가 필수다. target report는 기대 함수·사례 수와 모든 차이의 귀속이 완전해야 한다. 독립 소비자 case의 TS wire는 실제 측정값과 같아야 하고, Rust wire는 manifest의 명시 `expectedRust` 전체 응답·로그·파일과 같아야 한다. 이 기대값은 고정 원본 replace에서 예측한 검사 조건이지 실제 변이 관측값이 아니다. 다른 원인의 오류·로그·파일 차이로 이 소비자 감도를 대신하지 않는다.
6. 실제 종료 후 소스 해시가 committed 원본으로 복구되어야 한다. 적용 근거는 pinned 원본 runner의 find1회·파생 replace 해시·정상 종료·복구 제어흐름이다. build 직전 소스를 직접 관측했다고 주장하지 않으며 전역 hook은 없다.

MISSED·BUILD·BAD·only prefix·다른 함수·fatal·FLOOR·중복·범위 밖·누락·복구 실패는 거부한다. STALE를 포괄 허용하거나 ID별 verdict 예외를 만들지 않는다. 각 manifest는 고정 장부 전체와 요구 consumer case를 데이터로 선언한다. 실제 case3 typed DIFF에서 TS wire가 고정 TS와 같고 Rust wire는 고정 Rust 및 TS와 달라야 한다. raw STALE 목록은 이 typed 반응과 정확히 같아야 하며 미등록·중복·누락은 실패다. 별도 장부 밖 case0·6·21의 실제 독립 DIFF도 필수다. report에 없는 tuple이나 TS 동등을 집계로 추정하지 않는다. 기존 five 모드의 장부0·STALE 거부와 singleton의 한 표적 제약은 유지한다. suite 결과는 각각의 새 감도 자료이며 원본 거부나 전체 회복 결과를 바꾸지 않는다.

## 실행 상태

이 정의는 준비 중이며 이 아홉 표적의 새 hosted 실행은 없다. 제품8cf의 선택 TS-only 관측6건과 고정 source·원본 객체·원본%4·선택 완전성의 읽기 전용 계획을 먼저 확인했다. 새 정의의 회귀·서식·타입·라우팅·정확 SHA 검증과 부모 정적 검토 전 dispatch하지 않는다. 기존 실행과 제품·정의 ref는 그대로 유지한다.
