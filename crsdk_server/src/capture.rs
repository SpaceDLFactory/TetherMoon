// crsdk_server/src/capture.rs — main.rs에서 기능 계통별로 분리 (동작 불변)
use std::time::Duration;


use axum::{
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Json, Response,
    },
};
use serde::Deserialize;
use tokio::sync::broadcast;
use crate::state::*;
use crate::composite;

// 한 장 촬영 (blocking): 포커스 모드에 따라 MF=즉시 캡처 / AF=S1 반누름 시퀀스.
pub(crate) fn capture_one(handle: i64) -> crsdk::SdkResult<()> {
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

pub(crate) async fn shutter(State(s): State<AppState>) -> impl IntoResponse {
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
pub(crate) async fn shutter_down(State(s): State<AppState>) -> impl IntoResponse {
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

pub(crate) async fn shutter_up(State(s): State<AppState>) -> impl IntoResponse {
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
pub(crate) async fn half_down(State(s): State<AppState>) -> impl IntoResponse {
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

pub(crate) async fn half_up(State(s): State<AppState>) -> impl IntoResponse {
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

// ── 벌브 타이머: 셔터 BULB로 N초 정밀 노출 (호스트가 down→sleep→up 타이밍 제어) ──
// A7C는 카메라 네이티브 벌브타이머(0x0209) 미지원 → 서버가 홀드 시간을 대신 잰다.
#[derive(Deserialize)]
pub(crate) struct BulbReq { seconds: u64 }

pub(crate) async fn bulb(State(s): State<AppState>, Json(b): Json<BulbReq>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let secs = b.seconds.clamp(1, 900); // 1초~15분
    let (handle, model) = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => (c.0.device_handle(), c.1.clone()),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    let bulb_enc = crsdk::body::BodyProfile::for_model(&model).bulb;
    // 중복 트리거 방지: false→true 교체에 성공한 호출만 진행.
    if s.bulb_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "bulb already running".to_string());
    }
    let active = s.bulb_active.clone();
    tokio::spawn(async move {
        // 바디 프로필의 BULB 인코딩으로 노출 시작.
        let start = tokio::task::spawn_blocking(move || {
            match bulb_enc {
                crsdk::body::BulbEncoding::ShutterZero => {
                    crsdk::properties::set(handle, crsdk::properties::code::SHUTTER_SPEED, 0)?;
                }
            }
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
pub(crate) struct IntervalReq { interval_sec: u64, count: u32 }

pub(crate) async fn interval_start(State(s): State<AppState>, Json(b): Json<IntervalReq>) -> impl IntoResponse {
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

pub(crate) async fn interval_stop(State(s): State<AppState>) -> impl IntoResponse {
    // 실행 가드(interval_active)는 소유 태스크가 해제. 여기선 취소 신호만.
    // (브라케팅도 같은 가드를 쓰므로 이 stop이 브라케팅도 중단시킨다.)
    s.interval_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, "stopped".to_string())
}

/// EV 브라케팅 촬영 순서의 allowed-list 인덱스들. 현재 인덱스 base 기준 대칭 오프셋
/// (frames=5,step=1 → 오프셋 -2,-1,0,+1,+2)을 리스트 경계로 클램프한다. 짝수 frames는
/// 아래로 한 칸 더 치우친다(-half..). 리스트가 비면 빈 벡터.
pub(crate) fn bracket_indices(base: usize, len: usize, frames: usize, step: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let step = step.max(1) as isize;
    let half = (frames / 2) as isize;
    (0..frames as isize)
        .map(|k| ((base as isize) + (k - half) * step).clamp(0, len as isize - 1) as usize)
        .collect()
}

#[derive(serde::Deserialize)]
pub(crate) struct BracketReq {
    frames: usize,
    step: usize,
}

/// 노출 브라케팅(AEB): 현재 노출보정(EV comp)을 기준으로 카메라의 EV allowed 리스트를
/// 인덱스로 스텝하며 frames장 촬영한다. 각 장은 노출 반영 settle 후 촬영하고 다음 노출로
/// 바꾸기 전 다운로드 완료를 기다린다. 끝나면 EV를 원래 값으로 복원. 인터벌 가드를 재사용해
/// 시퀀스 촬영끼리 상호배타이며, /api/interval/stop 으로 중단된다. 진행은 /events SSE(bracket).
pub(crate) async fn bracket_start(State(s): State<AppState>, Json(b): Json<BracketReq>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    const EV: u32 = crsdk::properties::code::EXPOSURE_BIAS_COMPENSATION;
    let frames = b.frames.clamp(2, 9);
    let step = b.step.clamp(1, 5);
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    if s
        .interval_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "sequence already running".to_string());
    }
    s.interval_cancel.store(false, Ordering::SeqCst);
    let guard = RunGuard(s.interval_active.clone());
    let cancel = s.interval_cancel.clone();
    let events = s.events_tx.clone();
    tokio::spawn(async move {
        let _guard = guard; // 종료 시 interval_active 해제
        let prop =
            tokio::task::spawn_blocking(move || crsdk::properties::get(handle, EV)).await;
        let (allowed, base_val) = match prop {
            Ok(Ok(Some(p))) if !p.allowed.is_empty() => (p.allowed, p.current),
            _ => {
                tracing::warn!("bracket: EV comp not available on this body");
                let _ = events.send(
                    serde_json::json!({"type":"bracket","error":"no EV comp"}).to_string(),
                );
                return;
            }
        };
        let base_idx = allowed
            .iter()
            .position(|&v| v == base_val)
            .unwrap_or(allowed.len() / 2);
        let idxs = bracket_indices(base_idx, allowed.len(), frames, step);
        let mut rx = events.subscribe();
        for (i, &idx) in idxs.iter().enumerate() {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let val = allowed[idx];
            let _ = tokio::task::spawn_blocking(move || crsdk::properties::set(handle, EV, val))
                .await;
            tokio::time::sleep(Duration::from_millis(300)).await; // 노출 반영 settle
            let _ = tokio::task::spawn_blocking(move || capture_one(handle)).await;
            // 다음 노출로 바꾸기 전 다운로드 완료 대기(RAW ~8s). 실패해도 계속.
            let _ = wait_jpeg_download(&mut rx, Duration::from_secs(15)).await;
            let _ = events.send(
                serde_json::json!({"type":"bracket","done":i+1,"total":frames}).to_string(),
            );
        }
        // EV 원복
        let _ =
            tokio::task::spawn_blocking(move || crsdk::properties::set(handle, EV, base_val)).await;
        let _ = events
            .send(serde_json::json!({"type":"bracket","done":frames,"total":frames,"finished":true}).to_string());
        tracing::info!("bracket done");
    });
    (StatusCode::OK, format!("bracket {frames}f step{step}"))
}

#[cfg(test)]
mod bracket_tests {
    use super::bracket_indices;

    #[test]
    fn symmetric_around_base() {
        // base=5, 리스트 충분, frames=5, step=1 → 3,4,5,6,7
        assert_eq!(bracket_indices(5, 11, 5, 1), vec![3, 4, 5, 6, 7]);
    }
    #[test]
    fn step_scales_offsets() {
        // step=2 → 1,3,5,7,9
        assert_eq!(bracket_indices(5, 11, 5, 2), vec![1, 3, 5, 7, 9]);
    }
    #[test]
    fn clamps_at_edges() {
        // base=0이면 아래로 못 가 0에서 클램프
        assert_eq!(bracket_indices(0, 5, 5, 1), vec![0, 0, 0, 1, 2]);
        // base=끝이면 위로 클램프
        assert_eq!(bracket_indices(4, 5, 3, 1), vec![3, 4, 4]);
    }
    #[test]
    fn empty_list() {
        assert_eq!(bracket_indices(0, 0, 3, 1), Vec::<usize>::new());
    }
}

// ── 동영상 녹화 (MovieRecord) ────────────────────────────────────────────
pub(crate) async fn movie_start(State(s): State<AppState>) -> impl IntoResponse {
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

pub(crate) async fn movie_stop(State(s): State<AppState>) -> impl IntoResponse {
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

pub(crate) async fn cancel_shooting(State(s): State<AppState>) -> impl IntoResponse {
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

// ── 다중노출 (소프트웨어 — A7C엔 없는 기능) ──────────────────────────────
#[derive(Deserialize)]
pub(crate) struct MultiExpReq {
    count: Option<u32>,           // 합칠 장수(기본 3)
    mode: Option<String>,         // "average"|"lighten"|"add"(기본 average)
    shot_timeout_ms: Option<u64>, // 장당 JPEG 다운로드 대기(기본 12000; RAW 느림)
}

/// events_tx(JSON)에서 다음 JPEG download_complete 파일경로를 기다림. RAW 등 비-JPEG는 건너뜀.
pub(crate) async fn wait_jpeg_download(
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

/// N장 연속 촬영 → 다운로드된 JPEG들을 블렌드 합성 → 1장 저장. 진행/완료는 events SSE.
pub(crate) async fn multi_exposure(State(s): State<AppState>, Json(b): Json<MultiExpReq>) -> Response {
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

