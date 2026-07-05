// crsdk_server — Phase 13b/13c
//
// 제공 엔드포인트:
//   GET  /             — alive
//   GET  /api/status   — 카메라 연결 상태 JSON
//   POST /api/connect  — enumerate + connect 시도
//   POST /api/disconnect — Camera Drop (RAII)
//   POST /api/shutter  — 셔터 작동
//   /web/*             — 정적 파일 (UI)
//
// 카메라가 없는 상태에서도 서버는 정상 부팅한다.
// 시작 시 connect를 한 번 시도하지만, 실패 시 Disconnected 상태로 계속.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use std::convert::Infallible;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json, Redirect, Response,
    },
    routing::{get, post},
    Router,
};
use crsdk::{
    connection::ConnectMode, Camera, CameraEnumerator, CameraEvent, LiveViewStream, SdkError,
    SdkSession,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use tower_http::services::ServeDir;

mod autofocus; // SW-AF: 라이브뷰 프레임 선명도 측정
mod composite; // 다중노출/스태킹: JPEG N장 → 1장 합성

// ── macOS USB 간섭 억제 (ptpcamerad) ────────────────────────────────────
// launchd가 ~100ms마다 ptpcamerad를 재시작하며 USB PTP 인터페이스를 선점한다.
// 일회성 kill로는 connect 핸드셰이크(최대 10s) 윈도우를 못 버틴다.
// 50ms 주기 kill loop를 백그라운드 자식 프로세스로 돌리고, Drop이 회수한다.
// (crsdk_example의 UsbInterferenceSuppressor와 동일 — 추후 lib로 통합 가능)
// ptpcamerad는 macOS 전용 이슈. 다른 OS는 USB 선점 메커니즘이 달라(Windows: WinUSB/libusb
// 드라이버) 여기선 no-op. (cross-platform 구조화 — Windows 측 처리는 추후 결정)
#[allow(dead_code)]
struct UsbInterferenceSuppressor {
    #[cfg(target_os = "macos")]
    child: std::process::Child,
}

impl UsbInterferenceSuppressor {
    #[cfg(target_os = "macos")]
    fn start() -> Option<Self> {
        let _ = std::process::Command::new("pkill")
            .args(["-KILL", "ptpcamerad"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("launchctl")
            .args(["stop", "com.apple.ptpcamerad"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let child = std::process::Command::new("bash")
            .args([
                "-c",
                "while :; do pkill -KILL ptpcamerad 2>/dev/null; sleep 0.05; done",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        Some(Self { child })
    }

    #[cfg(not(target_os = "macos"))]
    fn start() -> Option<Self> {
        None // TODO(windows): USB 드라이버/억제가 필요한지 실측 후 결정
    }
}

#[cfg(target_os = "macos")]
impl Drop for UsbInterferenceSuppressor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── SDK 세션 'static화 ───────────────────────────────────────────────────
// Camera<'session>의 lifetime은 SdkSession을 따른다. Arc/Mutex에 담으려면
// 'static이 필요하므로 OnceLock으로 프로세스 수명만큼 살린다.
static SESSION: OnceLock<SdkSession> = OnceLock::new();

fn sdk_session() -> &'static SdkSession {
    SESSION.get_or_init(|| SdkSession::new(0).expect("SDK init"))
}

// ── Camera Send 어댑터 ──────────────────────────────────────────────────
// crsdk::Camera는 내부 DeviceCallback에 *mut c_void 를 들고 있어 기본적으로
// !Send이다. 그러나 그 포인터가 가리키는 C++ RustDeviceCallback의 모든 함수
// 슬롯은 std::atomic으로 보호되며, 객체 자체는 힙에서 절대 이동하지 않는다.
// 따라서 Camera 자체를 다른 스레드로 옮기는 것은 안전하다. crsdk lib을
// 건드리지 않기 위해 server 안에서만 newtype으로 unsafe impl Send.
struct CameraCell(Camera<'static>, String, String); // (camera, model명, lens_model)
unsafe impl Send for CameraCell {}

// ── 검출기(RT-DETR CoreML, 옵셔널) ───────────────────────────────────────
/// TETHERMOON_DETECTOR_MODEL(.mlpackage 경로) 환경변수로 1회 로드. 없거나 실패 시 None.
#[cfg(feature = "detector")]
fn load_detector() -> Option<Arc<detector::Detector>> {
    let path = std::env::var("TETHERMOON_DETECTOR_MODEL").ok()?;
    match detector::Detector::new(&path) {
        Some(d) => {
            tracing::info!("detector loaded: {path}");
            Some(Arc::new(d))
        }
        None => {
            tracing::warn!("detector model load failed: {path}");
            None
        }
    }
}

/// 현재 라이브뷰 프레임에서 RT-DETR 검출 → 박스 JSON(추적AF의 검출 소스).
/// bbox는 라이브뷰 픽셀 좌표(x0,y0,x1,y1), img_w/h와 함께 반환 → UI가 정규화해 오버레이.
#[cfg(feature = "detector")]
async fn detect(State(s): State<AppState>) -> Response {
    let Some(det) = s.detector.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "detector not loaded (set TETHERMOON_DETECTOR_MODEL)",
        )
            .into_response();
    };
    let mut rx = s.lv_tx.subscribe();
    while rx.try_recv().is_ok() {} // 최신 프레임
    let frame = match tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
        Ok(Ok(f)) => f,
        _ => return (StatusCode::PRECONDITION_REQUIRED, "live view not running").into_response(),
    };
    let res = tokio::task::spawn_blocking(
        move || -> Result<(i32, i32, Vec<detector::Detection>), String> {
            let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&frame[..]));
            let px = dec.decode().map_err(|e| format!("decode: {e}"))?;
            let info = dec.info().ok_or("no image info")?;
            if info.pixel_format != jpeg_decoder::PixelFormat::RGB24 {
                return Err(format!("lv not RGB24: {:?}", info.pixel_format));
            }
            let (w, h) = (info.width as i32, info.height as i32);
            Ok((w, h, det.infer(&px, w, h, 0.4, 50)))
        },
    )
    .await;
    match res {
        Ok(Ok((w, h, dets))) => {
            let arr: Vec<_> = dets
                .iter()
                .map(|d| serde_json::json!({"class": d.class, "score": d.score, "bbox": d.bbox}))
                .collect();
            Json(serde_json::json!({"img_w": w, "img_h": h, "detections": arr})).into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

// ── App state ──────────────────────────────────────────────────────────
#[derive(Clone)]
struct AppState {
    camera: Arc<Mutex<Option<CameraCell>>>,
    save_path: Arc<Mutex<String>>,
    events_tx: broadcast::Sender<String>, // JSON으로 직렬화된 CameraEvent fan-out
    last_image: Arc<Mutex<Option<String>>>, // 마지막 PC 저장 파일 경로 (미리보기)
    bulb_active: Arc<std::sync::atomic::AtomicBool>, // 벌브 타이머 노출 진행중 (중복 트리거 방지)
    interval_active: Arc<std::sync::atomic::AtomicBool>, // 인터벌 촬영 진행중 (단일 실행 가드, 소유자만 해제)
    interval_cancel: Arc<std::sync::atomic::AtomicBool>, // 인터벌 취소 신호 (stop이 set, 루프가 관측)
    lv_tx: broadcast::Sender<Arc<Vec<u8>>>, // LiveView 프레임 fan-out (다중 클라이언트)
    lv_running: Arc<std::sync::Mutex<bool>>, // LiveView 프로듀서 가동 여부 (시작/종료 race 방지용 락)
    af_active: Arc<std::sync::atomic::AtomicBool>, // SW-AF 스윕 진행중 (단일 실행 가드, 소유자만 해제)
    af_cancel: Arc<std::sync::atomic::AtomicBool>, // SW-AF 취소 신호 (cancel이 set, 스윕이 관측)
    af_target: Arc<std::sync::Mutex<(f64, f64, f64, f64)>>, // 추적AF 대상 ROI(cx,cy,w,h) — retarget이 갱신, 연속 루프가 매 사이클 관측
    me_active: Arc<std::sync::atomic::AtomicBool>, // 다중노출 시퀀스 진행중 (중복 트리거 방지)
    connecting: Arc<std::sync::atomic::AtomicBool>, // 연결 시도 진행중 (동시 connect 직렬화)
    #[cfg(feature = "detector")]
    detector: Option<Arc<detector::Detector>>, // RT-DETR CoreML(추적AF, 옵셔널). 모델 미로드시 None
}

// ── /api/status DTO ─────────────────────────────────────────────────────
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Status {
    Connected {
        model: String,
        handle: String,
        save_path: String,
        lens_model: String,
    },
    Disconnected,
}

// ── Handlers ────────────────────────────────────────────────────────────

// 루트(IP:포트만 입력) → 웹 UI로 리다이렉트.
async fn root() -> Redirect {
    Redirect::to("/web/index.html")
}

async fn status(State(s): State<AppState>) -> Json<Status> {
    // .await 전에 두 락을 순차로 처리 — guard 잡은 채 await 피함.
    let save_path = s.save_path.lock().await.clone();
    let guard = s.camera.lock().await;
    Json(match &*guard {
        Some(c) => Status::Connected {
            model: c.1.clone(), // connect 시 캡처한 실제 모델명
            handle: format!("0x{:08X}", c.0.device_handle()),
            save_path,
            lens_model: c.2.clone(),
        },
        None => Status::Disconnected,
    })
}

/// 연결 코어 — HTTP 핸들러와 부팅 태스크가 공유한다 (핸들러 시그니처에 결합되지 않도록).
/// Ok(()) = 연결 완료(또는 이미 연결됨), Err(msg) = 실패 사유.
async fn connect_core(s: &AppState) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    if s.camera.lock().await.is_some() {
        return Ok(()); // 이미 연결됨
    }
    // 동시 connect 방지: 오토리커넥트 루프(3s)와 수동 /connect가 겹치면 SDK에 이중 연결
    // 시도가 발생한다. 진행 중이면 스킵하고, 가드가 모든 종료 경로에서 플래그를 해제한다.
    if s.connecting.swap(true, Ordering::SeqCst) {
        return Ok(()); // 다른 연결 시도가 진행 중 — 그쪽이 완료한다.
    }
    let _cg = RunGuard(s.connecting.clone());
    // 가드 획득 직전에 다른 시도가 막 연결했을 수 있으니 재확인.
    if s.camera.lock().await.is_some() {
        return Ok(());
    }

    // 원하는 저장 경로를 blocking 진입 전에 읽어둔다 (tokio Mutex는 blocking에서 await 불가).
    let want = s.save_path.lock().await.clone();

    // 반환: (camera, 저장경로, 모델명, 렌즈모델)
    let result: anyhow::Result<(Camera<'static>, String, String, String)> =
        tokio::task::spawn_blocking(move || {
            let session = sdk_session();
            let cams = CameraEnumerator::new(session, 5)
                .map_err(|e| anyhow::anyhow!("enumerate: {:?}", e))?;
            if cams.count() == 0 {
                anyhow::bail!("no cameras detected (check USB / PC Remote mode)");
            }
            let model = cams.get(0).map(|i| i.model).unwrap_or_default();
            let cam_ptr = cams
                .camera_ptr(0)
                .map_err(|e| anyhow::anyhow!("camera_ptr: {:?}", e))?;
            let camera = Camera::connect(
                session,
                cam_ptr,
                Duration::from_secs(10),
                ConnectMode::Usb,
            )
            .map_err(|e| anyhow::anyhow!("connect: {:?}", e))?;

            // PC Remote 제어 권한 확보 — 없으면 속성 쓰기가 거부됨(editable=false).
            let h = camera.device_handle();
            if let Err(e) = crsdk::properties::set(
                h,
                crsdk::properties::code::PRIORITY_KEY_SETTINGS,
                crsdk::properties::priority_key::PC_REMOTE,
            ) {
                tracing::warn!("set PriorityKey=PCRemote failed: {e:?}");
            }

            // PC 저장 경로 설정 (공식 샘플은 connect 직후 무조건 호출).
            let dir = if want.is_empty() {
                std::env::current_dir()
                    .map(|d| d.join("captures").to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "captures".to_string())
            } else {
                want
            };
            let _ = std::fs::create_dir_all(&dir);
            match crsdk::connection::set_save_info(h, &dir, "", -1) {
                Ok(()) => tracing::info!("save path set: {dir}"),
                Err(e) => tracing::warn!("set_save_info failed: {e:?}"),
            }
            // 렌즈 모델명 조회 (실패해도 빈 문자열로 진행).
            let lens = crsdk::properties::get_string(h, crsdk::properties::code::LENS_MODEL_NAME)
                .ok()
                .flatten()
                .unwrap_or_default();
            Ok((camera, dir, model, lens))
        })
        .await
        .unwrap_or_else(|join_err| Err(anyhow::anyhow!("task join: {join_err}")));

    match result {
        Ok((mut camera, dir, model, lens)) => {
            // 이벤트 수신기를 꺼내 어댑터 태스크로 넘긴다 (카메라 락을 잡지 않고 drain).
            let rx = camera.take_events();
            *s.camera.lock().await = Some(CameraCell(camera, model, lens));
            *s.save_path.lock().await = dir;
            if let Some(rx) = rx {
                let tx = s.events_tx.clone();
                let last_img = s.last_image.clone();
                let cam_state = s.camera.clone();
                tokio::task::spawn_blocking(move || {
                    // 카메라 Drop 시 sender가 사라져 recv가 Err → 루프 종료.
                    while let Ok(ev) = rx.recv() {
                        // PC 다운로드 완료 파일을 미리보기용으로 기억.
                        if let crsdk::CameraEvent::DownloadComplete { filename, .. } = &ev {
                            if !filename.is_empty() {
                                *last_img.blocking_lock() = Some(filename.clone());
                            }
                        }
                        let _ = tx.send(event_json(&ev)); // 구독자 0명이어도 OK
                        // 카메라 연결 끊김 → 상태 비움 → 자동 재연결 루프가 다시 붙음.
                        if let crsdk::CameraEvent::Disconnected { .. } = &ev {
                            *cam_state.blocking_lock() = None;
                            break;
                        }
                    }
                });
            }
            tracing::info!("camera connected");
            Ok(())
        }
        Err(e) => {
            tracing::warn!("connect failed: {e:#}");
            Err(format!("{e:#}"))
        }
    }
}

async fn connect(State(s): State<AppState>) -> impl IntoResponse {
    match connect_core(&s).await {
        Ok(()) => (StatusCode::OK, "connected".to_string()),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

/// CrDataType base nibble → 비트폭. 0이면 미상으로 64 가정.
fn type_bits(value_type: u32) -> u32 {
    match value_type & crsdk::control::data_type::BASE_MASK {
        1 => 8, 2 => 16, 3 => 32, 4 => 64, 5 => 128, _ => 64,
    }
}

/// 비트폭 기준 부호 확장 → i64.
fn signext(v: u64, bits: u32) -> i64 {
    if bits >= 64 { return v as i64; }
    let mask = (1u64 << bits) - 1;
    let m = v & mask;
    let sb = 1u64 << (bits - 1);
    if m & sb != 0 { (m | !mask) as i64 } else { m as i64 }
}

#[derive(Serialize)]
struct ControlInfoDto {
    value_type: u32,
    is_range: bool,
    is_array: bool,
    is_signed: bool,
    /// 부호 비트 켜져 있으면 비트폭 기준 부호확장, 아니면 그대로 i64 변환.
    values: Vec<i64>,
}

/// 디버그: 카메라가 실제로 보고하는 모든 property code 목록 + 일부 메타.
/// 어떤 속성이 있는지 한눈에 보고 빠진 게 카메라 한계인지 판별용.
/// 네트워크 발견 진단 — EnumCameraObjects가 찾는 모든 카메라를 연결타입/ssh와 함께 덤프.
/// (A7C를 Wi-Fi PC Remote 모드로 두고 같은 네트워크에서 호출해 WiFi 발견 가능 여부 확인용.)
async fn debug_enum() -> Response {
    match tokio::task::spawn_blocking(|| {
        let session = sdk_session();
        let cams = CameraEnumerator::new(session, 5).map_err(|e| format!("enumerate: {e:?}"))?;
        cams.list_all().map_err(|e| format!("list: {e:?}"))
    })
    .await
    {
        Ok(Ok(list)) => Json(serde_json::json!({
            "count": list.len(),
            "cameras": list.iter().map(|c| serde_json::json!({
                "name": c.name,
                "model": c.model,
                "usb_pid": format!("0x{:04X}", c.usb_pid),
                "connection_status": c.connection_status,
                "ssh_support": c.ssh_support,
                "connection_type": c.connection_type,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

async fn debug_all_codes(State(s): State<AppState>) -> Response {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::properties::get_all(handle)).await {
        Ok(Ok(props)) => {
            let mut rows: Vec<(String, String, bool, usize)> = props
                .iter()
                .map(|p| {
                    (
                        format!("0x{:04X}", p.code),
                        format!("0x{:04X}", p.value_type),
                        p.editable,
                        p.allowed.len(),
                    )
                })
                .collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            Json(serde_json::json!({
                "count": rows.len(),
                "rows": rows.iter().map(|(c,t,e,n)| serde_json::json!({
                    "code": c, "type": t, "editable": e, "allowed_n": n,
                })).collect::<Vec<_>>(),
            }))
            .into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

/// 연결된 바디가 노출하는 속성 코드 집합 + 모델명 — 프론트가 UI를 큐레이션한다.
async fn capabilities(State(s): State<AppState>) -> Response {
    let (handle, model) = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => (c.0.device_handle(), c.1.clone()),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::capability::Capabilities::probe(handle, model))
        .await
    {
        Ok(Ok(caps)) => Json(serde_json::json!({
            "model": caps.model,
            "supported": caps.supported.iter().map(|c| format!("0x{c:04X}")).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

async fn focus_nearfar_info(State(s): State<AppState>) -> Response {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    let result = tokio::task::spawn_blocking(move || {
        crsdk::control::get_info(handle, crsdk::control::code::NEAR_FAR)
    })
    .await;
    match result {
        Ok(Ok(info)) => {
            let bits = type_bits(info.value_type);
            let values: Vec<i64> = if info.is_signed() {
                info.values.iter().map(|&v| signext(v, bits)).collect()
            } else {
                info.values.iter().map(|&v| v as i64).collect()
            };
            Json(ControlInfoDto {
                value_type: info.value_type,
                is_range: info.is_range(),
                is_array: info.is_array(),
                is_signed: info.is_signed(),
                values,
            })
            .into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct FocusStep {
    step: i32, // 부호=방향(음수=Near, 양수=Far), 크기=스텝
}

async fn focus_near_far(
    State(s): State<AppState>,
    Json(body): Json<FocusStep>,
) -> impl IntoResponse {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    let step = body.step;
    match tokio::task::spawn_blocking(move || crsdk::control::focus_near_far(handle, step)).await {
        Ok(Ok(())) => (StatusCode::OK, "ok".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── 소프트웨어 오토포커스 (MF + 컨트라스트 검출 풀스윕) ───────────────────
// A7C는 절대 초점 위치 API가 없어 NearFar 상대 스텝만 가능 → 스텝을 세면서
// 윈도우를 풀스윕하고 각 지점 선명도(라플라시안 분산)를 측정, 최고점으로 복귀한다.
// 선명도 ROI 중심은 사용자가 라이브뷰에서 찍은 정규화 좌표 (x,y).

/// NearFar 1회 구동 (블로킹 SDK 호출을 spawn_blocking).
async fn af_drive(handle: i64, step: i32) {
    let _ = tokio::task::spawn_blocking(move || crsdk::control::focus_near_far(handle, step)).await;
}

/// 한 지점 선명도: stale 프레임 비우고 fresh `frames`장 평균. 라이브뷰 없으면 None.
async fn af_grab(
    rx: &mut broadcast::Receiver<Arc<Vec<u8>>>,
    cx: f64,
    cy: f64,
    roi_w: f64,
    roi_h: f64,
    frames: u32,
) -> Option<f64> {
    while rx.try_recv().is_ok() {} // 구동 전 쌓인 오래된 프레임 폐기
    let mut sum = 0.0;
    let mut k = 0.0;
    for _ in 0..frames {
        let f = match tokio::time::timeout(Duration::from_millis(700), rx.recv()).await {
            Ok(Ok(f)) => f,
            // 브로드캐스트 랙(느린 소비자)은 복구 가능 — 이 프레임만 건너뛰고 계속.
            // 치명적으로 처리하면 AF 부하가 유발한 랙에 스스로 중단됨(오탐 "라이브뷰 없음").
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break, // 타임아웃 또는 채널 닫힘
        };
        if let Ok(Some(m)) = tokio::task::spawn_blocking(move || {
            autofocus::focus_measure(&f[..], cx, cy, roi_w, roi_h)
        })
        .await
        {
            sum += m;
            k += 1.0;
        }
    }
    if k > 0.0 {
        Some(sum / k)
    } else {
        None
    }
}

/// 현재 위치에서 Far(+step)로 n번 이동하며 측정. (scores, best_index, bracketed) 반환.
/// bracketed=true면 피크를 지나 명확히 하강해 **조기 종료**(남은 윈도우 측정 생략 = 속도).
/// cancel=true면 즉시 중단(부분 scores). 끝나면 focus는 마지막 측정 지점.
#[allow(clippy::too_many_arguments)]
async fn af_phase(
    handle: i64,
    rx: &mut broadcast::Receiver<Arc<Vec<u8>>>,
    cx: f64,
    cy: f64,
    roi_w: f64,
    roi_h: f64,
    frames: u32,
    settle: Duration,
    step: i32,
    n: u32,
    cancel: &std::sync::atomic::AtomicBool,
    events: &broadcast::Sender<String>,
    phase: &str,
) -> (Vec<f64>, usize, bool) {
    use std::sync::atomic::Ordering;
    let mut scores = Vec::with_capacity(n as usize + 1);
    let mut best = -1.0f64;
    let mut best_k = 0usize;
    let mut drops = 0u32; // best 대비 급락 연속 횟수
    let mut bracketed = false;
    for i in 0..=n as usize {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let sc = af_grab(rx, cx, cy, roi_w, roi_h, frames).await.unwrap_or(0.0);
        if sc > best {
            best = sc;
            best_k = i;
            drops = 0;
        } else if sc < best * 0.6 {
            drops += 1; // 피크 대비 급락
        } else {
            drops = 0; // 어깨/노이즈 1점은 리셋(broad peak 보호)
        }
        scores.push(sc);
        // 진행률을 SSE로 흘려 UI가 실시간 표시.
        let _ = events.send(format!(
            r#"{{"type":"af_progress","phase":"{phase}","i":{i},"n":{n},"score":{sc:.0}}}"#
        ));
        // 피크를 충분히 지나 3연속 급락 → 가둠 확정, 남은 윈도우 측정 생략(속도). 보수적이라
        // 평탄/broad 장면은 발동 안 함(그땐 끝까지 측정 후 margin/widen 판정).
        if drops >= 3 && best > 0.0 {
            bracketed = true;
            break;
        }
        if i < n as usize {
            af_drive(handle, step).await;
            tokio::time::sleep(settle).await;
        }
    }
    (scores, best_k, bracketed)
}

/// Near 방향(-)으로 step을 times번 구동. **스텝마다 settle** — rapid 연속 NearFar는
/// 카메라가 대부분 드롭해(실측: -10 rapid가 +10 settled를 못 되돌림) 의도한 거리만큼 안 움직인다.
/// 측정 스윕과 동일 cadence라야 재현-기하(measure_and_land)가 성립.
async fn af_move_near(handle: i64, step: i32, times: usize, settle: Duration) {
    for _ in 0..times {
        af_drive(handle, -step.abs()).await;
        tokio::time::sleep(settle).await;
    }
}

/// 백래시 강건한 측정+착지: 현재 위치 중심 ±(m/2·step) 윈도우를 측정하고 피크 위치에 정착.
/// 핵심 — 백래시는 *방향 반전*에서 생기므로, 측정 스윕(−오버슈트→+스윕)과 착지를 **동일 기하로
/// 재현**한다: 스윕 시작점으로 −방향 복귀 후 +방향으로 정확히 pk스텝. 측정 때와 같은 −→+ 진입이라
/// pk 물리 위치가 그대로 재현된다(step 카운트가 반전 없이 선형 → 신뢰). (scores, pk) 반환.
#[allow(clippy::too_many_arguments)]
async fn measure_and_land(
    handle: i64,
    rx: &mut broadcast::Receiver<Arc<Vec<u8>>>,
    cx: f64,
    cy: f64,
    roi_w: f64,
    roi_h: f64,
    frames: u32,
    settle: Duration,
    move_gap: Duration,
    step: i32,
    m: u32,
    cancel: &std::sync::atomic::AtomicBool,
    events: &broadcast::Sender<String>,
    phase: &str,
) -> (Vec<f64>, usize, bool) {
    // 재배치 이동(오버슈트·복귀·착지)은 측정을 안 하므로 settle(이미지 안정화용 250ms)이 아니라
    // 명령 등록만 되는 move_gap으로 충분 — 등록되면 스텝당 이동량은 gap과 무관해 재현-기하 유지.
    // 측정 스윕(af_phase)만 settle. (rapid=0ms는 드롭되니 move_gap은 등록 임계 위로.)
    // 1) −방향 오버슈트(래시를 Near로 확립) + 윈도우 중심 정렬.
    af_move_near(handle, step, (m / 2) as usize, move_gap).await;
    // 2) +방향 측정 스윕 → pk. (focus는 +방향 끝. bracketed면 조기 종료.)
    let (scores, pk, bracketed) = af_phase(
        handle, rx, cx, cy, roi_w, roi_h, frames, settle, step, m, cancel, events, phase,
    )
    .await;
    // 3) 측정과 동일 기하 재현 착지: 스윕 시작점으로 −복귀(= +스윕한 만큼) 후 +방향 pk스텝.
    let swept = scores.len().saturating_sub(1); // 실제 +구동 횟수(조기종료/취소 시 부분)
    af_move_near(handle, step, swept, move_gap).await;
    for _ in 0..pk {
        af_drive(handle, step).await;
        tokio::time::sleep(move_gap).await;
    }
    (scores, pk, bracketed)
}

#[derive(Deserialize)]
struct SwAfReq {
    x: Option<f64>,          // ROI 중심 정규화(없으면 0.5 중앙)
    y: Option<f64>,
    roi: Option<f64>,        // ROI 한 변 비율(기본 0.25; 정사각형 점-선택)
    roi_w: Option<f64>,      // ROI 가로 비율(직사각형 박스 — 없으면 roi)
    roi_h: Option<f64>,      // ROI 세로 비율(직사각형 박스 — 없으면 roi)
    step: Option<i32>,       // coarse NearFar 스텝(기본 5)
    count: Option<u32>,      // coarse 스윕 지점 수(기본 24; 윈도우 = ±count/2 스텝)
    fine_step: Option<i32>,  // 캐스케이드 종착 스텝(기본 1=granularity, 정밀)
    settle_ms: Option<u64>,  // 측정 전 안정 대기(기본 250)
    move_ms: Option<u64>,    // 재배치 이동 스텝 gap(측정 아님 — 등록만, 기본 120)
    frames: Option<u32>,     // 지점당 평균 프레임 수(기본 2)
    threshold: Option<f64>,  // (continuous) baseline 대비 이 비율 미만이면 재합초(기본 0.7)
    check_ms: Option<u64>,   // (continuous) 모니터 주기(기본 600)
}

#[derive(Serialize)]
struct SwAfResult {
    best_index: usize,        // fine 단계 최종 best
    best_score: f64,
    points: usize,            // fine 측정 지점 수
    coarse_scores: Vec<f64>,  // 진단용
    fine_scores: Vec<f64>,
    x: f64,
    y: f64,
}

/// SW-AF 스윕 파라미터(요청에서 파싱·클램프). lock 코어와 continuous가 공유.
#[derive(Clone, Copy)]
struct SwAfParams {
    cx: f64,
    cy: f64,
    roi_w: f64,
    roi_h: f64,
    step: i32,
    n: u32,
    fine_step: i32,
    settle: Duration,
    fine_settle: Duration,
    move_gap: Duration,
    frames: u32,
}

impl SwAfParams {
    fn from_req(b: &SwAfReq) -> Self {
        let step = b.step.unwrap_or(8).abs().max(1); // 다중해상도 레벨0 시작 스텝(범위용, 큼)
        Self {
            cx: b.x.unwrap_or(0.5),
            cy: b.y.unwrap_or(0.5),
            roi_w: b.roi_w.or(b.roi).unwrap_or(0.25),
            roi_h: b.roi_h.or(b.roi).unwrap_or(0.25),
            step,
            n: b.count.unwrap_or(24).clamp(4, 200),
            fine_step: b.fine_step.unwrap_or(1).abs().clamp(1, step),
            settle: Duration::from_millis(b.settle_ms.unwrap_or(250)),
            // fine/refine은 step이 작아 이미지가 빨리 안정 → 측정 settle 절반(최소 100ms).
            fine_settle: Duration::from_millis((b.settle_ms.unwrap_or(250) / 2).max(100)),
            move_gap: Duration::from_millis(b.move_ms.unwrap_or(120).clamp(20, 1000)),
            frames: b.frames.unwrap_or(2).clamp(1, 5),
        }
    }
}

/// 합초 1회(coarse→fine + 백래시 보정). (coarse_scores, ck, fine_scores, fk, best_score) 반환.
/// cancel=true면 coarse 후 조기 종료(이미 coarse best로 복귀). 진행률은 events로 emit.
async fn swaf_lock(
    handle: i64,
    rx: &mut broadcast::Receiver<Arc<Vec<u8>>>,
    p: &SwAfParams,
    cancel: &std::sync::atomic::AtomicBool,
    events: &broadcast::Sender<String>,
) -> (Vec<f64>, usize, Vec<f64>, usize, f64) {
    use std::sync::atomic::Ordering;
    // 큰 widen 이동 후엔 방향 반전 누적 백래시로 인덱스 모델과 실제 위치가 어긋나, 피크를
    // 보고도 한참 못 앉을 수 있다(실측: coarse 689 봤는데 final 83). 그 경우 더 가까워진
    // 위치에서 한 번 더 락하면 widen 없이 정밀 수렴(83→780). 최대 2패스.
    let mut pass = 0u32;
    loop {
        pass += 1;
        // 다중해상도 coarse→fine 캐스케이드. 단일 스텝은 "범위(큰 스텝)"와 "좁은 피크 비앨리어싱
        // (작은 스텝)"을 동시에 못 잡는다 → 레벨0은 큰 스텝(s0)+widen으로 영역을 가두고, 이후 스텝을
        // 절반씩 줄이며 직전 ±스텝을 더 촘촘히 재측정·재현착지, fine_step(정밀)까지 줌인.
        // 범위=레벨0 큰 스텝, 정밀=step1 종착 → 범위/정밀 트레이드오프 해소.
        let s0 = p.step.max(p.fine_step);
        // 레벨0: 범위 확보 + widen. 피크가 윈도우 밖(best가 가장자리)이면 best 중심으로 2배 확장,
        // 조기종료(early=피크 지나 하강)나 중앙 절반 안이면 가둠 완료.
        let mut n = p.n;
        // widen 상한 = 1회 doubling. 더 멀면 reconverge 2패스가 더 가까운 위치에서 처리(매 widen이
        // 전체 재스윕이라 폭주하면 느림 — 시간 묶기). 저콘트라스트 평탄 장면 폭주도 방지.
        let max_n = p.n.saturating_mul(2);
        let (coarse_scores, ck) = loop {
            let (scores, k, early) = measure_and_land(
                handle, rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames, p.settle, p.move_gap, s0, n,
                cancel, events, "coarse",
            )
            .await;
            let end = scores.len().saturating_sub(1);
            let margin = (end / 4).max(1);
            let interior = k >= margin && k + margin <= end;
            if early || interior || n >= max_n || cancel.load(Ordering::SeqCst) {
                break (scores, k);
            }
            n = (n * 2).min(max_n);
        };
        if cancel.load(Ordering::SeqCst) {
            let bs = coarse_scores.get(ck).copied().unwrap_or(0.0);
            return (coarse_scores, ck, Vec::new(), 0, bs);
        }
        // 줌인 레벨들: 스텝 절반씩, 직전 ±스텝(±step)을 next 단위로 덮어(m≈4+여유) 재측정·재현착지.
        // fine_step 도달까지. 각 레벨이 직전 잔차(±step)를 새 해상도로 다시 가두므로 앨리어싱 없음.
        let mut step = s0;
        let mut fine_scores = coarse_scores.clone();
        let mut fk = ck;
        while step > p.fine_step && !cancel.load(Ordering::SeqCst) {
            let next = (step / 2).max(p.fine_step);
            let m = (2 * step as u32 / next as u32).max(4) + 2;
            let phase = if next == p.fine_step { "fine" } else { "zoom" };
            let (s2, k2, _) = measure_and_land(
                handle, rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames, p.fine_settle, p.move_gap, next,
                m, cancel, events, phase,
            )
            .await;
            fine_scores = s2;
            fk = k2;
            step = next;
        }
        // 최종 위치에서 라플라시안(행렬연산)을 한 번 더 측정 → 백래시 보정 이동 후 실제 도달한
        // 샤프니스 정상값. 스윕 중 fk 점수는 측정 당시 위치 값이라 복귀 후와 다를 수 있어 재측정.
        tokio::time::sleep(p.settle).await;
        let final_score = af_grab(rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames)
            .await
            .unwrap_or_else(|| fine_scores.get(fk).copied().unwrap_or(0.0));
        // 피크는 봤는데 한참 못 앉았으면(widen 후 드리프트) 더 가까워진 위치에서 1회 더 락.
        // 평탄 장면은 peak≈final이라 안 걸림.
        let peak = coarse_scores
            .iter()
            .chain(fine_scores.iter())
            .copied()
            .fold(0.0_f64, f64::max);
        if pass >= 2 || final_score >= 0.6 * peak || cancel.load(Ordering::SeqCst) {
            return (coarse_scores, ck, fine_scores, fk, final_score);
        }
    }
}

/// 현재 조리개 f-number(예: 1.8). 실패/미지원 시 None.
async fn current_f_number(handle: i64) -> Option<f64> {
    let props = tokio::task::spawn_blocking(move || crsdk::properties::get_all(handle))
        .await
        .ok()?
        .ok()?;
    let raw = props
        .iter()
        .find(|p| p.code == crsdk::properties::code::F_NUMBER)?
        .current;
    (100..=9999).contains(&raw).then_some(raw as f64 / 100.0)
}

/// 조리개 기반 ROI 박스 크기. 개방(작은 f, 얕은 DOF)일수록 작게 → 서로 다른 거리 영역이
/// 섞이지 않아 측정이 깨끗(피크가 선명). f/5.6에서 현행 기본값 roi 0.25에 앵커, f-number에 비례.
/// (coarse 스텝은 다중해상도 캐스케이드가 알아서 줌인하므로 조리개로 안 건드린다.)
fn aperture_roi(f: f64) -> f64 {
    (0.25 * f / 5.6).clamp(0.10, 0.40)
}

/// 요청이 ROI를 안 줬으면 현재 조리개로 박스 크기를 채운다(개방→작게). 명시값은 존중.
async fn apply_aperture_defaults(handle: i64, b: &mut SwAfReq) {
    if b.roi.is_some() || b.roi_w.is_some() || b.roi_h.is_some() {
        return;
    }
    if let Some(f) = current_f_number(handle).await {
        b.roi = Some(aperture_roi(f));
    }
}

async fn sw_autofocus(State(s): State<AppState>, Json(mut b): Json<SwAfReq>) -> Response {
    use std::sync::atomic::Ordering;
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    // 단일 실행 가드
    if s.af_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "autofocus already running").into_response();
    }
    // af_active는 이 가드가 모든 종료(정상/에러 return/future 드롭)에서 해제 — 소유자만 해제.
    // 취소는 별도 af_cancel 신호로만 처리하고, 이전 런의 잔여 취소값은 여기서 초기화한다.
    let _af_guard = RunGuard(s.af_active.clone());
    s.af_cancel.store(false, Ordering::SeqCst);
    apply_aperture_defaults(handle, &mut b).await; // 조리개로 step/roi 기본값(개방→작게)
    let p = SwAfParams::from_req(&b);
    let cancel = s.af_cancel.clone();
    let events = s.events_tx.clone();
    let mut rx = s.lv_tx.subscribe();

    // 라이브뷰 가동 확인: 첫 프레임이 안 오면 중단.
    if af_grab(&mut rx, p.cx, p.cy, p.roi_w, p.roi_h, 1).await.is_none() {
        return (StatusCode::PRECONDITION_REQUIRED, "live view not running").into_response();
    }

    let (coarse_scores, ck, fine_scores, fk, best_score) =
        swaf_lock(handle, &mut rx, &p, &cancel, &events).await;

    // fine까지 갔으면 fine 기준, coarse에서 취소됐으면 coarse 기준.
    let (best_index, points) = if fine_scores.is_empty() {
        (ck, coarse_scores.len())
    } else {
        (fk, fine_scores.len())
    };
    tracing::info!(
        "sw-af: best {best_index}/{} score {best_score:.0} (x={:.2} y={:.2})",
        points.saturating_sub(1),
        p.cx,
        p.cy
    );
    Json(SwAfResult {
        best_index,
        best_score,
        points,
        coarse_scores,
        fine_scores,
        x: p.cx,
        y: p.cy,
    })
    .into_response()
}

/// 연속 AF: 초기 합초 후 모니터 루프 — ROI 선명도가 baseline 대비 threshold 미만으로
/// 떨어지면(피사체 이동/카메라 흔들림) 재합초. /cancel(af_cancel=true)로 정지.
/// 즉시 "started" 반환하고 백그라운드 진행, 상태는 /events SSE(af_continuous).
async fn sw_autofocus_continuous(State(s): State<AppState>, Json(mut b): Json<SwAfReq>) -> Response {
    use std::sync::atomic::Ordering;
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    if s.af_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "autofocus already running").into_response();
    }
    // af_active 소유권은 spawn 태스크로 옮기는 가드가 태스크 종료 시 해제(소유자만 해제).
    // 취소는 af_cancel 신호로만; 이전 런 잔여 취소값 초기화.
    s.af_cancel.store(false, Ordering::SeqCst);
    let guard = RunGuard(s.af_active.clone());
    apply_aperture_defaults(handle, &mut b).await; // 조리개로 step/roi 기본값(개방→작게)
    let p = SwAfParams::from_req(&b);
    let threshold = b.threshold.unwrap_or(0.7).clamp(0.3, 0.95);
    let check = Duration::from_millis(b.check_ms.unwrap_or(600).clamp(150, 5000));
    let cancel = s.af_cancel.clone();
    let events = s.events_tx.clone();
    let lv_tx = s.lv_tx.clone();
    let cam = s.camera.clone();
    // 추적AF: 대상 ROI를 초기 지점으로 세팅(retarget이 이후 갱신). 태스크가 매 사이클 관측.
    *s.af_target.lock().unwrap_or_else(|e| e.into_inner()) = (p.cx, p.cy, p.roi_w, p.roi_h);
    let target = s.af_target.clone();

    tokio::spawn(async move {
        let _guard = guard; // 태스크 종료(정상/조기 return) 시 af_active 해제
        let mut rx = lv_tx.subscribe();
        if af_grab(&mut rx, p.cx, p.cy, p.roi_w, p.roi_h, 1).await.is_none() {
            let _ = events.send(
                r#"{"type":"af_continuous","state":"error","reason":"no_liveview"}"#.to_string(),
            );
            return;
        }
        // 초기 합초
        let (_, _, _, _, mut baseline) = swaf_lock(handle, &mut rx, &p, &cancel, &events).await;
        let _ = events.send(format!(
            r#"{{"type":"af_continuous","state":"locked","score":{baseline:.0}}}"#
        ));
        // 모니터 루프
        while !cancel.load(Ordering::SeqCst) {
            tokio::time::sleep(check).await;
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            // 카메라가 끊기면(스테일 핸들) 스윕이 무의미 → 종료해 af_active 가드를 해제한다
            // (그렇지 않으면 무한 모니터가 재연결 후 재시작을 막는다).
            if cam.lock().await.as_ref().map(|c| c.0.device_handle()) != Some(handle) {
                break;
            }
            // 추적AF: 클라가 retarget으로 갱신한 최신 ROI를 매 사이클 반영(피사체 추적).
            let pc = {
                let (tx, ty, tw, th) = *target.lock().unwrap_or_else(|e| e.into_inner());
                let mut q = p;
                q.cx = tx; q.cy = ty; q.roi_w = tw; q.roi_h = th;
                q
            };
            let cur = af_grab(&mut rx, pc.cx, pc.cy, pc.roi_w, pc.roi_h, pc.frames)
                .await
                .unwrap_or(0.0);
            if baseline > 0.0 && cur < baseline * threshold {
                let _ = events.send(format!(
                    r#"{{"type":"af_continuous","state":"refocus","score":{cur:.0}}}"#
                ));
                // 재-lock은 좁게: 피사체는 이미 초점 근처에서 조금씩 이동한다. 풀스윕(step·n 큼)은
                // 순간 큰 디포커스로 검출을 놓쳐 bbox가 사라짐 → 스텝·윈도우를 줄인 국소 탐색.
                let mut relock = pc;
                relock.n = relock.n.min(10);
                relock.step = (relock.step / 2).max(relock.fine_step);
                let (_, _, _, _, nb) = swaf_lock(handle, &mut rx, &relock, &cancel, &events).await;
                baseline = nb;
                let _ = events.send(format!(
                    r#"{{"type":"af_continuous","state":"locked","score":{baseline:.0}}}"#
                ));
            } else {
                let _ = events.send(format!(
                    r#"{{"type":"af_continuous","state":"hold","score":{cur:.0}}}"#
                ));
            }
        }
        let _ = events.send(r#"{"type":"af_continuous","state":"stopped"}"#.to_string());
        tracing::info!("sw-af continuous: stopped");
    });
    (StatusCode::OK, "continuous started").into_response()
}

async fn sw_autofocus_cancel(State(s): State<AppState>) -> impl IntoResponse {
    // 실행 가드(af_active)는 소유 태스크가 해제한다. 여기선 취소 신호만 올린다.
    s.af_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, "cancel")
}

#[derive(Deserialize)]
struct RetargetReq { x: f64, y: f64, w: Option<f64>, h: Option<f64> }

/// 추적AF: 진행 중인 연속 AF의 대상 ROI를 갱신(피사체가 움직이면 클라가 새 centroid 전송).
/// 좌표는 미회전 정규화(SW-AF와 동일). 연속 AF 미실행 중이면 다음 시작 때 덮여 무해.
async fn sw_autofocus_retarget(State(s): State<AppState>, Json(b): Json<RetargetReq>) -> impl IntoResponse {
    let mut g = s.af_target.lock().unwrap_or_else(|e| e.into_inner());
    g.0 = b.x.clamp(0.0, 1.0);
    g.1 = b.y.clamp(0.0, 1.0);
    if let Some(w) = b.w { g.2 = w.clamp(0.05, 0.9); }
    if let Some(h) = b.h { g.3 = h.clamp(0.05, 0.9); }
    (StatusCode::OK, "retargeted")
}

#[derive(Deserialize)]
struct SharpReq {
    x: Option<f64>,
    y: Option<f64>,
    roi: Option<f64>,
    img: Option<u8>, // 1이면 측정한 프레임(JPEG)을 그대로 반환(눈으로 확인용)
}

/// 진단: 현재 라이브뷰 프레임의 (x,y) ROI 라플라시안 분산을 측정.
/// img=1 → 측정에 쓴 그 JPEG을 반환(`X-Sharpness` 헤더에 점수). 아니면 JSON {score}.
/// "라플라시안 돌리고 이미지를 까봐서 정상값 확인"용.
async fn debug_sharpness(State(s): State<AppState>, Query(q): Query<SharpReq>) -> Response {
    let cx = q.x.unwrap_or(0.5);
    let cy = q.y.unwrap_or(0.5);
    let roi = q.roi.unwrap_or(0.25);
    let mut rx = s.lv_tx.subscribe();
    while rx.try_recv().is_ok() {} // 최신 프레임을 위해 stale 비움
    let frame = match tokio::time::timeout(Duration::from_millis(800), rx.recv()).await {
        Ok(Ok(f)) => f,
        _ => return (StatusCode::PRECONDITION_REQUIRED, "live view not running").into_response(),
    };
    let bytes = frame.to_vec();
    let measured = bytes.clone();
    let score = tokio::task::spawn_blocking(move || {
        autofocus::focus_measure(&measured, cx, cy, roi, roi)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(0.0);

    if q.img == Some(1) {
        let sval = HeaderValue::from_str(&format!("{score:.1}"))
            .unwrap_or_else(|_| HeaderValue::from_static("0"));
        (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg")),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                (header::HeaderName::from_static("x-sharpness"), sval),
            ],
            bytes,
        )
            .into_response()
    } else {
        Json(serde_json::json!({ "score": score, "x": cx, "y": cy, "roi": roi })).into_response()
    }
}

#[derive(Deserialize)]
struct SetSavePath {
    path: String,
    #[serde(default)]
    prefix: String, // 파일명 접두사 (빈 문자열이면 카메라 기본 DSC)
}

async fn set_save_path(
    State(s): State<AppState>,
    Json(body): Json<SetSavePath>,
) -> impl IntoResponse {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    let dir = body.path.trim().to_string();
    if dir.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty path".to_string());
    }

    let dir2 = dir.clone();
    let prefix = body.prefix.trim().to_string();
    let res = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir2)
            .map_err(|e| format!("mkdir: {e}"))?;
        crsdk::connection::set_save_info(handle, &dir2, &prefix, -1)
            .map_err(|e| format!("set_save_info: {e:?}"))?;
        Ok::<_, String>(dir2)
    })
    .await;

    match res {
        Ok(Ok(applied)) => {
            *s.save_path.lock().await = applied.clone();
            (StatusCode::OK, applied)
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

/// 저장 폴더를 OS 네이티브 폴더 선택창으로 고른다(서버=PC 쪽에 창이 뜸).
/// 다이얼로그는 서버의 현재 save_path 에서 열린다(TM_INITDIR 환경변수 주입 — 이스케이프 회피).
/// 고른 절대경로를 반환만 하고 적용은 UI가 /api/savepath 로. 취소 시 204.
async fn browse_save_path(State(s): State<AppState>) -> impl IntoResponse {
    let init = s.save_path.lock().await.clone();
    let res = tokio::task::spawn_blocking(move || -> Option<String> {
        #[cfg(target_os = "macos")]
        {
            // 현재 경로가 있으면 거기서 열기(default location), 없으면 기본.
            let out = std::process::Command::new("osascript")
                .env("TM_INITDIR", &init)
                .args([
                    "-e", "set d to system attribute \"TM_INITDIR\"",
                    "-e", "if d is \"\" then",
                    "-e", "  POSIX path of (choose folder with prompt \"TetherMoon: 저장 폴더 선택\")",
                    "-e", "else",
                    "-e", "  POSIX path of (choose folder with prompt \"TetherMoon: 저장 폴더 선택\" default location (POSIX file d))",
                    "-e", "end if",
                ])
                .output().ok()?;
            if !out.status.success() { return None; } // 취소 시 non-zero
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if p.is_empty() { None } else { Some(p) }
        }
        #[cfg(target_os = "windows")]
        {
            // FolderBrowserDialog는 STA 필요. 한글 경로 위해 출력 UTF-8 강제.
            // 현재 경로는 $env:TM_INITDIR 로 받아 SelectedPath 초기값으로(거기서 열림).
            let script = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
                Add-Type -AssemblyName System.Windows.Forms; \
                $d=New-Object System.Windows.Forms.FolderBrowserDialog; \
                $d.Description='TetherMoon save folder'; \
                if($env:TM_INITDIR -and (Test-Path $env:TM_INITDIR)){ $d.SelectedPath=$env:TM_INITDIR }; \
                if($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK){[Console]::Out.Write($d.SelectedPath)}";
            let out = std::process::Command::new("powershell")
                .env("TM_INITDIR", &init)
                .args(["-NoProfile", "-STA", "-Command", script])
                .output().ok()?;
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if p.is_empty() { None } else { Some(p) }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        { let _ = init; None }
    }).await;

    match res {
        Ok(Some(path)) => (StatusCode::OK, path),
        Ok(None) => (StatusCode::NO_CONTENT, String::new()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

async fn disconnect(State(s): State<AppState>) -> impl IntoResponse {
    // Camera Drop이 deactivate_callback → disconnect → release 순서로 실행
    *s.camera.lock().await = None;
    tracing::info!("camera disconnected");
    (StatusCode::OK, "disconnected")
}

// 한 장 촬영 (blocking): 포커스 모드에 따라 MF=즉시 캡처 / AF=S1 반누름 시퀀스.
fn capture_one(handle: i64) -> crsdk::SdkResult<()> {
    let mf = matches!(
        crsdk::properties::get(handle, crsdk::properties::code::FOCUS_MODE),
        Ok(Some(p)) if p.current == crsdk::properties::focus_mode::MF
    );
    if mf {
        crsdk::shutter::capture(handle)
    } else {
        crsdk::shutter::capture_af(handle)
    }
}

async fn shutter(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    }; // lock 해제

    match tokio::task::spawn_blocking(move || capture_one(handle)).await {
        Ok(Ok(())) => (StatusCode::OK, "captured".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── 셔터 press-hold (연사) ───────────────────────────────────────────────
// CAPTURE 버튼을 누르면 down, 떼면 up. 누르는 동안 드라이브가 연속이면 연사.
async fn shutter_down(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::shutter::shutter_down(handle)).await {
        Ok(Ok(())) => (StatusCode::OK, "down".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

async fn shutter_up(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::shutter::shutter_up(handle)).await {
        Ok(Ok(())) => (StatusCode::OK, "up".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── 반셔터 (S1 반누름) press-hold → AF 합초·고정 / 해제 ────────────────────
// down=S1 LOCKED(AF 탐색·고정), up=S1 UNLOCKED. CAPTURE와 별개로 사전 합초용.
async fn half_down(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || {
        crsdk::properties::set(handle, crsdk::properties::code::S1, crsdk::properties::lock::LOCKED)
    })
    .await
    {
        Ok(Ok(())) => (StatusCode::OK, "down".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

async fn half_up(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || {
        crsdk::properties::set(handle, crsdk::properties::code::S1, crsdk::properties::lock::UNLOCKED)
    })
    .await
    {
        Ok(Ok(())) => (StatusCode::OK, "up".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── 진단: 자이로(중력센서) 레벨 — 라이브뷰 자동회전 가능 여부 확인용 ──
#[derive(Serialize)]
struct LevelDto { on: bool, roll: i32, pitch: i32, z: i32 }

async fn level_info(State(s): State<AppState>) -> Response {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::liveview::get_level(handle)).await {
        Ok(Ok(l)) => Json(LevelDto { on: l.on, roll: l.roll, pitch: l.pitch, z: l.z }).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

// ── 진단: AF 프레임 실위치 — 명령 좌표 vs 카메라가 실제 놓은 박스 (증상1 보정용) ──
#[derive(Serialize)]
struct AfFrameDto { valid: bool, x_num: u32, x_deno: u32, y_num: u32, y_deno: u32, width: u32, height: u32 }

async fn af_frame_info(State(s): State<AppState>) -> Response {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::liveview::get_af_frame(handle)).await {
        Ok(Ok(f)) => Json(AfFrameDto {
            valid: f.valid, x_num: f.x_num, x_deno: f.x_deno,
            y_num: f.y_num, y_deno: f.y_deno, width: f.width, height: f.height,
        }).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

// ── 벌브 타이머: 셔터 BULB로 N초 정밀 노출 (호스트가 down→sleep→up 타이밍 제어) ──
// A7C는 카메라 네이티브 벌브타이머(0x0209) 미지원 → 서버가 홀드 시간을 대신 잰다.
#[derive(Deserialize)]
struct BulbReq { seconds: u64 }

async fn bulb(State(s): State<AppState>, Json(b): Json<BulbReq>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let secs = b.seconds.clamp(1, 900); // 1초~15분
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    // 중복 트리거 방지: false→true 교체에 성공한 호출만 진행.
    if s.bulb_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "bulb already running".to_string());
    }
    let active = s.bulb_active.clone();
    tokio::spawn(async move {
        // 셔터를 BULB(0)로 보장한 뒤 노출 시작.
        let start = tokio::task::spawn_blocking(move || {
            crsdk::properties::set(handle, crsdk::properties::code::SHUTTER_SPEED, 0)?;
            crsdk::shutter::shutter_down(handle)
        })
        .await;
        match start {
            Ok(Ok(())) => tokio::time::sleep(std::time::Duration::from_secs(secs)).await,
            other => tracing::warn!("bulb start failed: {other:?}"),
        }
        // 노출 종료 (실패해도 best-effort).
        let _ = tokio::task::spawn_blocking(move || crsdk::shutter::shutter_up(handle)).await;
        active.store(false, Ordering::SeqCst);
        tracing::info!("bulb exposure done ({secs}s)");
    });
    (StatusCode::OK, format!("bulb {secs}s"))
}

// ── 인터벌(타임랩스): 소프트웨어로 N초마다 M장 촬영 (A7C는 내장 인터벌 설정 미노출) ──
// interval_cancel로 취소 신호. 대기는 1초 단위로 쪼개 /stop에 ~1s 내 반응.
#[derive(Deserialize)]
struct IntervalReq { interval_sec: u64, count: u32 }

async fn interval_start(State(s): State<AppState>, Json(b): Json<IntervalReq>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let interval = b.interval_sec.clamp(1, 3600);
    let count = b.count.clamp(1, 10000);
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    if s.interval_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "interval already running".to_string());
    }
    // 실행 가드는 태스크로 이동해 종료 시 interval_active 해제(소유자만). 취소는 interval_cancel 신호.
    s.interval_cancel.store(false, Ordering::SeqCst);
    let guard = RunGuard(s.interval_active.clone());
    let cancel = s.interval_cancel.clone();
    tokio::spawn(async move {
        let _guard = guard; // 태스크 종료 시 interval_active 해제
        for i in 0..count {
            if cancel.load(Ordering::SeqCst) { break; } // 취소
            match tokio::task::spawn_blocking(move || capture_one(handle)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("interval shot {i} failed: {e:?}"),
                Err(e) => tracing::warn!("interval shot {i} join: {e}"),
            }
            if i + 1 < count {
                for _ in 0..interval {
                    if cancel.load(Ordering::SeqCst) { break; }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        tracing::info!("interval done");
    });
    (StatusCode::OK, format!("interval {count}x@{interval}s"))
}

async fn interval_stop(State(s): State<AppState>) -> impl IntoResponse {
    // 실행 가드(interval_active)는 소유 태스크가 해제. 여기선 취소 신호만.
    s.interval_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, "stopped".to_string())
}

// ── 동영상 녹화 (MovieRecord) ────────────────────────────────────────────
async fn movie_start(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::shutter::movie_record_start(handle)).await {
        Ok(Ok(())) => (StatusCode::OK, "rec".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

async fn movie_stop(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::shutter::movie_record_stop(handle)).await {
        Ok(Ok(())) => (StatusCode::OK, "stop".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

async fn cancel_shooting(State(s): State<AppState>) -> impl IntoResponse {
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    match tokio::task::spawn_blocking(move || crsdk::shutter::cancel_shooting(handle)).await {
        Ok(Ok(())) => (StatusCode::OK, "cancelled".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── 촬영 미리보기: 마지막 PC 저장 이미지 반환 ────────────────────────────
async fn last_image(State(s): State<AppState>) -> Response {
    let path = match s.last_image.lock().await.clone() {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "no image").into_response(),
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let lp = path.to_lowercase();
            // 실제 확장자에 맞는 content-type(RAW를 image/jpeg로 라벨하던 버그 수정 — 브라우저가
            // ARW를 JPEG로 못 그려 깨짐). RAW/미지원은 octet-stream → UI onerror로 스킵.
            let ct = if lp.ends_with(".heif") || lp.ends_with(".heic") {
                "image/heif"
            } else if lp.ends_with(".jpg") || lp.ends_with(".jpeg") {
                "image/jpeg"
            } else {
                "application/octet-stream" // .arw 등 RAW
            };
            ([(header::CONTENT_TYPE, ct)], bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "read fail").into_response(),
    }
}

// ── 다중노출 (소프트웨어 — A7C엔 없는 기능) ──────────────────────────────
#[derive(Deserialize)]
struct MultiExpReq {
    count: Option<u32>,           // 합칠 장수(기본 3)
    mode: Option<String>,         // "average"|"lighten"|"add"(기본 average)
    shot_timeout_ms: Option<u64>, // 장당 JPEG 다운로드 대기(기본 12000; RAW 느림)
}

/// events_tx(JSON)에서 다음 JPEG download_complete 파일경로를 기다림. RAW 등 비-JPEG는 건너뜀.
async fn wait_jpeg_download(
    rx: &mut broadcast::Receiver<String>,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("다운로드 타임아웃".into());
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(msg)) => {
                let v: serde_json::Value = match serde_json::from_str(&msg) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("type").and_then(|t| t.as_str()) != Some("download_complete") {
                    continue;
                }
                if let Some(f) = v.get("filename").and_then(|f| f.as_str()) {
                    let lf = f.to_lowercase();
                    if lf.ends_with(".jpg") || lf.ends_with(".jpeg") {
                        return Ok(f.to_string());
                    }
                    // RAW(.arw) 등은 무시하고 JPEG 계속 대기
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return Err("다운로드 타임아웃".into()),
        }
    }
}

/// 단일실행 AtomicBool 가드: 드롭 시 반드시 false로 되돌린다. 핸들러 future가 정상/에러
/// 종료뿐 아니라 **클라이언트 연결 끊김으로 취소(드롭)**될 때도 플래그를 해제해, 취소
/// 라우트가 없는 인라인 작업이 "already running"으로 영구 잠기는 것을 막는다.
struct RunGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// N장 연속 촬영 → 다운로드된 JPEG들을 블렌드 합성 → 1장 저장. 진행/완료는 events SSE.
async fn multi_exposure(State(s): State<AppState>, Json(b): Json<MultiExpReq>) -> Response {
    use std::sync::atomic::Ordering;
    let count = b.count.unwrap_or(3).clamp(2, 30);
    let mode = composite::Blend::from_str(b.mode.as_deref().unwrap_or("average"));
    let shot_timeout = Duration::from_millis(b.shot_timeout_ms.unwrap_or(12000));

    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };
    if s
        .me_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "multi-exposure already running").into_response();
    }
    // 이 가드가 모든 종료 경로(정상/에러 return/클라 끊김 드롭)에서 me_active를 해제.
    let _me_guard = RunGuard(s.me_active.clone());
    let events = s.events_tx.clone();
    // 첫 셔터 전에 구독해야 다운로드 이벤트를 놓치지 않음.
    let mut ev_rx = s.events_tx.subscribe();

    let mut files: Vec<String> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let shot = tokio::task::spawn_blocking(move || capture_one(handle)).await;
        let shot = shot
            .map_err(|e| format!("task: {e}"))
            .and_then(|r| r.map_err(|e| format!("sdk: {e:?}")));
        if let Err(e) = shot {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("capture {}/{count}: {e}", i + 1))
                .into_response();
        }
        match wait_jpeg_download(&mut ev_rx, shot_timeout).await {
            Ok(f) => files.push(f),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("frame {}/{count}: {e} — JPEG 저장(STILL→PC, 파일형식 JPEG) 필요", i + 1),
                )
                    .into_response();
            }
        }
        let _ = events.send(format!(
            r#"{{"type":"multi_exposure","i":{},"n":{count}}}"#,
            i + 1
        ));
    }

    // 합성(CPU): 파일 읽기 + 블렌드 → spawn_blocking.
    let files2 = files.clone();
    let blended = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let mut jpegs = Vec::with_capacity(files2.len());
        for f in &files2 {
            jpegs.push(std::fs::read(f).map_err(|e| format!("read {f}: {e}"))?);
        }
        composite::blend_jpegs(&jpegs, mode)
    })
    .await;

    let out = match blended {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("blend: {e}")).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    };

    // 저장: save_path/ME_<epoch>.jpg → 미리보기(last_image)로도 노출.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let save_dir = s.save_path.lock().await.clone();
    let path = std::path::Path::new(&save_dir).join(format!("ME_{secs}.jpg"));
    if let Err(e) = tokio::fs::write(&path, &out).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("save: {e}")).into_response();
    }
    let path_str = path.to_string_lossy().to_string();
    *s.last_image.lock().await = Some(path_str.clone());
    let _ = events.send(format!(
        r#"{{"type":"multi_exposure_done","file":{}}}"#,
        serde_json::json!(path_str)
    ));
    Json(serde_json::json!({"file": path_str, "count": count})).into_response()
}

// ── AF 포인트 지정 (정규화 0~1 → x:0~639 y:0~479, (x<<16)|y device property) ──
// 좌표계/패킹은 공식 RemoteCli 샘플(execute_pos_xy)을 따름. 위치 지정엔 FocusArea가
// Flexible Spot이어야 하므로 좌표 설정 전에 Flexible_Spot_S로 전환한다.
#[derive(Deserialize)]
struct AfPoint { x: f64, y: f64, #[serde(default)] area: Option<u64> }

// AF 좌표 보정 — 바디마다 라이브뷰 좌표계/매핑이 다르다. 모델별 테이블로 키화한다.
// 좌표범위(x_max/y_max)는 SDK AF 그리드 기준 640×480 공통; y_cal만 바디별 실측이다.
struct AfCalib {
    x_max: u32,                   // X 좌표 최대 (0..=x_max)
    y_max: u32,                   // Y 좌표 최대 (선형 폴백 시 사용)
    y_cal: &'static [(f64, f64)], // (cmd_y, 실측 y_num) S커브 역보정표. 비면 선형.
}

// A7C 실측 (cmd_y, 카메라가 실제 놓은 y_num). 카메라가 cmd→실위치를 S커브로
// 매핑(중앙 압축)하므로, 클릭 ny를 박스 도달범위[28,297]에 선형 대응시키는 목표
// y_num을 역보간해 cmd_y를 구한다. FocusArea=M 기준 실측 (다른 크기도 근사 사용).
const A7C_Y_CAL: [(f64, f64); 5] =
    [(0.0, 28.0), (120.0, 66.0), (240.0, 162.0), (359.0, 256.0), (479.0, 297.0)];

/// 연결된 모델에 맞는 AF 보정. 미측정 바디는 선형 폴백.
fn af_calib(model: &str) -> AfCalib {
    if model.eq_ignore_ascii_case("ILCE-7C") {
        AfCalib { x_max: 639, y_max: 479, y_cal: &A7C_Y_CAL }
    } else {
        AfCalib { x_max: 639, y_max: 479, y_cal: &[] } // 미측정: 선형 매핑
    }
}

impl AfCalib {
    fn x(&self, nx: f64) -> u32 {
        (nx.clamp(0.0, 1.0) * self.x_max as f64).round() as u32
    }
    fn y(&self, ny: f64) -> u32 {
        let cal = self.y_cal;
        if cal.len() < 2 {
            return (ny.clamp(0.0, 1.0) * self.y_max as f64).round() as u32;
        }
        let (amin, amax) = (cal[0].1, cal[cal.len() - 1].1);
        let target = amin + ny.clamp(0.0, 1.0) * (amax - amin); // 도달범위에 선형 대응
        for w in cal.windows(2) {
            let (c0, a0) = w[0];
            let (c1, a1) = w[1];
            if target <= a1 {
                let t = (target - a0) / (a1 - a0);
                return (c0 + t * (c1 - c0)).round() as u32;
            }
        }
        cal[cal.len() - 1].0 as u32
    }
}

async fn af_point(State(s): State<AppState>, Json(b): Json<AfPoint>) -> impl IntoResponse {
    let (handle, model) = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => (c.0.device_handle(), c.1.clone()),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    let cal = af_calib(&model);
    let x = cal.x(b.x);
    let y = cal.y(b.y); // Y는 S커브 역보정 (모델별)
    let packed = ((x << 16) | y) as u64;
    // 위치 지정이 먹히는 영역만 통과: 존(0x02)·Flexible/Expand(0x04~08)·트래킹(0x12,0x14~1A). 그 외엔 S.
    let area = match b.area {
        Some(v @ (0x02 | 0x04..=0x08 | 0x12 | 0x14..=0x1A)) => v,
        _ => crsdk::properties::focus_area::FLEXIBLE_SPOT_S,
    };
    let r = tokio::task::spawn_blocking(move || {
        use crsdk::properties::{self, code};
        properties::set(handle, code::FOCUS_AREA, area)?;
        properties::set(handle, code::AF_AREA_POSITION, packed)
    })
    .await;
    match r {
        Ok(Ok(())) => (StatusCode::OK, "ok".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── 이벤트 스트림 (SSE) ──────────────────────────────────────────────────
// CameraEvent를 JSON 문자열로 변환 (lib에 serde 의존성 추가하지 않기 위해 수동).
fn event_json(ev: &CameraEvent) -> String {
    match ev {
        CameraEvent::Connected { version } => {
            format!(r#"{{"type":"connected","version":{version}}}"#)
        }
        CameraEvent::Disconnected { error } => {
            format!(r#"{{"type":"disconnected","error":{error}}}"#)
        }
        CameraEvent::PropertyChanged => r#"{"type":"property_changed"}"#.to_string(),
        CameraEvent::LvPropertyChanged => r#"{"type":"lv_property_changed"}"#.to_string(),
        CameraEvent::Warning(code) => format!(r#"{{"type":"warning","code":{code}}}"#),
        CameraEvent::WarningExt { code, p1, p2, p3 } => {
            format!(r#"{{"type":"warning_ext","code":{code},"p1":{p1},"p2":{p2},"p3":{p3}}}"#)
        }
        CameraEvent::Error(code) => format!(r#"{{"type":"error","code":{code}}}"#),
        CameraEvent::DownloadComplete { filename, kind } => {
            // 파일명에 특수문자가 있을 수 있어 serde_json으로 안전 이스케이프.
            let f = serde_json::to_string(filename).unwrap_or_else(|_| "\"\"".to_string());
            format!(r#"{{"type":"download_complete","filename":{f},"kind":{kind}}}"#)
        }
    }
}

async fn events(State(s): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = s.events_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|r| match r {
        Ok(json) => Some(Ok(Event::default().data(json))),
        Err(_) => None, // lagged — 건너뜀
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── LiveView MJPEG 스트림 (다중 클라이언트 fan-out) ──────────────────────
// multipart/x-mixed-replace — 브라우저 <img>가 디코딩.
// 카메라당 단일 프로듀서(spawn_blocking)가 LiveViewStream을 소유하며 16ms마다 프레임을
// fetch → broadcast로 모든 구독자에 fan-out. 각 /lv 요청은 구독만 하므로 SDK 라이브뷰
// 접근은 항상 하나뿐(다중 클라이언트가 버퍼를 다투지 않음). 프로듀서는 첫 시청자에 시작,
// 카메라 해제(fetch 에러) 시 종료 → lv_running=false → 재연결 후 다음 /lv가 재시작.
// (브라우저가 닫혀도 연결 중엔 계속 가동: broadcast는 무손실·비블로킹이라 hyper가 끊긴
//  Receiver를 즉시 드롭하지 않아 시청자-0 종료를 신뢰할 수 없음 → 상시 가동으로 단순화.)
fn lv_producer(handle: i64, lv_tx: broadcast::Sender<Arc<Vec<u8>>>, running: Arc<std::sync::Mutex<bool>>) {
    // 연결 직후 카메라가 LiveView를 준비하는 데 시간이 필요 → 최대 4s 재시도
    let mut lv = None;
    for _ in 0..20 {
        match LiveViewStream::new(handle) {
            Ok(s) => { lv = Some(s); break; }
            Err(SdkError::LiveViewUnavailable) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => { *running.lock().unwrap_or_else(|e| e.into_inner()) = false; return; }
        }
    }
    let lv = match lv {
        Some(s) => s,
        None => {
            tracing::warn!("lv: LiveViewStream unavailable after retries");
            *running.lock().unwrap_or_else(|e| e.into_inner()) = false;
            return;
        }
    };
    tracing::info!("lv: producer started");

    let mut sent: u64 = 0;
    loop {
        match lv.fetch_frame() {
            Ok(frame) if !frame.is_empty() => {
                let _ = lv_tx.send(Arc::new(frame)); // 구독자 0이어도 무손실 송신(스킵)
                sent += 1;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(16)), // 아직 새 프레임 없음
            Err(e) => {
                tracing::warn!("lv: fetch error after {sent} frames: {e:?}");
                *running.lock().unwrap_or_else(|e| e.into_inner()) = false;
                break;
            }
        }
    }
    tracing::info!("lv: producer ended ({sent} frames)");
    // lv drops here → liveview_free_block
}

async fn liveview(State(s): State<AppState>) -> Response {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => {
                return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response()
            }
        }
    };

    // 먼저 구독 → receiver_count ≥ 1 보장(프로듀서가 곧바로 종료하지 않도록).
    let rx = s.lv_tx.subscribe();

    // 프로듀서 미가동이면 시작 (단일 프로듀서). 락으로 종료 판정과 직렬화.
    {
        let mut running = s.lv_running.lock().unwrap_or_else(|e| e.into_inner());
        if !*running {
            *running = true;
            let lv_tx = s.lv_tx.clone();
            let running_c = s.lv_running.clone();
            tokio::task::spawn_blocking(move || lv_producer(handle, lv_tx, running_c));
        }
    }

    let stream = BroadcastStream::new(rx).filter_map(|r| match r {
        Ok(frame) => {
            let mut buf = Vec::with_capacity(frame.len() + 80);
            buf.extend_from_slice(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: ");
            buf.extend_from_slice(frame.len().to_string().as_bytes());
            buf.extend_from_slice(b"\r\n\r\n");
            buf.extend_from_slice(&frame);
            buf.extend_from_slice(b"\r\n");
            Some(Ok::<_, std::io::Error>(buf))
        }
        Err(_) => None, // lagged(느린 클라) → 해당 프레임 스킵
    });

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        )
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap()
}

// ── 속성 읽기/쓰기 ───────────────────────────────────────────────────────
#[derive(Serialize)]
struct PropView {
    value: u64,
    editable: bool,
    allowed: Vec<u64>,
    value_type: u32, // CrDataType (Range 0x4000 등) — 프론트가 allowed 해석에 사용
}

#[derive(Serialize)]
struct PropertiesDto {
    focus_mode: Option<PropView>,
    save_dest: Option<PropView>,
    exposure_mode: Option<PropView>,
    iso: Option<PropView>,
    shutter_speed: Option<PropView>,
    f_number: Option<PropView>,
    ev: Option<PropView>,
    white_balance: Option<PropView>,
    drive_mode: Option<PropView>,
    metering: Option<PropView>,
    flash_mode: Option<PropView>,
    file_type: Option<PropView>,
    recording_state: Option<PropView>,
    shutter_type: Option<PropView>,
    silent_mode: Option<PropView>,
    battery: Option<PropView>,
    remain_shots: Option<PropView>,
    jpeg_quality: Option<PropView>,
    picture_profile: Option<PropView>,
    color_temp: Option<PropView>,
    focus_area: Option<PropView>,
    focus_indication: Option<PropView>,
}

async fn properties(State(s): State<AppState>) -> Response {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response(),
        }
    };

    match tokio::task::spawn_blocking(move || crsdk::properties::get_all(handle)).await {
        Ok(Ok(props)) => {
            let find = |code: u32| {
                props.iter().find(|p| p.code == code).map(|p| PropView {
                    value: p.current,
                    editable: p.editable,
                    allowed: p.allowed.clone(),
                    value_type: p.value_type,
                })
            };
            use crsdk::properties::code;
            Json(PropertiesDto {
                focus_mode: find(code::FOCUS_MODE),
                save_dest: find(code::STILL_IMAGE_STORE_DESTINATION),
                exposure_mode: find(code::EXPOSURE_PROGRAM_MODE),
                iso: find(code::ISO_SENSITIVITY),
                shutter_speed: find(code::SHUTTER_SPEED),
                f_number: find(code::F_NUMBER),
                ev: find(code::EXPOSURE_BIAS_COMPENSATION),
                white_balance: find(code::WHITE_BALANCE),
                drive_mode: find(code::DRIVE_MODE),
                metering: find(code::METERING_MODE),
                flash_mode: find(code::FLASH_MODE),
                file_type: find(code::FILE_TYPE),
                recording_state: find(code::RECORDING_STATE),
                shutter_type: find(code::SHUTTER_TYPE),
                silent_mode: find(code::SILENT_MODE),
                battery: find(code::BATTERY_REMAIN),
                remain_shots: find(code::MEDIA_SLOT1_REMAINING_NUMBER),
                jpeg_quality: find(code::STILL_IMAGE_QUALITY),
                picture_profile: find(code::PICTURE_PROFILE),
                color_temp: find(code::COLOR_TEMP),
                focus_area: find(code::FOCUS_AREA),
                focus_indication: find(code::FOCUS_INDICATION),
            })
            .into_response()
        }
        Ok(Err(e)) => {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct SetProp {
    code: u32,
    value: u64,
}

async fn set_property(
    State(s): State<AppState>,
    Json(body): Json<SetProp>,
) -> impl IntoResponse {
    let handle = {
        let guard = s.camera.lock().await;
        match &*guard {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };

    match tokio::task::spawn_blocking(move || crsdk::properties::set(handle, body.code, body.value))
        .await
    {
        Ok(Ok(())) => (StatusCode::OK, "set".to_string()),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("sdk: {e:?}")),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("task: {e}")),
    }
}

// ── Entry ──────────────────────────────────────────────────────────────

/// UI 정적파일 디렉토리. 우선순위: ① 실행파일 옆 `web/`(폴더형 배포) →
/// ② `../Resources/web`(.app 번들: Contents/MacOS/ → Contents/Resources/web) →
/// ③ 빌드 디렉토리의 `web/`(개발).
fn web_dir() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [dir.join("web"), dir.join("../Resources/web")] {
                if cand.is_dir() {
                    return cand;
                }
            }
        }
    }
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/web"))
}

/// 맥의 LAN IPv4 (폰 접속용). UDP connect 트릭 — 실제 패킷은 보내지 않고
/// OS가 외부로 나갈 때 쓰는 인터페이스 주소를 읽는다. 오프라인/경로 없음이면 None.
fn lan_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    sock.local_addr().ok().map(|a| a.ip().to_string())
}

/// 서버 정보 — 버전 + 폰 접속 URL(LAN). UI가 폰 접속 안내에 사용.
async fn server_info() -> Json<serde_json::Value> {
    let lan = lan_ip().map(|ip| format!("http://{ip}:8080/web/index.html"));
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "lan_url": lan,
    }))
}

/// 웹 UI에서 서버 종료 (LSUIElement 에이전트 앱이라 Dock으로 종료 불가 → Quit 버튼용).
/// 카메라를 먼저 해제(Drop)한 뒤 잠시 후 프로세스 종료.
async fn quit(State(s): State<AppState>) -> impl IntoResponse {
    tracing::info!("quit requested via API");
    *s.camera.lock().await = None; // Camera Drop(disconnect/release) 동기 실행
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    "bye"
}

/// 자기 자신을 제외한, 같은 이름(crsdk_server)의 실행 중 인스턴스 PID들. (unix: pgrep)
#[cfg(unix)]
fn other_instance_pids() -> Vec<u32> {
    let me = std::process::id();
    std::process::Command::new("pgrep")
        .args(["-x", "crsdk_server"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .filter_map(|s| s.parse::<u32>().ok())
                .filter(|&p| p != me)
                .collect()
        })
        .unwrap_or_default()
}

/// 실행 시 기존 인스턴스를 종료한다. 중복 인스턴스가 카메라를 두고 다투면
/// ConnectTimeout(0x8208)이 나므로, 일반 사용자가 앱을 여러 번 켜도 단일 인스턴스가
/// 되도록 한다. 먼저 SIGTERM(기존 인스턴스의 graceful shutdown → 카메라 해제)을 보내고,
/// 종료를 기다린 뒤 남으면 SIGKILL.
#[cfg(unix)]
fn terminate_other_instances() {
    let pids = other_instance_pids();
    if pids.is_empty() {
        return;
    }
    for p in &pids {
        tracing::info!("existing instance pid {p} found — terminating (single-instance)");
        let _ = std::process::Command::new("kill").arg(p.to_string()).status();
    }
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(100)); // 최대 ~3s graceful 대기
        if other_instance_pids().is_empty() {
            return;
        }
    }
    for p in other_instance_pids() {
        let _ = std::process::Command::new("kill").args(["-9", &p.to_string()]).status();
    }
    std::thread::sleep(Duration::from_millis(300)); // 포트/카메라 해제 여유
}

/// Windows: named mutex로 2번째 인스턴스를 막는다(기존 인스턴스가 카메라를 유지).
/// unix처럼 기존 인스턴스를 force-kill하면 카메라 PC Remote 세션이 매달려
/// ConnectTimeout이 나므로, Windows에선 "두 번째가 그냥 종료" 하는 편이 안전하다.
/// (뮤텍스 핸들은 CloseHandle 하지 않아 프로세스 수명 동안 유지되고, 종료 시 OS가 해제)
#[cfg(windows)]
fn terminate_other_instances() {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    extern "system" {
        fn CreateMutexW(attr: *const core::ffi::c_void, owner: i32, name: *const u16)
            -> *mut core::ffi::c_void;
        fn GetLastError() -> u32;
    }
    let name: Vec<u16> = OsStr::new("TetherMoon_crsdk_server_singleton")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return; // 뮤텍스 생성 실패 — 막지 않고 진행
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        tracing::error!(
            "another TetherMoon instance is already running — exiting (single-instance)"
        );
        std::process::exit(0);
    }
    // handle 의도적으로 닫지 않음(프로세스 수명 = 뮤텍스 수명)
}

/// 그 외 OS: no-op.
#[cfg(not(any(unix, windows)))]
fn terminate_other_instances() {}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // 기존 인스턴스 종료 → 단일 인스턴스 보장(중복 시 카메라 ConnectTimeout 방지).
    tokio::task::spawn_blocking(terminate_other_instances)
        .await
        .ok();

    // USB 간섭 억제 시작 — main 수명 동안 유지 (graceful shutdown 시 Drop이 회수)
    let _killer = UsbInterferenceSuppressor::start();
    // 억제기가 None인 건 macOS에서만 "실패"다. 다른 OS는 ptpcamerad가 없어 no-op(정상).
    #[cfg(target_os = "macos")]
    if _killer.is_none() {
        tracing::warn!("ptpcamerad suppressor failed to start — connect may time out");
    }

    let (events_tx, _) = broadcast::channel::<String>(64);
    let state = AppState {
        camera: Arc::new(Mutex::new(None)),
        save_path: Arc::new(Mutex::new(String::new())),
        events_tx,
        last_image: Arc::new(Mutex::new(None)),
        bulb_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        interval_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        interval_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        lv_tx: broadcast::channel::<Arc<Vec<u8>>>(4).0,
        lv_running: Arc::new(std::sync::Mutex::new(false)),
        af_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        af_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        af_target: Arc::new(std::sync::Mutex::new((0.5, 0.5, 0.25, 0.25))),
        me_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        connecting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        #[cfg(feature = "detector")]
        detector: load_detector(),
    };

    // 자동 (재)연결 루프: 미연결 상태면 3초마다 connect 시도.
    // connect_core는 이미 연결돼 있으면 즉시 Ok 반환하므로 폴링이 안전.
    // 카메라 절전/케이블 흔들림으로 끊겨도 다시 붙는다.
    let s2 = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        loop {
            if s2.camera.lock().await.is_none() {
                let _ = connect_core(&s2).await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    // shutdown handler가 카메라 explicitly disconnect 하도록 state 미리 clone
    let shutdown_state = state.clone();

    let app = Router::new()
        .route("/", get(root))
        .route("/api/status", get(status))
        .route("/api/serverinfo", get(server_info))
        .route("/api/quit", post(quit))
        .route("/api/connect", post(connect))
        .route("/api/disconnect", post(disconnect))
        .route("/api/shutter", post(shutter))
        .route("/api/half/down", post(half_down))
        .route("/api/half/up", post(half_up))
        .route("/api/bulb", post(bulb))
        .route("/api/interval", post(interval_start))
        .route("/api/interval/stop", post(interval_stop))
        .route("/api/_debug/level", get(level_info))
        .route("/api/_debug/afframe", get(af_frame_info))
        .route("/api/shutter/down", post(shutter_down))
        .route("/api/shutter/up", post(shutter_up))
        .route("/api/movie/start", post(movie_start))
        .route("/api/movie/stop", post(movie_stop))
        .route("/api/cancel", post(cancel_shooting))
        .route("/api/last_image", get(last_image))
        .route("/api/multi_exposure", post(multi_exposure))
        .route("/api/af_point", post(af_point))
        .route("/api/properties", get(properties))
        .route("/api/property", post(set_property))
        .route("/api/savepath", post(set_save_path))
        .route("/api/savepath/browse", post(browse_save_path))
        .route("/api/focus_nearfar", post(focus_near_far))
        .route("/api/focus_nearfar/info", get(focus_nearfar_info))
        .route("/api/sw_autofocus", post(sw_autofocus))
        .route("/api/sw_autofocus/continuous", post(sw_autofocus_continuous))
        .route("/api/sw_autofocus/cancel", post(sw_autofocus_cancel))
        .route("/api/sw_autofocus/retarget", post(sw_autofocus_retarget))
        .route("/api/capabilities", get(capabilities))
        .route("/api/_debug/sharpness", get(debug_sharpness))
        .route("/api/_debug/codes", get(debug_all_codes))
        .route("/api/_debug/enum", get(debug_enum))
        .route("/events", get(events))
        .route("/lv", get(liveview))
        .nest_service("/web", ServeDir::new(web_dir()));
    #[cfg(feature = "detector")]
    let app = app.route("/api/detect", post(detect)); // RT-DETR 검출(추적AF, 옵셔널)
    let app = app.with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("bind 0.0.0.0:8080");
    tracing::info!("crsdk_server listening on http://0.0.0.0:8080");
    tracing::info!("  on this PC  : http://localhost:8080/web/index.html");
    if let Some(ip) = lan_ip() {
        tracing::info!("  on a phone  : http://{ip}:8080/web/index.html  (same Wi-Fi)");
    }

    // 실행 시 기본 브라우저로 UI를 띄운다(더블클릭 UX). 개발/테스트 중 매 재시작마다
    // 탭이 열리는 걸 막으려면 CRSDK_NO_BROWSER=1.
    if std::env::var_os("CRSDK_NO_BROWSER").is_none() {
        const URL: &str = "http://localhost:8080/web/index.html";
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(URL).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", URL]).spawn();
        #[cfg(all(unix, not(target_os = "macos")))]
        let _ = std::process::Command::new("xdg-open").arg(URL).spawn(); // Linux 등
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_state))
        .await
        .expect("axum serve");
    tracing::info!("crsdk_server stopped");
}

// ── Graceful shutdown ──────────────────────────────────────────────────
// Ctrl+C(SIGINT) 또는 SIGTERM(pkill 기본)이 들어오면 카메라를 명시적으로 None으로
// 만들어 Camera::Drop을 즉시 실행시킨다. Drop 체인: deactivate_callback →
// disconnect → release_device. 이게 없으면 카메라에 세션이 남아 재연결 시
// CrError_Connect_FailBusy(0x820B)가 난다.
//
// 주의: /lv(MJPEG)·/events(SSE)는 무한 스트리밍 연결이라 자발적으로 닫히지 않는다.
// 따라서 with_graceful_shutdown의 연결 드레인이 영원히 끝나지 않아 프로세스가 좀비로
// 남는다(SIGTERM에도 안 죽음 → 중복 인스턴스 → ConnectTimeout). 카메라 Drop은 아래에서
// 수동으로 끝내므로, 짧은 유예 후 워치독이 강제 종료해 이 행을 끊는다. (이 시점엔 중요한
// 정리가 이미 끝났으므로 process::exit가 안전하다.)
async fn shutdown_signal(state: AppState) {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await; // 설치 실패 시 이 분기는 영원히 대기
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => { s.recv().await; }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received — disconnecting camera");
    *state.camera.lock().await = None; // Camera Drop(disconnect/release)을 동기 실행

    // 스트리밍 연결이 드레인되지 않아 graceful shutdown이 무한 대기하는 것을 방지.
    // 카메라 정리는 위에서 끝났으니, 유예 후 강제 종료한다. 정상 연결은 그 사이 닫히고
    // serve()가 먼저 반환하면 main 종료로 프로세스가 정상 종료(이 태스크는 함께 사라짐).
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        tracing::warn!("forcing exit — streaming connections (/lv, /events) did not drain");
        std::process::exit(0);
    });
}
