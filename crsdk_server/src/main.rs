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

// ── App state ──────────────────────────────────────────────────────────
#[derive(Clone)]
struct AppState {
    camera: Arc<Mutex<Option<CameraCell>>>,
    save_path: Arc<Mutex<String>>,
    events_tx: broadcast::Sender<String>, // JSON으로 직렬화된 CameraEvent fan-out
    last_image: Arc<Mutex<Option<String>>>, // 마지막 PC 저장 파일 경로 (미리보기)
    bulb_active: Arc<std::sync::atomic::AtomicBool>, // 벌브 타이머 노출 진행중 (중복 트리거 방지)
    interval_active: Arc<std::sync::atomic::AtomicBool>, // 인터벌 촬영 진행중 (취소 신호 겸용)
    lv_tx: broadcast::Sender<Arc<Vec<u8>>>, // LiveView 프레임 fan-out (다중 클라이언트)
    lv_running: Arc<std::sync::Mutex<bool>>, // LiveView 프로듀서 가동 여부 (시작/종료 race 방지용 락)
    af_active: Arc<std::sync::atomic::AtomicBool>, // SW-AF 스윕 진행중 (단일 실행 + 취소 신호 겸용)
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
    if s.camera.lock().await.is_some() {
        return Ok(()); // 이미 연결됨
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
            _ => break,
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

/// 현재 위치에서 Far(+step)로 n번 이동하며 n+1 지점 측정. (scores, best_index) 반환.
/// active=false면 즉시 중단(부분 scores). 끝나면 focus는 마지막 측정 지점(far 끝).
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
    active: &std::sync::atomic::AtomicBool,
    events: &broadcast::Sender<String>,
    phase: &str,
) -> (Vec<f64>, usize) {
    use std::sync::atomic::Ordering;
    let mut scores = Vec::with_capacity(n as usize + 1);
    let mut best = -1.0f64;
    let mut best_k = 0usize;
    for i in 0..=n as usize {
        if !active.load(Ordering::SeqCst) {
            break;
        }
        let sc = af_grab(rx, cx, cy, roi_w, roi_h, frames).await.unwrap_or(0.0);
        if sc > best {
            best = sc;
            best_k = i;
        }
        scores.push(sc);
        // 진행률을 SSE로 흘려 UI가 실시간 표시.
        let _ = events.send(format!(
            r#"{{"type":"af_progress","phase":"{phase}","i":{i},"n":{n},"score":{sc:.0}}}"#
        ));
        if i < n as usize {
            af_drive(handle, step).await;
            tokio::time::sleep(settle).await;
        }
    }
    (scores, best_k)
}

/// Near 방향(-)으로 step을 times번 구동 후 settle.
async fn af_move_near(handle: i64, step: i32, times: usize, settle: Duration) {
    for _ in 0..times {
        af_drive(handle, -step.abs()).await;
    }
    if times > 0 {
        tokio::time::sleep(settle).await;
    }
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
    fine_step: Option<i32>,  // fine NearFar 스텝(기본 1=granularity)
    fine_count: Option<u32>, // fine 스윕 지점 수(기본 8; coarse best 주변 ±fine_count/2)
    settle_ms: Option<u64>,  // 구동 후 안정 대기(기본 250)
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
struct SwAfParams {
    cx: f64,
    cy: f64,
    roi_w: f64,
    roi_h: f64,
    step: i32,
    n: u32,
    fine_step: i32,
    fine_n: u32,
    settle: Duration,
    frames: u32,
}

impl SwAfParams {
    fn from_req(b: &SwAfReq) -> Self {
        let step = b.step.unwrap_or(5).abs().max(1);
        Self {
            cx: b.x.unwrap_or(0.5),
            cy: b.y.unwrap_or(0.5),
            roi_w: b.roi_w.or(b.roi).unwrap_or(0.25),
            roi_h: b.roi_h.or(b.roi).unwrap_or(0.25),
            step,
            n: b.count.unwrap_or(24).clamp(4, 200),
            fine_step: b.fine_step.unwrap_or(1).abs().clamp(1, step),
            fine_n: b.fine_count.unwrap_or(8).clamp(2, 40),
            settle: Duration::from_millis(b.settle_ms.unwrap_or(250)),
            frames: b.frames.unwrap_or(2).clamp(1, 5),
        }
    }
}

/// 합초 1회(coarse→fine + 백래시 보정). (coarse_scores, ck, fine_scores, fk, best_score) 반환.
/// active=false면 coarse 후 조기 종료(이미 coarse best로 복귀). 진행률은 events로 emit.
async fn swaf_lock(
    handle: i64,
    rx: &mut broadcast::Receiver<Arc<Vec<u8>>>,
    p: &SwAfParams,
    active: &std::sync::atomic::AtomicBool,
    events: &broadcast::Sender<String>,
) -> (Vec<f64>, usize, Vec<f64>, usize, f64) {
    use std::sync::atomic::Ordering;
    // PHASE 1 coarse
    af_move_near(handle, p.step, (p.n / 2) as usize, p.settle).await;
    let (coarse_scores, ck) = af_phase(
        handle, rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames, p.settle, p.step, p.n, active, events, "coarse",
    )
    .await;
    let coarse_end = coarse_scores.len().saturating_sub(1);
    af_move_near(handle, p.step, coarse_end.saturating_sub(ck), p.settle).await;
    if !active.load(Ordering::SeqCst) {
        let bs = coarse_scores.get(ck).copied().unwrap_or(0.0);
        return (coarse_scores, ck, Vec::new(), 0, bs);
    }
    // PHASE 2 fine — fine 범위가 coarse 한 격자(±step)를 fine_step 단위로 덮도록 보장.
    // (안 그러면 coarse 정점이 fine 범위 밖이라 fine이 더 나쁜 위치에 멈춤; 실측 발견)
    let fine_n = p
        .fine_n
        .max(2 * (p.step as u32).div_ceil(p.fine_step.max(1) as u32) + 2);
    af_move_near(handle, p.fine_step, (fine_n / 2) as usize, p.settle).await;
    let (fine_scores, fk) = af_phase(
        handle, rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames, p.settle, p.fine_step, fine_n, active,
        events, "fine",
    )
    .await;
    // fine best로 복귀 + 백래시 보정(최종 접근 한 방향=Far)
    let backlash = 2usize;
    let fine_end = fine_scores.len().saturating_sub(1);
    af_move_near(handle, p.fine_step, fine_end.saturating_sub(fk) + backlash, p.settle).await;
    for _ in 0..backlash {
        af_drive(handle, p.fine_step).await;
    }
    // 최종 위치에서 라플라시안(행렬연산)을 한 번 더 측정 → 백래시 보정 이동 후 실제 도달한
    // 샤프니스 정상값. 스윕 중 fk 점수는 측정 당시 위치 값이라 복귀 후와 다를 수 있어 재측정.
    tokio::time::sleep(p.settle).await;
    let final_score = af_grab(rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames)
        .await
        .unwrap_or_else(|| fine_scores.get(fk).copied().unwrap_or(0.0));
    (coarse_scores, ck, fine_scores, fk, final_score)
}

async fn sw_autofocus(State(s): State<AppState>, Json(b): Json<SwAfReq>) -> Response {
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
    let p = SwAfParams::from_req(&b);
    let active = s.af_active.clone();
    let events = s.events_tx.clone();
    let mut rx = s.lv_tx.subscribe();

    // 라이브뷰 가동 확인: 첫 프레임이 안 오면 중단.
    if af_grab(&mut rx, p.cx, p.cy, p.roi_w, p.roi_h, 1).await.is_none() {
        active.store(false, Ordering::SeqCst);
        return (StatusCode::PRECONDITION_REQUIRED, "live view not running").into_response();
    }

    let (coarse_scores, ck, fine_scores, fk, best_score) =
        swaf_lock(handle, &mut rx, &p, &active, &events).await;
    active.store(false, Ordering::SeqCst);

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
/// 떨어지면(피사체 이동/카메라 흔들림) 재합초. /cancel(af_active=false)로 정지.
/// 즉시 "started" 반환하고 백그라운드 진행, 상태는 /events SSE(af_continuous).
async fn sw_autofocus_continuous(State(s): State<AppState>, Json(b): Json<SwAfReq>) -> Response {
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
    let p = SwAfParams::from_req(&b);
    let threshold = b.threshold.unwrap_or(0.7).clamp(0.3, 0.95);
    let check = Duration::from_millis(b.check_ms.unwrap_or(600).clamp(150, 5000));
    let active = s.af_active.clone();
    let events = s.events_tx.clone();
    let lv_tx = s.lv_tx.clone();

    tokio::spawn(async move {
        let mut rx = lv_tx.subscribe();
        if af_grab(&mut rx, p.cx, p.cy, p.roi_w, p.roi_h, 1).await.is_none() {
            let _ = events.send(
                r#"{"type":"af_continuous","state":"error","reason":"no_liveview"}"#.to_string(),
            );
            active.store(false, Ordering::SeqCst);
            return;
        }
        // 초기 합초
        let (_, _, _, _, mut baseline) = swaf_lock(handle, &mut rx, &p, &active, &events).await;
        let _ = events.send(format!(
            r#"{{"type":"af_continuous","state":"locked","score":{baseline:.0}}}"#
        ));
        // 모니터 루프
        while active.load(Ordering::SeqCst) {
            tokio::time::sleep(check).await;
            if !active.load(Ordering::SeqCst) {
                break;
            }
            let cur = af_grab(&mut rx, p.cx, p.cy, p.roi_w, p.roi_h, p.frames)
                .await
                .unwrap_or(0.0);
            if baseline > 0.0 && cur < baseline * threshold {
                let _ = events.send(format!(
                    r#"{{"type":"af_continuous","state":"refocus","score":{cur:.0}}}"#
                ));
                let (_, _, _, _, nb) = swaf_lock(handle, &mut rx, &p, &active, &events).await;
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
        active.store(false, Ordering::SeqCst);
        let _ = events.send(r#"{"type":"af_continuous","state":"stopped"}"#.to_string());
        tracing::info!("sw-af continuous: stopped");
    });
    (StatusCode::OK, "continuous started").into_response()
}

async fn sw_autofocus_cancel(State(s): State<AppState>) -> impl IntoResponse {
    s.af_active
        .store(false, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, "cancel")
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
// interval_active를 취소 신호로 겸용. 대기는 1초 단위로 쪼개 /stop에 ~1s 내 반응.
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
    let active = s.interval_active.clone();
    tokio::spawn(async move {
        for i in 0..count {
            if !active.load(Ordering::SeqCst) { break; } // 취소
            match tokio::task::spawn_blocking(move || capture_one(handle)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("interval shot {i} failed: {e:?}"),
                Err(e) => tracing::warn!("interval shot {i} join: {e}"),
            }
            if i + 1 < count {
                for _ in 0..interval {
                    if !active.load(Ordering::SeqCst) { break; }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
        active.store(false, Ordering::SeqCst);
        tracing::info!("interval done");
    });
    (StatusCode::OK, format!("interval {count}x@{interval}s"))
}

async fn interval_stop(State(s): State<AppState>) -> impl IntoResponse {
    s.interval_active.store(false, std::sync::atomic::Ordering::SeqCst);
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
            Err(_) => { *running.lock().unwrap() = false; return; }
        }
    }
    let lv = match lv {
        Some(s) => s,
        None => {
            tracing::warn!("lv: LiveViewStream unavailable after retries");
            *running.lock().unwrap() = false;
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
                *running.lock().unwrap() = false;
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
        let mut running = s.lv_running.lock().unwrap();
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
        lv_tx: broadcast::channel::<Arc<Vec<u8>>>(4).0,
        lv_running: Arc::new(std::sync::Mutex::new(false)),
        af_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        .route("/api/capabilities", get(capabilities))
        .route("/api/_debug/sharpness", get(debug_sharpness))
        .route("/api/_debug/codes", get(debug_all_codes))
        .route("/api/_debug/enum", get(debug_enum))
        .route("/events", get(events))
        .route("/lv", get(liveview))
        .nest_service("/web", ServeDir::new(web_dir()))
        .with_state(state);

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
