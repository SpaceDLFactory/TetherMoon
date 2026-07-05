# TetherMoon 🌙

![TetherMoon](gallery/sns-wide.png)

*[English README](README.md) · [日本語 README](README.ja.md)*

**Sony Camera Remote SDK**의 Rust FFI 래퍼 + 브라우저 기반 **테더링 서버**(단일 페이지 웹 UI).
폰·PC 브라우저에서 노출·포커스·촬영·라이브뷰·장노출/타임랩스를 원격 제어합니다.

> ### ⚠️ 대상 기기: Sony A7C (ILCE-7C) 전용
> **ILCE-7C 한 대로만** macOS(Apple Silicon)·USB에서 개발·검증했습니다. 다른 바디는 미검증이며,
> A7C가 노출하지 않는 기능(자이로 레벨·Creative Look·벌브 타이머·AF영역 device property 등)은
> 코드에 남아 있어도 이 바디에선 동작하지 않습니다. 멀티바디 지원은 향후 과제입니다.

## 빠른 시작 — 그냥 쓰고 싶다면

빌드 필요 없습니다. 최신 **[릴리즈](../../releases/latest)** 를 받으세요:

1. `.dmg`를 받아 열고 **TetherMoon**을 Applications로 끌어다 놓기.
2. 실행. 처음 한 번만: 앱 우클릭 → **열기** → **열기**.
3. A7C를 USB로 연결하고 **PC 원격**으로 설정 (카메라: MENU → USB →
   *USB 연결 모드* → *PC 원격*). 콘솔이 브라우저로 자동으로 열립니다.
4. 폰에서 보려면 페이지 하단에 표시된 LAN 주소를 폰 브라우저에 입력
   (폰이 같은 Wi-Fi에 있어야 함).

이 아래 내용은 **소스에서 빌드**하려는 분들을 위한 것입니다.

## 기능

- **라이브뷰** — MJPEG + 포커스 피킹, RGB 히스토그램, 3분할 그리드 토글(뷰와 함께 회전), 수동 회전
- **노출·색** — ISO·셔터·조리개·EV·화이트밸런스(+켈빈 슬라이더)·측광·드라이브·플래시모드·파일포맷·JPEG품질·Picture Profile
- **포커스** — MF Near/Far 슬라이더, 라이브뷰 클릭 AF 포인트(Y축 보정·회전 인식), AF 영역 모드(와이드/존/중앙/플렉서블 S·M·L/트래킹), 반셔터(S1) + 합초 표시
- **소프트웨어 AF** — MF 상태에서 컨트라스트 검출 AF: 라이브뷰에서 점을 찍으면 서버가 다중해상도 포커스 스윕(coarse 범위 → fine 줌인)으로 그 ROI의 라플라시안 분산 선명도를 측정, 백래시 강건 착지 + 실시간 ROI 박스·진행률. A7C처럼 절대 초점 API가 없는 바디에서도 동작
- **추적 AF** *(실험적·macOS·옵셔널)* — 옵셔널 검출 모듈(`--features detector`)이 라이브뷰에서 **RT-DETR** 객체검출을 **CoreML**(Apple Neural Engine, 준실시간)로 실행. 오버레이의 검출 박스를 선택하면 그 객체를 추적하며 centroid를 소프트웨어 AF의 포커스 ROI로 공급. 기본 빌드엔 미포함(ML 의존성 없음)
- **촬영** — 단발·연사(누르고 유지)·동영상·취소
- **다중노출** — 소프트웨어 다중노출(A7C엔 없는 기능): N장 촬영 후 서버에서 1장으로 합성 — *평균*(클래식)·*lighten*(밝은 픽셀 누적 — 라이트트레일/별)·*add*
- **미니멀 리모컨** — 미리보기 없는 모바일 페이지(케이블 릴리즈 스타일): 셔터·AF·동영상 + ISO/셔터/조리개, 라이브뷰 스트림 없음(배터리 절약)
- **장노출** — 고정 1″–30″, BULB, **소프트웨어 벌브 타이머**(1–900초)
- **타임랩스** — 소프트웨어 인터벌(장수 × 간격) + 취소
- **저장** — PC 저장(폴더·접두사), 촬영 미리보기, 배터리·남은 컷
- **멀티바디 대비** — 바디가 보고하는 capability로 컨트롤을 큐레이션(미노출 속성은 자동 숨김)
- **다중 시청자** — 라이브뷰가 단일 카메라 스트림에서 여러 브라우저로 fan-out(폰+PC 동시)
- **안정성** — 자동 재연결, graceful shutdown(카메라 세션 클린 해제), 실행 시 브라우저 자동 오픈

## 스크린샷

단일 페이지 **Tether Console** — 왼쪽은 포커스 피킹·3분할 그리드가 얹힌 라이브뷰,
오른쪽은 모든 컨트롤.

| 연결됨 | 라이브뷰 (MF 초점 이동) |
|---|---|
| ![연결 UI](gallery/ui-connected.png) | ![라이브뷰](gallery/ui-liveview.png) |

![전체 UI, 미연결](gallery/ui-disconnected.png)

## 아키텍처

```
Sony C++ SDK ──► wrapper/wrapper.{h,cpp}  (pure-C shim, SCRSDK 네임스페이스 브리지)
                     └─► build.rs (cc + bindgen) ─► src/ffi.rs
                            └─► safe Rust lib: session / enumerate / connection /
                                liveview / shutter / control / properties / callback / error
                                   └─► crsdk_server (axum/tokio) + web/{index.html, styles.css, app.js}
```

모든 SDK 호출은 `spawn_blocking`으로 격리, 카메라는 `Arc<Mutex<…>>` 뒤에 둡니다.

## 빌드

**Sony SDK는 이 저장소에 미포함**(아래 *라이선스* 참조)입니다. 직접 받아 프로젝트 루트에
`CrSDK_v2.01.00_20260203a_Mac/`로 배치하세요.

```bash
# 전제: Rust, LLVM/Clang (brew install llvm)
export DYLD_LIBRARY_PATH=$DYLD_LIBRARY_PATH:$(pwd)/CrSDK_v2.01.00_20260203a_Mac/RemoteCli/external/crsdk/

cargo run -p crsdk_server        # → http://localhost:8080/web/index.html
```

macOS의 `ptpcamerad`가 USB 카메라 접근을 방해하므로 서버가 부팅 시 억제합니다(정상 동작).

## 배포 (바이너리 .app)

Sony 라이선스는 SDK 라이브러리를 **앱 안에 동봉해** 배포하는 것을 허용합니다.
`scripts/make_app.sh`가 SDK 라이브러리를 `Contents/Frameworks/`에 담은 자급식 macOS 앱
번들(`dist/TetherMoon.app`)을 만듭니다:

```bash
./scripts/make_app.sh
```

미리 빌드된 배포본은 [Releases](../../releases)에 있습니다. 첫 실행: 우클릭 → 열기,
또는 `xattr -dr com.apple.quarantine "TetherMoon.app"`.

## 🌙 첫 작품

이 툴로 찍은 첫 사진 — ILCE-7C + FE 100-400 GM, 무보정.

![first moon](gallery/first-moon.jpg)

> © neko.kim.film (김괭필름)

## 후원

도움이 되셨다면 ☕

[![Ctee 후원](https://img.shields.io/badge/Ctee-sdlfactory-FF5A5F)](https://ctee.kr/place/sdlfactory)

## 문의

질문·버그 제보·피드백: **spacedlfactory@gmail.com**

## 라이선스

이 저장소의 소스코드는 **MIT 라이선스**([LICENSE](LICENSE)).

**Sony Camera Remote SDK는 미포함**이며 **저작권은 Sony**에 있습니다.
[Sony Developer World](https://www.sony.net/CameraRemoteSDK/)에서 받아
[라이선스 계약](https://support.d-imaging.sony.co.jp/app/sdk/licenseagreement/ja.html)에
동의해야 합니다. 본 프로젝트는 Sony와 무관한 독립·비공식 프로젝트입니다.
