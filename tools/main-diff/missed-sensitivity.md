# 미검출 소비자 경계의 좁은 증명

## 범위

이 검증 정의 가지의 `seuoffice-rust.yml`은 새 제품과 별도 검증 정의 SHA를 정확히 checkout하는 수동 좁은 전용 경로다. GitHub에 이미 등록된 파일 경로를 사용하되 이 가지에서만 prepare와 좁은 matrix를 담는다. 이 정의 가지를 제품·plan·main에 합치지 않으며, 기존 정의97ff·실행43604410368의 workflow 본문은 바꾸지 않는다. manifest는 제품 `276b0d2a8ce8b04b266ecf42048b6ef10b06d4aa`에서 원본 그대로인 네 비동등 변이와 명시적으로 교체한 MCP613 후속 하나를 고른다. 전체 중심 baseline·6,121개 결함·489개 엔진 결함·전체 재개 합격이 아니다.

새 workflow 파일을 기본 가지 등록 없이 수동 실행할 수 있다고 가정하지 않는다. GitHub의 [workflow_dispatch 문서](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch)에 따라 기존 등록 경로를 쓴다. 이 ref의 명시 입력은 `product_sha`뿐이며 다른 행렬·모드 입력을 받지 않는다.

원본 실행 `43604410368`의 다섯 MISSED는 모두 그대로 보존한다. MCP613의 원본 `is_none_or`→`is_some_and`는 고정 공개 non-reentrant 이벤트 모델에서 동등하므로 이를 성공으로 만들지 않는다. 동일 index·개수·file·selector·find를 보존하고 name·replace만 바꾼 stop-before-gone 후속은 원본 해시 `063d2fd4…b373`에서 `916bf663…9c2a`로의 전체 객체 계보를 manifest와 제품 `mutation-613-lineage.json`에 기록한다. 그 후속도 실제 새 case62 typed DIFF를 통과해야 한다. 제품 후속의 원인·TS oracle은 제품의 `five-miss-followup.md`에 있다.

## 고정 선택

각 case는 원본 catalog 바이트 해시, index, 원본·제품 변이 객체 해시, control0 객체 해시, 선택 함수 전체 목록, 추가 소비자 case index와 실제 TS wire를 기록한다. 제품의 함수 registry가 제공하는 사례 수와 single 여부도 정상 baseline의 기대 목록과 같아야 한다.

길이 `L`인 원본 catalog에 `N=4*ceil(L/4)`를 쓰고 `FAULT_SHARD=원본index/N`을 원본 `faults.mjs`에 준다. `N>=L`이므로 선택되는 것은 원본 control0과 그 원본 index 두 개뿐이다. index 재번호화, catalog 복사·부분 배열, 가짜 입력은 없다. `N`이4의 배수라 기존 `%4` 계보도 유지된다.

단일 Linux mutation matrix는 최대8개 job, 각 job120분이다. 정확 다섯 target만 있어 실제 mutation job도 다섯 개다. 정규 무료 hosted runner와 GitHub queue를 쓰고 다른 실행을 취소하거나 우선순위를 강제하지 않는다. concurrency는 이 제품의 좁은 감도 전용 group, `cancel-in-progress=false`다. 기존 원본·재개 group과 다르다. 수동 입력은 정확 `product_sha` 하나이며 manifest와 다른 값은 product checkout 전 거부한다.

## 정상 비교와 실제 변이

1. 정의·제품 checkout의 정확 HEAD와 부모 제품375 대비 허용 변경 목록·전체 바이트 해시를 대조한다. 원본 mutator 바이트 해시는 `5ee0052330c9b76197beb4a4056a1bffdfc2c063f98440f608ebca94644adc5b`를 유지한다.
2. 표준 제품 WASM을 준비하고 그 실제 해시를 job identity에 남긴다. native 정상 빌드와 모든 변이는 `MAIN_DIFF_PROFILE=node-faults`, `CARGO_INCREMENTAL=1`이다. 최적화·panic·overflow 설정은 바꾸지 않는다.
3. control과 target이 고른 함수의 모든 사례를 실제 제품 registry와 원본 TS/Rust 비교기로 정상 비교한다. group loader 격리를 유지하고 실제 엔진을 시작한다. 전체 함수/사례·정확 장부·0차이·STALE0·FLOOR0·정상 종료가 필수다. 이 경로의 선택 함수에는 장부 항목이 없으며 실제 제품에서 다시 확인한다.
4. 원본 mutator로 control0과 target만 실행한다. producer가 마지막 정상 빌드까지 성공한 때만 완결 감사를 한다. `faults.json`은 원본 고정 폴더에서, 마지막 `report.json`은 실제 `MAIN_DIFF_OUT`에서 읽는다. 실패 때도 raw 파일을 보존하되 completion을 만들지 않는다.
5. control의 실제 DIFF와 CAUGHT, target의 실제 DIFF와 CAUGHT, 두 행 전체 성공 summary, 정확 이름·파일·selector·index·해시가 필수다. target report는 기대 함수·사례 수가 완전하고 모든 차이의 index·wire·함수 귀속이 맞아야 한다. 특히 새 소비자 case의 실제 typed DIFF와 manifest의 관측 TS wire가 일치해야 한다. 다른 case의 차이만으로 새 사례 감도를 대신하지 않는다.
6. 실제 종료 후 소스 해시가 committed 원본으로 복구되어야 한다. 적용 근거는 pinned 원본 runner의 find1회·파생 replace 해시·정상 종료·복구 제어흐름이다. build 직전 소스를 직접 관측했다고 주장하지 않으며 전역 hook은 없다.

MISSED·BUILD·BAD·only prefix·다른 함수·STALE·fatal·FLOOR·중복·범위 밖·누락·복구 실패는 거부한다. report에 없는 tuple이나 TS 동등을 집계로 추정하지 않는다. 정상 비교가 실패하면 변이를 시작하지 않는다. 각 artifact는 `missed-sensitivity-only`의 해당 target 증거일 뿐, 기존 실패 실행을 성공으로 바꾸지 않는다.

## 실행 상태

이 정의는 준비 중이며 hosted 감도 결과는 아직 없다. 기존 제품375/정의97ff 재개 실행과 원본 제품30/정의7594 실행은 그대로 유지한다. 부모 정적 검토와 정확 ref·범위 중복 확인 전 새 dispatch는 하지 않는다.
