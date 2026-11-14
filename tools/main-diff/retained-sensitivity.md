# 변이별 원본 보고서 보존의 표적 증명

## 실행 범위

`main-retained-producer-proof`는 새 생산자 제품과 별도 CI 정의를 정확 SHA로 고정한다. 기존 제품·정의·실행의 판정은 바꾸지 않는다. 이 정의는 전체 baseline, 5748개 재개 결함 검사 또는 옛 mutator와 자동 호환되는 정의가 아니다.

원본 `b5-agent-apps.json`의 `41/56` residue와 양성 대조 0을 그대로 선택한다. 원본 index는 `0,41,97,153,209,265,321,377,433,489,545,601`이며 재번호화하지 않는다. #545 뒤에 #601이 실행되므로, 마지막 보고서만 읽는 옛 방식과 달리 중간 #545의 실제 typed 자료가 남아야 한다.

한 Linux job이 정상 선택 함수 baseline을 먼저 검증한 뒤 원본 변이 순회를 수행한다. 정상 범위는 18개 함수·942개 case·고정 장부 3개다. profile은 `node-faults`, `CARGO_INCREMENTAL=1`이며 실제 제품 WASM을 먼저 만든다. 변이 선택, 빌드, 판정식, 양성 대조 중단, source 복구와 마지막 정상 빌드의 순서는 제품 생산자가 유지한다.

## 소비 감사

정의의 manifest는 제품과 부모 제품, 두 생산자 모듈 해시, 원본 catalog 바이트 해시와 길이, 각 원본 index·객체 해시, 실제 변경 파일 계보를 고정한다. 제품에서 읽은 값만으로 같은 제품의 신원을 승인하지 않는다.

소비 감사는 다음 자료를 결속한다.

- 정상 baseline의 정확 함수·case 수, 장부 tuple, 0 unlisted DIFF·0 STALE·0 FLOOR.
- 원본 selector별 실제 명령 순서, 독립 출력 폴더, 반환 상태와 원본 보고서 바이트 해시.
- committed source의 원본 해시, find 1회, replace 파생 해시와 생산자가 실제 읽은 적용·비교·복구 해시.
- 선택된 모든 원본 index의 양성 대조 또는 실제 비장부 DIFF, 완결 record와 최종 정상 빌드 표시.
- STALE마다 실제 report의 장부 case TS tuple이 고정 장부와 같고, Rust tuple은 고정 Rust 및 실제 TS와 다르다는 자료. report에 없는 tuple은 추정하지 않는다.

`files`는 문자열 값만 허용한다. 실제 JS 오류 진단, unknown selector, 다른 함수, 누락·중복 case, 부정확 집계, only-STALE, source 복구 실패, 증거 IO 오류는 거부한다. JSON 데이터 안의 오류 이름은 실행 진단으로 바꾸지 않는다.

loader는 manifest가 정한 고정 경로만 읽고 심볼릭 링크와 `capture-errors.json`을 거부한다. 옛 기본 report나 마지막 report로 빠진 중간 자료를 대신하지 않는다.

#545는 고정 장부 case3의 실제 반응과 별도 case21의 전체 페이지를 함께 검사한다. 독립 case21의 TS `totalChars=3`, Rust `totalChars=7` 외 파일·텍스트·offset·로그가 이미 관측한 singleton 자료와 정확히 같아야 한다.

## 결과의 한계

순수 Node 회귀는 identity·계획·자료 보존·엄격 감사 경계를 검사한다. native 감도나 시간 개선을 증명하지 않는다. hosted 증명은 검토된 정의와 제품의 정확 SHA에서 한 번 실행한 자료로 별도 판정한다.

성공 receipt의 범위는 이 residue의 변이별 원본 자료와 선택 소비 경계다. 원본 실패·MISSED·불완전 artifact를 성공으로 소급하지 않고, batch1 보류를 해제하지 않는다. 추가 쓰기 비용은 측정 전이며 속도 개선을 주장하지 않는다.
