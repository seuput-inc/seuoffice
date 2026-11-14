# 첨부 본문 전체 길이의 독립 검출 경계

## 원본 거부는 보존한다

제품 `375b08bd0e1f7e8432e6514468cc9d25be01b100`, 정의 `97ffeb5ec856c6a05399eae70525ede3568057ba`, 실행 `43604410368`의 `b5-agent-apps.json#13/28`은 불합격이다. 원본 index 545의 객체 해시는 `d32c9c5906fef7cdc13f38e147651c9df4f369f1c9c9b0baa2a41e13a617ef8c`다. 이 변이는 `number(length16(&page_text) as f64)`를 `number(page_text.len() as f64)`로 바꾼다.

원본 row는 장부에 등록된 `mindmap/ai-attachments.readAttachmentPage` case 3의 DIFF와 `mindmap-attachment-page-cuts-a-surrogate-pair-3`의 STALE만 남겼다. 장부 밖 독립 DIFF는 관측되지 않았고, 해당 변이의 typed 보고서는 뒤 변이로 덮였다. producer의 CAUGHT를 엄격한 성공으로 올리지 않는다.

## 추가한 소비자 사례

부모 제품은 `276b0d2a8ce8b04b266ecf42048b6ef10b06d4aa`다. 기존 `pageCases()`의 21개 사례 뒤에 전체 본문 `가😀`를 offset 0, maxChars 128로 읽는 사례 하나를 추가한다. 같은 사례 생성기를 쓰는 diagram, epub, mindmap의 `readAttachmentPage`에 각각 index 21이 추가된다. 앞의 사례, 입력, 장부, Rust 구현, 원본 변이 객체와 index는 바꾸지 않는다.

본문을 자르지 않으므로 정상 응답은 전체 본문과 UTF-16 길이 3이다. surrogate 절단이 없는 이 입력에서 원본 545는 UTF-8 바이트 길이 7을 내는 비동등 변이다. 정상 TS/Rust 동등과 실제 변이의 새 case 21 DIFF는 별도의 정확 제품과 정의로 증명해야 한다. 소스 산술만으로 hosted 성공을 주장하지 않는다.

## 좁은 증명의 경계

기존 제품 276/정의 77의 다섯 표적 실행과 기존 제품 375/정의 97 실행은 불변이다. 새 증명은 정상 선택 함수의 전체 사례와 기존 장부 tuple 재현을 먼저 검사하고, 원본 control 0과 변이 545만 원본 실행기로 실행한다. 새 case 21의 실제 장부 밖 DIFF가 필수다. 장부 case 3의 실제 typed tuple도 보존해 기존 반응과 구별한다.

장부를 지우거나 STALE를 일괄 허용하지 않는다. 새 사례만으로 원본 row를 소급 인수하거나 알려진 네 건의 typed 증거 목록에 자동 추가하지 않는다. 전체 중심 비교, 전체 회복 행렬, batch1 승인은 이 증명의 범위가 아니다. 아직 새 hosted 실행은 없다.
