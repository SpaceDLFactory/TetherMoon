// crsdk_server/src/lifecycle.rs — main.rs에서 기능 계통별로 분리 (동작 불변)
use std::time::Duration;


use axum::{
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Json,
    },
};
use crsdk::{
    connection::ConnectMode, Camera, CameraEnumerator,
};
use serde::Serialize;
use crate::state::*;
use crate::stream::event_json;

// ── macOS USB 간섭 억제 (ptpcamerad) ────────────────────────────────────
// launchd가 ~100ms마다 ptpcamerad를 재시작하며 USB PTP 인터페이스를 선점한다.
// 일회성 kill로는 connect 핸드셰이크(최대 10s) 윈도우를 못 버틴다.
// 50ms 주기 kill loop를 백그라운드 자식 프로세스로 돌리고, Drop이 회수한다.
// (crsdk_example의 UsbInterferenceSuppressor와 동일 — 추후 lib로 통합 가능)
// ptpcamerad는 macOS 전용 이슈. 다른 OS는 USB 선점 메커니즘이 달라(Windows: WinUSB/libusb
// 드라이버) 여기선 no-op. (cross-platform 구조화 — Windows 측 처리는 추후 결정)
#[allow(dead_code)]
pub(crate) struct UsbInterferenceSuppressor {
    #[cfg(target_os = "macos")]
    child: std::process::Child,
}

impl UsbInterferenceSuppressor {
    #[cfg(target_os = "macos")]
    pub(crate) fn start() -> Option<Self> {
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
        // 재시작 완화: 직전 서버 종료~신규 억제 사이 틈에 ptpcamerad가 카메라 USB를 잡을 수
        // 있다. 킬 루프가 그놈을 정리하고 USB 인터페이스가 풀릴 짧은 시간을 준 뒤 진행 —
        // 첫 연결 시도가 '선점됨'으로 실패해 replug가 필요해지는 빈도를 줄인다(완전 제거 아님).
        std::thread::sleep(Duration::from_millis(400));
        Some(Self { child })
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn start() -> Option<Self> {
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

// ── /api/status DTO ─────────────────────────────────────────────────────
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Status {
    Connected {
        model: String,
        handle: String,
        save_path: String,
        lens_model: String,
    },
    Disconnected,
}

// ── Handlers ────────────────────────────────────────────────────────────

pub(crate) async fn status(State(s): State<AppState>) -> Json<Status> {
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
pub(crate) async fn connect_core(s: &AppState) -> Result<(), String> {
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

pub(crate) async fn connect(State(s): State<AppState>) -> impl IntoResponse {
    match connect_core(&s).await {
        Ok(()) => (StatusCode::OK, "connected".to_string()),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

/// 카메라 해제(Drop: deactivate_callback → disconnect → release)를 blocking 풀에서 실행.
/// Camera Drop은 실제 USB 왕복이라 수 초 걸릴 수 있음 → tokio 워커/카메라 락을 붙잡은 채
/// 동기 실행하면 다른 핸들러가 락에서 대기하고 워커가 스톨한다. take() 후 spawn_blocking에서 drop.
pub(crate) async fn release_camera(camera: &tokio::sync::Mutex<Option<CameraCell>>) {
    let cell = camera.lock().await.take();
    if cell.is_some() {
        let _ = tokio::task::spawn_blocking(move || drop(cell)).await;
    }
}

pub(crate) async fn disconnect(State(s): State<AppState>) -> impl IntoResponse {
    release_camera(&s.camera).await;
    tracing::info!("camera disconnected");
    (StatusCode::OK, "disconnected")
}

/// 웹 UI에서 서버 종료 (LSUIElement 에이전트 앱이라 Dock으로 종료 불가 → Quit 버튼용).
/// 카메라를 먼저 해제(Drop)한 뒤 잠시 후 프로세스 종료.
pub(crate) async fn quit(State(s): State<AppState>) -> impl IntoResponse {
    tracing::info!("quit requested via API");
    release_camera(&s.camera).await; // Camera Drop(disconnect/release)을 blocking 풀에서
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    "bye"
}

/// 자기 자신을 제외한, 같은 이름(crsdk_server)의 실행 중 인스턴스 PID들. (unix: pgrep)
#[cfg(unix)]
pub(crate) fn other_instance_pids() -> Vec<u32> {
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
pub(crate) fn terminate_other_instances() {
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
pub(crate) fn terminate_other_instances() {
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
pub(crate) fn terminate_other_instances() {}

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
pub(crate) async fn shutdown_signal(state: AppState) {
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
    release_camera(&state.camera).await; // Camera Drop(disconnect/release)을 blocking 풀에서

    // 스트리밍 연결이 드레인되지 않아 graceful shutdown이 무한 대기하는 것을 방지.
    // 카메라 정리는 위에서 끝났으니, 유예 후 강제 종료한다. 정상 연결은 그 사이 닫히고
    // serve()가 먼저 반환하면 main 종료로 프로세스가 정상 종료(이 태스크는 함께 사라짐).
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        tracing::warn!("forcing exit — streaming connections (/lv, /events) did not drain");
        std::process::exit(0);
    });
}
