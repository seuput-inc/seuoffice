# 원본 변이별 비교 자료 보존

`MAIN_DIFF_MUTATION_EVIDENCE_DIR`가 없으면 기존 변이 실행 경로와 보고서 출력 위치를 그대로 쓴다. 값을 주면 새로 만들 절대 경로여야 한다. 기존 폴더에 덧쓰거나 자동 재개하지 않는다.

## 신원과 입력

옵션을 켠 실행은 `PRODUCT_SHA`, `DEFINITION_SHA`, `GITHUB_RUN_ID`, `GITHUB_RUN_ATTEMPT`를 요구한다. 제품 SHA는 실제 checkout의 git HEAD와 같아야 한다. 생산자와 보존 helper의 바이트 해시, profile과 incremental 설정, catalog 경로와 원본 바이트 해시·항목 수, 원본 index·변이 객체 해시를 기록한다. 전체 환경 변수는 저장하지 않는다.

현재 정식 검증은 `MAIN_DIFF_PROFILE=node-faults`, `CARGO_INCREMENTAL=1`을 쓴다. 이 옵션은 빌드 profile이나 원본 selector·index·판정식을 바꾸는 옵션이 아니다.

## 남기는 파일

- `run.json`: 제품·정의·실행·생산자 신원과 원본 shard 선택.
- `catalogs/<ordinal>.json`: 원본 경로·바이트 해시·변이 항목 수.
- `mutations/<ordinal>-<index>/start.json`: 원본 객체와 source의 원본·파생 해시 및 find 횟수.
- `applied.json`: 원래 replace 직후, build 전에 실제 파일에서 읽은 적용 source 해시.
- `groups/<position>-report.raw.json`: 해당 비교 자식이 만든 JSON의 원본 바이트.
- `groups/<position>-command.json`: 실제 group·함수·명령·상태·전체 stdout/stderr·보고서와 source 해시.
- `end.json`: 기존 finally 복구 뒤의 실제 source 해시와 기존 verdict·detail, 위 파일의 해시 결속.
- `producer-finished.json`: 기존 마지막 정상 addon build 뒤의 전체 record·결과 해시. 성공 판정 표시가 아니다.
- `capture-errors.json`: 보존 오류가 생긴 경우의 별도 운영 오류.

각 비교 자식에만 원본 index/group별 고유한 절대 `MAIN_DIFF_OUT`을 준다. 자식 반환 직후, 다음 group 전에 원본 보고서를 보존한다. 옛 기본 폴더의 report를 대신 읽지 않는다. 공개된 증거 파일은 `wx`로 한 번만 쓰며, 뒤의 변이가 앞의 자료를 덮어쓰지 않는다.

## 판정과 한계

기존 case/group 선택, build, verdict·detail 계산, source finally, 양성 대조 중단과 마지막 정상 build 순서는 유지한다. 보존 파일 쓰기 오류는 기존 BUILD 판정으로 바꾸지 않고 별도로 모은다. 비교 출력 폴더 준비에 실패하면 그 자식을 실행하지 않고 현재 source를 복구한 뒤 변이 순회를 끝낸다. 공유 출력 위치로 대신 실행하거나 가짜 verdict를 만들지 않는다. 마지막 정상 build 뒤 보존 오류가 있으면 명시적인 비정상 종료다. 오류나 복구 실패로 완결 파일이 없으면 미완료다.

보존됐다는 사실은 CAUGHT, 감도, 전체 실행 성공을 뜻하지 않는다. 소비자는 실제 정상 baseline과 장부의 정확 tuple 재현, 원본 ID·객체·source·명령·원본 보고서·복구·전체 선택 완전성을 따로 검사해야 한다. STALE는 실제 typed tuple 변화와 별도의 비장부 DIFF가 없으면 계속 거부한다.

생산자 바이트가 달라지므로 기존 원본 mutator 해시 고정 감사와 자동 호환되지 않는다. 새 `per-mutation-producer-evidence/v1` 정의가 정확 새 제품과 생산자 해시를 소비한다. 기존 제품·정의·실행·거부 결과를 소급 바꾸지 않는다. 적용 source 해시는 build 직전 파일 관측이며 rustc가 내부에서 읽은 모든 자료를 추적했다는 뜻이 아니다.

단위 시험은 원본 자료의 보존·덮어쓰기 방지·누락·손상·운영 오류 경로를 확인한다. 실제 native 변이 감도와 추가 쓰기 비용은 별도의 정확 제품·정의 hosted 증명 범위다.
