// crsdk_server/src/stream.rs — main.rs에서 기능 계통별로 분리 (동작 불변)
use std::sync::Arc;
use std::time::Duration;

use std::convert::Infallible;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
};
use crsdk::{
    CameraEvent, LiveViewStream, SdkError,
};
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use crate::state::*;

// ── 이벤트 스트림 (SSE) ──────────────────────────────────────────────────
// CameraEvent를 JSON 문자열로 변환 (lib에 serde 의존성 추가하지 않기 위해 수동).
pub(crate) fn event_json(ev: &CameraEvent) -> String {
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

pub(crate) async fn events(State(s): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
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
/// lv_producer 종료(정상/에러/**패닉**/early-return) 시 반드시 lv_running=false로 되돌려
/// 다음 /lv 요청이 프로듀서를 재시작하게 한다. 없으면 패닉 시 running=true가 고착돼
/// 프로세스 재시작 전까지 라이브뷰가 영구 정지한다.
pub(crate) struct LvGuard(Arc<std::sync::Mutex<bool>>);
impl Drop for LvGuard {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }
}

pub(crate) fn lv_producer(handle: i64, lv_tx: broadcast::Sender<Arc<Vec<u8>>>, running: Arc<std::sync::Mutex<bool>>,
               cam: Arc<Mutex<Option<CameraCell>>>) {
    let _run = LvGuard(running.clone());
    // 연결 직후 카메라가 LiveView를 준비하는 데 시간이 필요 → 최대 4s 재시도
    let mut lv = None;
    for _ in 0..20 {
        match LiveViewStream::new(handle) {
            Ok(s) => { lv = Some(s); break; }
            Err(SdkError::LiveViewUnavailable) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return, // LvGuard가 running=false 처리
        }
    }
    let lv = match lv {
        Some(s) => s,
        None => {
            tracing::warn!("lv: LiveViewStream unavailable after retries");
            return; // LvGuard가 running=false 처리
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
                // running=false는 함수 종료 시 LvGuard가 처리.
                // 스트리밍하다 죽으면 하드 USB 제거 가능성(SDK OnDisconnected 콜백 미발화 대비) →
                // 세션을 비워 재연결 루프가 다시 붙게 한다. 스타트업 transient(0프레임)는 제외.
                // 우리 handle이 아직 걸려 있을 때만(새로 붙은 세션 오염 방지).
                if sent > 10 {
                    let mut g = cam.blocking_lock();
                    if g.as_ref().map(|c| c.0.device_handle()) == Some(handle) {
                        tracing::warn!("lv: treating fetch error as disconnect → clearing session");
                        *g = None;
                    }
                }
                break;
            }
        }
    }
    tracing::info!("lv: producer ended ({sent} frames)");
    // lv drops here → liveview_free_block
}

pub(crate) async fn liveview(State(s): State<AppState>) -> Response {
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
            let cam = s.camera.clone();
            tokio::task::spawn_blocking(move || lv_producer(handle, lv_tx, running_c, cam));
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

