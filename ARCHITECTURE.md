# ARCHITECTURE — CrSDK Rust Wrapper + 테더링 서버

전체 맥락용 문서. lib(`crsdk`) + 서버(`crsdk_server`) + 웹 UI를 한눈에.
(기능 현황은 `STATUS.md` 참조.)

## 1. 큰 그림

```
[Sony A7C] ──USB(PTP)── [Mac: crsdk_server] ──HTTP/SSE/WiFi── [브라우저/폰: Tether Console UI]
```

단일 카메라를 동기 제어하는 Rust FFI 래퍼 위에, axum HTTP/SSE 서버를 얹어
브라우저/폰에서 연결·라이브뷰·노출 제어·촬영·저장을 원격으로 한다.

## 2. 워크스페이스 구조

```
crsdk_rust_wrapper/                  (Cargo workspace)
├── Cargo.toml                       [workspace] + crsdk lib(package)
├── src/                             ── crsdk lib (safe FFI 래퍼) ──
│   ├── ffi.rs            bindgen 생성 바인딩 (pub(crate))
│   ├── error.rs          CrErrorCode, SdkError, is_error/is_warning
│   ├── session.rs        SdkSession — SDK init/release (RAII)
│   ├── enumerate.rs      CameraEnumerator — 카메라 탐색
│   ├── callback.rs       DeviceCallback + CameraEvent (std::mpsc)
│   ├── connection.rs     Camera (RAII Drop 체인, take_events, set_save_info)
│   ├── liveview.rs       LiveViewStream — JPEG 프레임 fetch
│   ├── shutter.rs        capture (MF) / capture_af (AF S1 시퀀스)
│   ├── control.rs        제어코드(Range 파싱) 접근
│   ├── properties.rs     get/set + 속성 코드·값 상수 (Array·Range allowed)
│   └── capability.rs     Capabilities{model, supported} + probe + has (바디 능력)
├── wrapper/wrapper.{h,cpp}          C++ SDK → 순수 C ABI shim
├── build.rs                         cc(libwrapper.a) + bindgen + CrAdapter symlink + rpath
└── crsdk_server/                    ── axum HTTP/SSE 서버 (bin) ──
    ├── src/main.rs                  핸들러·AppState·어댑터
    └── web/index.html               단일 페이지 UI (B·Tether Console)
```

## 3. 레이어 (FFI 3단)

```
Sony C++ SDK (네임스페이스·vtable)
  └─ wrapper.cpp   순수 C 함수 + opaque void*  (예외/C++타입 경계 차단, std::atomic 콜백)
      └─ bindgen → ffi.rs   (unsafe extern "C")
          └─ src/*.rs   안전 Rust (RAII, Result, Send 경계)
              └─ crsdk_server   비즈니스/HTTP
```

## 4. 서버 상태 & 동시성

```rust
AppState {
    camera:     Arc<tokio::Mutex<Option<CameraCell>>>, // CameraCell = unsafe Send newtype
    save_path:  Arc<tokio::Mutex<String>>,
    events_tx:  broadcast::Sender<String>,             // JSON 직렬화 이벤트 fan-out
    last_image: Arc<Mutex<Option<String>>>,            // 마지막 PC 저장(미리보기)
    bulb_active, interval_active: Arc<AtomicBool>,      // 소프트 벌브/인터벌 진행 플래그
    lv_tx:      broadcast::Sender<Arc<Vec<u8>>>,        // LiveView 프레임 fan-out(다중 클라)
    lv_running: Arc<std::sync::Mutex<bool>>,            // LiveView 단일 프로듀서 보장 락
}
```

- **세션 'static화**: `OnceLock<SdkSession>` → `Camera<'static>`
- **SDK는 동기(blocking)** → 모든 SDK 호출은 `spawn_blocking`으로 격리 (tokio 워커 보호)
- **락 규칙**: `.await` 전에 handle만 꺼내고 락 해제 → 블로킹 작업 중 락 미보유
- **LiveView fan-out**: 카메라당 단일 프로듀서가 `lv_tx`로 broadcast → 각 `/lv`는 구독만
- **Graceful shutdown**: SIGTERM/SIGINT → 카메라 Drop 수동 실행 후, 스트리밍 연결이
  드레인 안 되므로 2s 유예 뒤 워치독이 `process::exit`(좀비 방지)

## 5. HTTP/SSE 엔드포인트

| 메서드 | 경로 | 역할 |
|--------|------|------|
| GET | `/api/status` | 연결상태·model·handle·save_path |
| POST | `/api/connect` `/disconnect` | 연결 (PriorityKey=PCRemote + set_save_info 자동) |
| GET | `/api/properties` | ISO/SS/Av/EV/WB/Drive/Metering/FileType/Focus/Save 현재값+allowed |
| GET | `/api/capabilities` | 연결 바디 model + 노출 property code 집합 (프론트 UI 큐레이션) |
| POST | `/api/property` `{code,value}` | 속성 쓰기 (Fetch-Modify-Set) |
| POST | `/api/shutter` | focus mode 감지 → MF: capture / AF: capture_af |
| POST | `/api/savepath` `{path}` | 저장 폴더 변경 |
| GET | `/events` (SSE) | DownloadComplete·PropertyChanged·Disconnected·Error |
| GET | `/lv` (MJPEG) | LiveView 영상 스트림 (단일 프로듀서→broadcast, 다중 클라이언트) |
| GET | `/web/*` | 정적 UI |

## 6. 핵심 데이터 흐름

```
연결:   POST /connect → spawn_blocking[enumerate→Connect→PriorityKey→set_save_info]
                      → CameraCell 저장 + take_events()→어댑터 spawn

이벤트: SDK 콜백스레드 →(std mpsc)→ Camera.events →take_events→ 어댑터(spawn_blocking)
                      →(JSON)→ broadcast →subscribe→ SSE → 브라우저 EventSource

라이브뷰: GET /lv → spawn_blocking[LiveViewStream 16ms fetch] →(mpsc 2)→ ReceiverStream
                  → multipart/x-mixed-replace → <img>

속성:   GET /properties(2s 폴링) + SSE PropertyChanged(즉시) → fillSelect dropdown
        change → POST /property → spawn_blocking[set] → 재폴링

셔터:   POST /shutter → spawn_blocking[focus mode read → MF capture / AF S1+capture]
        → PC저장 시 SDK 다운로드 → OnCompleteDownload → SSE → "저장됨" 토스트
```

## 6.1 소프트웨어 AF (SW-AF) — 컨트라스트 검출

A7C는 **절대 초점 위치 API가 없다**(`focus_near_far`는 부호=방향·크기=속도의 상대 너지뿐).
그래서 컨트라스트 검출 AF를 직접 구현한다. 코드: `crsdk_server/src/autofocus.rs`(순수 측정)
+ `main.rs::sw_autofocus`(스윕 오케스트레이션). 참조 알고리즘(AXIS/OpenCV의 중앙ROI
라플라시안 분산)을 **상대 MF 구동 + 사용자 지점 선택**으로 포팅.

```
POST /api/sw_autofocus {x,y,roi,step,count,fine_step,fine_count,settle_ms,frames}
  └ lv_tx 구독으로 라이브뷰 JPEG 프레임 획득(별도 LiveViewStream 안 만듦 — 단일 프로듀서 재사용)
  └ 측정: jpeg-decoder로 디코드 → (x,y) 정규화 중심 ROI → 4-이웃 라플라시안 응답의 분산
  PHASE 1 coarse: Near n/2 이동 → Far로 n 스텝 풀스윕(지점마다 측정) → 피크 인덱스
  PHASE 2 fine  : coarse best 복귀 → 작은 스텝으로 ±fine_n/2 미세 스윕 → 정점
  복귀: 백래시 보정 — 최종 접근을 한 방향(Far)으로 통일(overshoot 후 되돌림)
  진행률: 지점마다 {type:af_progress,phase,i,n,score}를 events_tx로 → /events SSE → UI
  취소: af_active(AtomicBool) — /api/sw_autofocus/cancel 가 false 저장, 루프가 감지
```

UI(`web/index.html`): "SW-AF" 버튼이 지점선택을 무장 → 라이브뷰 클릭(미회전 좌표로 변환,
탭-투-포커스와 공유) → 측정 ROI를 점선 박스로 표시 + 튜닝 슬라이더(ROI/step/count/settle).
한계: 상대 스텝이라 백래시·해상도 오차 잔존(coarse→fine로 완화). 라이브뷰+MF 필요.

## 7. RAII / 안전성 불변식

- **Drop 체인**(Camera): deactivate_callback → disconnect → release_device → destroy_callback
- **콜백 함수포인터 std::atomic** → Drop 시 SDK 백그라운드 스레드가 null 만나 no-op (UAF 방지)
- **USB 억제기**: ptpcamerad 50ms kill loop (Drop이 회수)
- **경고≠에러**: `is_warning()`로 CrWarning(0x2xxxx)을 빈 결과로 처리 (LiveView 스트림 유지)
- **Ctrl+C**: ctrlc + graceful_shutdown → Drop 보장

## 8. 속성 코드 (헤더 enum을 정확히 파싱한 값 — 순차 아님)

| 속성 | 코드 | 비고 |
|------|------|------|
| S1 (반누름) | 0x0001 | AF lock 트리거 |
| FNumber | 0x0100 | f값 = value/100 |
| ExposureBiasCompensation (EV) | 0x0101 | 부호 1/1000 EV |
| ShutterSpeed | 0x0103 | (num<<16)\|den |
| IsoSensitivity | 0x0104 | 0xFFFFFF=AUTO |
| ExposureProgramMode | 0x0105 | A7C는 물리 다이얼(읽기 전용) |
| FileType | 0x0106 | 1=JPEG 2=RAW 3=RAW+JPEG |
| WhiteBalance | 0x0108 | |
| MeteringMode | 0x010A | |
| DriveMode | 0x010E | |
| FocusMode | 0x0109 | 1=MF 2=AF-S 3=AF-C |
| FocusIndication | 0x0707 | AF 합초 결과(미사용) |
| StillImageStoreDestination | 0x0119 | 1=PC 2=SD 3=PC+SD |
| PriorityKeySettings | 0x011A | 2=PCRemote(쓰기 권한) |

## 9. 빌드 / 실행

```bash
# clang 21(시스템 Xcode)면 SDKROOT 불필요. (외장 Xcode15였을 땐 SDKROOT로 14.0 SDK 지정 필요)
cargo build -p crsdk_server
pkill -KILL ptpcamerad        # 서버 내장 억제기가 처리하지만 부팅 전 1회 권장
cargo run -p crsdk_server     # http://localhost:8080/web/index.html
```

## 9.1 동적 라이브러리 & 경로 처리 (크로스플랫폼)

Sony SDK는 **닫힌 동적 라이브러리**(정적 .lib 없음)라 양 OS 모두 런타임에 SDK 라이브러리 +
transport 플러그인(`CrAdapter`)을 찾아야 한다. 한 코드베이스에서 `build.rs`가 OS별로 분기한다.

| | macOS | Windows |
|---|---|---|
| SDK 핵심 | `libCr_Core.dylib` (+ `libmonitor_protocol*.dylib`) | `Cr_Core.dll` (+ `monitor_protocol*.dll`) |
| 링크 | `-l Cr_Core`, 헤더/`external/crsdk` | `Cr_Core.lib`(import lib) — lib crate의 `build.rs`가 emit한 `rustc-link-search/-lib`가 다운스트림 bin에 전파 |
| transport 플러그인 | `Contents/Frameworks/CrAdapter/`(NSBundle 기준) | exe 옆 `CrAdapter\`(Cr_PTP_USB/IP, libusb-1.0, libssh2) |
| 라이브러리 탐색 | **rpath** — dev는 `build.rs`가 SDK dir를 절대 rpath로, 배포는 `make_app.sh`가 dylib을 `Contents/Frameworks`에 동봉 + `@executable_path/../Frameworks` rpath, dev 절대 rpath 제거 | **rpath 없음** — DLL은 exe와 같은 폴더/`PATH`. `build.rs`(`#[cfg(windows)]`)가 OUT_DIR에서 `target\<profile>` 파생해 모든 SDK DLL + `CrAdapter\`를 거기로 복사 |
| CrAdapter 연결 | `build.rs`(`#[cfg(unix)]`)가 `target/<profile>/Contents/Frameworks/CrAdapter` 심링크 | 위 복사로 처리 |
| **VC++ 런타임** | (불필요) | exe·`Cr_Core.dll` 모두 `msvcp140.dll`/`vcruntime140.dll`/`vcruntime140_1.dll`(VC++ 2015-2022 재배포) 의존. Windows 기본 미포함 → `package-win.ps1`이 redist에서 **app-local 복사**(클린 머신 대응) |
| USB 드라이버 | 불필요(libusb 바로 열림) | **libusbK 드라이버 1회 설치 필수**(MTP 대신). 인스톨러가 자동 설치 — `docs/WINDOWS-PORT.md` §2.7 |

**저장 경로(서버=PC 폴더) UI 선택**도 크로스플랫폼: `POST /api/savepath/browse` →
macOS `osascript (choose folder)` / Windows `FolderBrowserDialog`로 서버 PC에 네이티브
폴더창을 띄워 선택 경로를 반환. (브라우저는 서버측 경로를 직접 못 고르므로 서버가 대신 띄움.)

배포물: macOS=`make_app.sh`→`TetherMoon.app`(dylib 동봉). Windows=`package-win.ps1`→
`dist\TetherMoon-win-x64.zip`(DLL+VC런타임+CrAdapter+web+driver) 또는 `installer.iss`→
`TetherMoon-setup.exe`(드라이버 자동설치). 양쪽 다 Sony 바이너리 동봉이라 공개 재배포는 라이선스 확인 필요.

## 10. 알려진 부채 / 다음 후보

- LiveView 다중 클라이언트 지원(단일 프로듀서→broadcast fan-out). 해상도 변경 시 버퍼 재할당 미처리. 시청자-0 시 프로듀서 미종료(연결 중 상시 가동)
- AF 셔터는 시간 기반(500ms) — FocusIndication(0x0707)으로 합초 확인 가능
- ExposureProgramMode(PASM)는 A7C 물리 다이얼이라 SDK 쓰기 불가 (allowed 비어 비활성)
- AF 좌표 보정(`AfCalib`/`af_calib(model)`)은 모델별 키화 — A7C=실측표(FocusArea=M 기준), 미측정 바디=선형 폴백. 세션 간 ~5% 변동 가능
- 바디 추상화: `capability.rs`(Capabilities/probe/has) + `/api/capabilities` + 프론트 큐레이션 완료(step 1~4). step 5(소프트 폴백 일원화)는 네이티브 UI 생길 때 보류
- 다음 후보: 100% 확대 초점확인, 촬영 히스토리/필름스트립, WiFi/SSH
- (기능 현황 전체는 `STATUS.md`)
