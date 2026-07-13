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

use std::sync::Arc;
use std::time::Duration;


use axum::{
    response::{
        Json, Redirect,
    },
    routing::{get, post},
    Router,
};
use tokio::sync::{broadcast, Mutex};
use tower_http::services::ServeDir;

mod autofocus; // SW-AF: 라이브뷰 프레임 선명도 측정
mod composite; // 다중노출/스태킹: JPEG N장 → 1장 합성


mod afpoint;
mod capture;
mod lifecycle;
mod props;
mod stack;
mod state;
mod storage;
mod stream;
mod swaf;
#[allow(unused_imports)]
use crate::{afpoint::*, capture::*, lifecycle::*, props::*, stack::*, state::*, storage::*, stream::*, swaf::*};

// 루트(IP:포트만 입력) → 웹 UI로 리다이렉트.
async fn root() -> Redirect {
    Redirect::to("/web/index.html")
}

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
        stack_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        stack_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        stack_count: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        stack_preview: Arc::new(Mutex::new(None)),
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
        .route("/api/bracket", post(bracket_start)) // 노출 브라케팅(AEB)
        .route("/api/stack/start", post(stack_start)) // 라이브스택
        .route("/api/stack/stop", post(stack_stop))
        .route("/api/stack/status", get(stack_status))
        .route("/api/stack/preview", get(stack_preview))
        .route("/api/stack/folder", post(stack_folder)) // 저장 프레임 풀해상도 포스트스택
        .route("/api/stack/save", post(stack_save)) // 스택 결과 PC 저장
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
        .route("/api/brightest", post(brightest)) // 가장 밝은 별 좌표 → Star AF
        .route("/api/focus_score", post(focus_score)) // 라이브 초점 미터(선명도 폴링)

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

