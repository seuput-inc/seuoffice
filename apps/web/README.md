# @seuoffice/web

seuoffice를 브라우저에서 돌리는 정적 파일 묶음이다. 패키지에는 빌드 결과인 `dist/`만 들어 있다. 호스트는 `dist/` 안의 파일을 그대로 한 주소(`baseUrl`) 아래에 서빙한다. 서버 코드는 필요 없다.

```sh
# 설치는 @seuoffice/embed README처럼 GitHub 릴리스 주소로 한다(npm 레지스트리에는 없다)
# dist/ 를 호스트의 정적 경로로 복사하거나 그대로 서빙한다. 예:
cp -r node_modules/@seuoffice/web/dist ./public/seuoffice
```

호스트 화면에서는 `@seuoffice/embed`의 `openDocument`로 문서를 연다. `baseUrl`에는 `dist/`를 서빙하는 절대 주소(예: `https://호스트/seuoffice/`)를 넘긴다. 사용법은 `@seuoffice/embed` README를 본다.

- 두 패키지는 항상 같은 판을 쓴다. 호스트는 정확한 판 번호로 고정한다.
- `dist/index.html`은 파일을 직접 열어 보는 시험용 런처다. 호스트 화면에 쓰는 것은 `dist/<앱>/` 아래 페이지(`docs`, `sheets`, `slides`, `pdf`, `hwp`)이고, `@seuoffice/embed`가 주소를 알아서 만든다.

## 압축해서 보내기

`dist/`의 파일은 압축해서 보내야 한다. 처음 여는 데 걸리는 시간은 거의 받는 바이트가 정한다. 아래는 빈 캐시에서 문서 하나를 처음 열 때 받는 양이다(2026-11-10, v0.12.18 빌드, Chromium으로 잼. brotli 값은 받은 파일을 brotli로 눌러 계산).

| 형식 | 압축 없이 | brotli |
| ---- | --------- | ------ |
| hwpx | 30.8 MB   | 9.6 MB |
| xlsx | 16.9 MB   | 4.6 MB |
| docx | 10.7 MB   | 4.5 MB |
| pptx | 9.0 MB    | 2.7 MB |
| pdf  | 5.7 MB    | 1.6 MB |

- 서버의 압축 대상에 `application/wasm`, `text/javascript`, `text/css`, `application/json`, `text/plain`, `font/ttf`, `font/otf`를 넣는다. 많은 서버의 기본 목록에는 wasm과 글꼴이 없다(nginx `gzip_types`, `vite preview`도 wasm과 ttf는 압축하지 않는다). 한글 엔진 wasm 하나가 25.6MB라서, 이것이 빠지면 표의 왼쪽 값이 된다.
- CDN이 큰 파일을 자동 압축하지 않으면(파일 크기 상한이 있는 곳이 있다) 올리기 전에 미리 압축해 `Content-Encoding`을 붙인다.
- 확인: 브라우저 개발자 도구의 네트워크 탭에서 `rhwp_bg-*.wasm`의 응답 머리글에 `content-encoding: br`(또는 `gzip`)이 있는지 본다.

## 배포판에서 XLSX 보기 속도 재기

1. 비교할 릴리스에서 `seuoffice-web-<판>.tgz`와 `seuoffice-embed-<판>.tgz`를 받아 각각 풉니다.
2. 저장소 루트에서 아래 환경 변수를 설정합니다. 통합 문서는 벤치가 만듭니다.

   | 환경 변수                     | 값                                                      |
   | ----------------------------- | ------------------------------------------------------- |
   | `SEUOFFICE_BENCH_XLSX`        | 결과를 구분할 이름. 측정 대상의 전체 커밋 SHA           |
   | `SEUOFFICE_BENCH_XLSX_DIST`   | 웹 묶음을 푼 곳의 `package/dist` 경로                   |
   | `SEUOFFICE_BENCH_XLSX_EMBED`  | 임베드 묶음을 푼 곳의 `package/dist/index.js` 절대 경로 |
   | `SEUOFFICE_BENCH_XLSX_SIZES`  | `large,xl`. 빠른 확인은 `small`                         |
   | `SEUOFFICE_BENCH_XLSX_ROUNDS` | 성능 비교는 `5`. 빠른 확인은 `1`                        |

   첫 숫자와 스크롤 속도만 재려면 `SEUOFFICE_BENCH_XLSX_TIMING_ONLY=1`을 설정합니다. 메모리 계측과 별도 시작·끝 행 캡처는 생략합니다.

3. `npx playwright test --config apps/web/playwright.config.ts bench-xlsx --project=webkit`을 실행합니다. Windows의 설치된 Edge에서는 `SEUOFFICE_BENCH_XLSX_BROWSER=msedge`를 설정하고 `--project=edge`를 사용합니다. 각 회차는 새 브라우저 컨텍스트에서 엽니다.
4. 다음 실행 전에 `apps/web/test-results`의 JSON과 스크린샷을 다른 곳에 보관합니다. Playwright가 다음 실행 때 결과 폴더를 비웁니다.

`openedMs`는 파일 선택부터 호스트가 `opened`를 받기까지입니다. `firstNumericDrawMs`는 선택부터 그리드의 첫 소수 값에 대한 `fillText`가 끝나기까지이고, `openedToNumericDrawMs`는 이 두 시각의 차입니다. `-numeric-<회차>.png`에 `firstNumericText`가 보이면 `firstNumericCaptureMs`는 그 값이 보인 시각의 상한입니다. 캡처 완료 시각은 화면에 처음 나타난 프레임의 시각이 아닙니다. `scrollGapsOver100ms`는 스크롤 시작을 기준으로 한 100ms 초과 `requestAnimationFrame` 간격입니다.

`19de229a45b1131837883382cacb84c62b716100` 후보만 잴 때는 이 SHA에서 만든 웹·임베드 묶음을 모두 지정합니다. `SEUOFFICE_BENCH_XLSX`는 이름표일 뿐 묶음을 검증하지 않습니다. `SEUOFFICE_BENCH_XLSX_FIXTURES`에는 기존 `small.xlsx`, `large.xlsx`, `xl.xlsx`가 있는 폴더를 지정합니다. 파일이 하나라도 없으면 벤치를 시작하지 마세요. `SEUOFFICE_BENCH_XLSX_SIZES=small,large,xl`이면 작은 파일의 열기 비용부터 그 순서대로 잽니다. 한 크기의 모든 회차를 마친 뒤 다음 크기로 넘어갑니다. 새 컨텍스트가 운영 체제 파일 캐시까지 비우지는 않습니다.

회차별 PNG에서 `firstNumericText`를 확인한 뒤 같은 회차의 `firstNumericCaptureMs - openedMs`를 구하면 `opened`부터 그 숫자가 보일 때까지의 상한입니다. 상한이 5000ms를 넘었다는 이유만으로 실제 첫 표시가 늦었다고 판정하지 않습니다. 선택부터의 5000ms 기준에는 `openedMs`를 빼지 않습니다.

`scrollMaxGapMs`는 스크롤 구간에 걸친 rAF 간격 전체의 최댓값입니다. 시작·끝 경계를 걸친 정지도 포함하므로 끝 경계에 걸친 간격의 `atMs`는 스크롤 길이보다 클 수 있습니다. 100ms 기준은 회차별 값을 확인합니다. rAF는 브라우저 콜백이지 모니터에 새 내용이 표시됐다는 증거는 아닙니다.

이 벤치는 배포된 파일을 별도 로컬 출처에서 서빙합니다. 운영 CDN, 호스트 앱, Safari 또는 WKWebView의 실제 표시 시각을 대신하지 않습니다.

라이선스: FSL-1.1-ALv2(소스 공개, 판마다 2년 뒤 Apache-2.0), GenOffice·이음오피스에서 온 부분은 Apache-2.0. 저장소: https://github.com/seuput-inc/seuoffice
