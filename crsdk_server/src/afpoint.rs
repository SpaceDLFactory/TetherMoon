// crsdk_server/src/afpoint.rs — main.rs에서 기능 계통별로 분리 (동작 불변)


use axum::{
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Json, Response,
    },
};
use serde::{Deserialize, Serialize};
use crate::state::*;

// ── 진단: 자이로(중력센서) 레벨 — 라이브뷰 자동회전 가능 여부 확인용 ──
#[derive(Serialize)]
pub(crate) struct LevelDto { on: bool, roll: i32, pitch: i32, z: i32 }

pub(crate) async fn level_info(State(s): State<AppState>) -> Response {
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
pub(crate) struct AfFrameDto { valid: bool, x_num: u32, x_deno: u32, y_num: u32, y_deno: u32, width: u32, height: u32 }

pub(crate) async fn af_frame_info(State(s): State<AppState>) -> Response {
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

// ── AF 포인트 지정 (정규화 0~1 → x:0~639 y:0~479, (x<<16)|y device property) ──
// 좌표계/패킹은 공식 RemoteCli 샘플(execute_pos_xy)을 따름. 위치 지정엔 FocusArea가
// Flexible Spot이어야 하므로 좌표 설정 전에 Flexible_Spot_S로 전환한다.
#[derive(Deserialize)]
pub(crate) struct AfPoint { x: f64, y: f64, #[serde(default)] area: Option<u64> }

// AF 좌표 보정은 바디별 정적 지식 → crsdk::body::BodyProfile로 이동. 여긴 프로필을 질의만 한다.

pub(crate) async fn af_point(State(s): State<AppState>, Json(b): Json<AfPoint>) -> impl IntoResponse {
    let (handle, model) = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => (c.0.device_handle(), c.1.clone()),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    let cal = crsdk::body::BodyProfile::for_model(&model).af_calib;
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

