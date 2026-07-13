# 설계 — detector 크로스플랫폼 (추적AF를 Windows/Linux에서)

상태: **설계만** (미구현). 실측이 Windows/Linux 머신을 요구해 지금은 검증 불가 → 착수 전 이 문서로 합의만 굳혀둔다.

## 문제

추적 AF의 객체검출 `detector` 크레이트는 **CoreML + Obj-C++ 전용**이다:

- `detector/native/detector.mm` — Obj-C++로 CoreML `MLModel`을 로드/추론
- `detector/build.rs` — `cc`로 `.mm` 컴파일, `-framework CoreML/Vision` 링크
- `--features detector`는 **macOS에서만** 빌드된다

즉 Windows/Linux 사용자는 추적 AF를 못 쓴다. 배포 대상 다양화의 마지막 걸림돌.

## 이미 좋은 것 — 추상화 경계

`detector/src/lib.rs`의 공개 API는 **백엔드 무관**하다:

```rust
pub struct Detection { pub class: i32, pub score: f32, pub bbox: [f32; 4] } // 픽셀좌표 x0y0x1y1
pub struct Detector { /* opaque */ }
impl Detector {
    pub fn new(model_path: &str) -> Option<Detector>;
    pub fn infer(&self, rgb: &[u8], w: i32, h: i32, score_thresh: f32, max_n: usize) -> Vec<Detection>;
}
// Send + Sync (서버가 Arc로 공유)
```

CoreML은 전부 이 API 뒤 C ABI에 숨어 있다. 서버(`swaf::detect`, `state::load_detector`)는 이
Rust API만 호출한다. **따라서 백엔드 교체는 lib 내부 문제이고, 서버·프론트는 손대지 않는다.**

## 설계 — 백엔드 선택

같은 `Detector`/`Detection` API 아래 **cfg로 백엔드를 고른다**. 두 후보:

```
detector/src/
  lib.rs              공개 API (Detection, Detector) — 백엔드 무관 유지
  backend_coreml.rs   #[cfg(target_os="macos")]  기존 .mm/C ABI 래핑
  backend_ort.rs      #[cfg(not(macos))]         ONNX Runtime (ort 크레이트)
```

`Detector::new`/`infer`는 활성 백엔드로 위임한다. 컴파일 타임 분기라 런타임 오버헤드 0.

### 백엔드 B: ONNX Runtime (Windows/Linux, 그리고 macOS 폴백 가능)

- **크레이트**: [`ort`](https://crates.io/crates/ort) (ONNX Runtime 바인딩). 순수 Rust API, ORT 동적/정적 링크.
- **실행 공급자(EP)**: 기본 CPU. 가속은 선택 — Windows는 **DirectML**(범용 GPU), NVIDIA는 CUDA/TensorRT, macOS는 CoreML EP. v1은 **CPU만**(범용·검증 단순), 가속은 feature로 뒤에.
- **모델**: RT-DETR을 **ONNX로 export**해 동봉/다운로드. CoreML `.mlpackage`와 별도 자산.
  - 전처리(리사이즈 640², 정규화)와 후처리(박스 디코드·NMS)를 어디서 하나가 관건 → 아래 "정합" 참조.

### 전처리/후처리 정합 (제일 큰 리스크)

CoreML 모델은 리사이즈/정규화/디코드가 **모델 그래프 안**에 들어가 있을 수 있다(현재 `.mm`가
raw RGB를 받아 640²로 내부 리사이즈). ONNX export는 이게 **모델 밖**일 수 있음. 두 백엔드가
`infer(rgb,w,h)` → 같은 픽셀좌표 `Detection`을 내도록 맞춰야 한다:

- ONNX 백엔드가 Rust에서 **리사이즈(letterbox) + 정규화 + 좌표 역스케일**을 담당(현재 .mm가 하는 것과 동일 규약).
- 출력 텐서 형태(RT-DETR: `[N, num_queries, 4+num_classes]` 또는 분리 출력)에 맞춰 박스 디코드 + score_thresh + top-max_n. RT-DETR은 NMS-free지만 export 변형에 따라 필요할 수 있음.
- **검증 기준**: 같은 입력 프레임에 대해 두 백엔드의 상위 검출 박스가 ~픽셀 단위로 일치.

## 빌드/피처 전략

```
--features detector           → 플랫폼 기본 백엔드
    macOS:            CoreML (.mm, 기존)
    Windows/Linux:    ort (ONNX, CPU)
```

- `detector/Cargo.toml`: `ort`를 `#[cfg(not(macos))]` 의존으로. `cc`/CoreML은 macOS 전용 유지.
- `crsdk_server` `--features detector`는 그대로. 백엔드는 detector 크레이트가 타깃 OS로 자동 선택.
- 모델 자산: macOS=`.mlpackage`, 그 외=`.onnx`. `state::load_detector`가 존재하는 쪽 로드
  (env `TETHERMOON_DETECTOR_MODEL`). 리포 미포함(현행 유지).

## 단계

1. `ort` CPU 백엔드로 RT-DETR ONNX 로드+추론, **CLI 예제**에서 한 프레임 검출 출력 (Windows/Linux)
2. 전처리/후처리를 .mm 규약과 정합 — mac CoreML vs ort 출력 박스 비교(같은 프레임)
3. `backend_coreml`/`backend_ort` cfg 분리, 공개 API 불변
4. 서버 `--features detector`가 비-macOS에서 빌드+추적AF 동작 확인
5. 패키징(`package-win.ps1`)에 ONNX 모델/ORT 런타임 DLL 동봉

## 열린 질문 (착수 전 결정)

- **모델 배포**: 리포 미포함 유지 + 릴리즈 자산으로? 아니면 첫 실행 시 다운로드?
- **ORT 링크**: 동적(DLL 동봉, 가벼운 빌드) vs 정적(`ort` load-dynamic 대신 빌드타임). Windows 배포엔 동적+DLL 동봉이 단순.
- **가속 EP**: v1 CPU 확정. DirectML/CUDA는 실측 후 별도 feature.
- **RT-DETR export 소스**: 기존 CoreML 변환 레시피(메모리 `rt-detr-coreml-af`)에서 ONNX 분기가 있나, 아니면 원본 PyTorch/PaddleDetection에서 재export.

## 검증 제약

전처리 정합·추적AF 동작은 **Windows/Linux 머신 + ONNX 모델**이 있어야 실측된다. 그전까지는:
CLI 예제의 단일프레임 검출 + mac 대조로 논리 검증까지만 가능. 하드웨어-인-루프는 대상 OS 확보 시.

---
참고: 백엔드 무관 API는 이미 `detector/src/lib.rs`에 있음. capability/BodyProfile(바디 축)과
직교 — 이건 OS 축. 둘이 합쳐지면 "이 바디+이 OS에서 추적AF 가능?" 질의가 프로필+백엔드로 결정됨.
