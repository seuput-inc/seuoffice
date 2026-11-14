# 미검출 소비자 경계의 좁은 증명

## 범위

이 검증 정의 가지의 `seuoffice-rust.yml`은 제품 `5423afa6f9c59568665eb0eeedc3b514b8a843fa`와 별도 정의 SHA를 정확히 checkout하는 수동 전용 경로다. 명시 모드는 `main-chatgpt-consumer-sensitivity`다. 이 CI 가지를 제품·plan·main에 합치지 않는다. 원본 `b5-provider-chatgpt.json`의 index42·66만 고른다. 제품4b9·정의1e12의 원자료 보존 증명, 기존 singleton·다섯 표적·아홉 장부 표적 및 원본·재개 실행은 불변이다. 전체 중심 baseline·6,121개 결함·489개 엔진 결함·전체 재개 합격이 아니다.

새 workflow 파일을 기본 가지 등록 없이 수동 실행할 수 있다고 가정하지 않는다. GitHub의 [workflow_dispatch 문서](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#workflow_dispatch)에 따라 기존 등록 경로를 쓴다. 이 ref의 명시 입력은 `product_sha`뿐이며 다른 행렬·모드 입력을 받지 않는다.

원본 실행43604410368의 두 MISSED는 그대로 남긴다. #42는 실패하는 동시 refresh의 공유를 구별하는 새 case65, #66은 만료 fixture의 선행 인증을 피하고 실제 Responses stream 종료를 소비하는 새 case82를 검사한다. 기존 전체 registry와 모든 case 앞부분이 제품375의 실제 runtime 자료와 같고, 두 목표 함수의 기존147개 뒤에만 하나씩 추가됐음을 별도로 확인했다. 원본 catalog84개·index·변이 객체·공통 clock·TS/Rust 구현은 바꾸지 않는다.

## 고정 선택

각 case는 원본 catalog 바이트 해시, index, 원본·제품 변이 객체 해시, control0 객체 해시, 선택 함수 전체 목록, 추가 소비자 case index와 실제 TS wire를 기록한다. 제품의 함수 registry가 제공하는 사례 수와 single 여부도 정상 baseline의 기대 목록과 같아야 한다.

길이 `L`인 원본 catalog에 `N=4*ceil(L/4)`를 쓰고 `FAULT_SHARD=원본index/N`을 원본 `faults.mjs`에 준다. `N>=L`이므로 선택되는 것은 원본 control0과 그 원본 index 두 개뿐이다. index 재번호화, catalog 복사·부분 배열, 가짜 입력은 없다. `N`이4의 배수라 기존 `%4` 계보도 유지된다.

단일 Linux matrix의 설정 상한은8개 job, 각 job120분이며 실제 target은2개다. 각 원본 index는 한 번만 실행하고 positive control0만 job마다 반복한다. 정규 무료 hosted runner와 GitHub queue를 쓰며 다른 실행을 취소하거나 우선순위를 강제하지 않는다. 제품별 전용 concurrency는 `cancel-in-progress=false`이고 기존 원자료 보존·singleton·다섯 표적·아홉 표적·원본·재개 group과 다르다. 수동 입력은 정확 `product_sha` 하나이며 manifest와 다른 값은 product checkout 전 거부한다.

## 정상 비교와 실제 변이

1. 정의·제품 checkout의 정확 HEAD와 부모 제품375 대비 허용 변경 목록·전체 바이트 해시를 대조한다. 원본 mutator 바이트 해시는 `5ee0052330c9b76197beb4a4056a1bffdfc2c063f98440f608ebca94644adc5b`를 유지한다.
2. 표준 제품 WASM을 준비하고 그 실제 해시를 job identity에 남긴다. native 정상 빌드와 모든 변이는 `MAIN_DIFF_PROFILE=node-faults`, `CARGO_INCREMENTAL=1`이다. 최적화·panic·overflow 설정은 바꾸지 않는다.
3. 각 control과 target의 모든 사례를 실제 제품 registry와 원본 TS/Rust 비교기로 정상 비교한다. control의40개 사례를 두 번 실행하고 목표 함수의66개·83개를 더해 총229회다. group loader 격리를 유지하고 실제 엔진을 시작한다. 선택된 다섯 함수에는 장부가 없으며 장부 밖0차이·STALE0·FLOOR0·정상 종료가 필수다.
4. 원본 mutator로 control0과 target만 실행한다. producer가 마지막 정상 빌드까지 성공한 때만 완결 감사를 한다. `faults.json`은 원본 고정 폴더에서, 마지막 `report.json`은 실제 `MAIN_DIFF_OUT`에서 읽는다. 실패 때도 raw 파일을 보존하되 completion을 만들지 않는다.
5. control과 target의 실제 DIFF·CAUGHT, 두 행 전체 성공 summary, 정확 이름·파일·selector·index·해시가 필수다. target report는 기대 함수·사례 수와 모든 차이의 귀속이 완전해야 한다. 추가 case의 TS wire는 실제 측정값과 같아야 하고, Rust wire는 manifest의 명시 `expectedRust` 전체 응답·로그·파일 바이트와 같아야 한다. #42의 기대값은 정상 TS의 순차 실패·재시도 관측을 공유가 없는 원본 Rust 직렬 큐의 병렬 결과 모양으로만 바꿔 도출했다. #66은 정상 TS 오류의 원본 replace 문자열만 바꾼다. 둘 다 실제 Rust 변이 관측 전의 검사 조건이다.
6. 실제 종료 후 소스 해시가 committed 원본으로 복구되어야 한다. 적용 근거는 pinned 원본 runner의 find1회·파생 replace 해시·정상 종료·복구 제어흐름이다. build 직전 소스를 직접 관측했다고 주장하지 않으며 전역 hook은 없다.

MISSED·BUILD·BAD·only prefix·다른 함수·fatal·FLOOR·중복·범위 밖·누락·복구 실패·STALE는 거부한다. 새 scope는 장부0과 note 부재, 전체 `expectedRust` 일치를 요구하며 ID별 verdict 예외가 없다. 기존 five·singleton·suite의 mode·장부·note·typed 조건은 유지한다. 이 결과를 원본 MISSED나 전체 회복 결과로 바꾸지 않는다.

## 실행 상태

이 정의는 준비 중이며 두 표적의 새 hosted 실행은 없다. 제품5423의 기존 runtime case 보존·추가 두 TS 오라클·서식·타입·lint를 확인했고, 정상 TS 순차 재시도도 별도로 관측했다. 원본0·42·66의 객체 해시와 mutator 해시는 고정했다. 정의의 회귀·라우팅·정확 SHA 검증과 부모 정적 검토 전 dispatch하지 않는다. 실제 Rust 정상 동등과 두 원본 변이의 감도는 아직 미검증이다.
