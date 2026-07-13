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

// AF 좌표 보정 — 바디마다 라이브뷰 좌표계/매핑이 다르다. 모델별 테이블로 키화한다.
// 좌표범위(x_max/y_max)는 SDK AF 그리드 기준 640×480 공통; y_cal만 바디별 실측이다.
pub(crate) struct AfCalib {
    x_max: u32,                   // X 좌표 최대 (0..=x_max)
    y_max: u32,                   // Y 좌표 최대 (선형 폴백 시 사용)
    y_cal: &'static [(f64, f64)], // (cmd_y, 실측 y_num) S커브 역보정표. 비면 선형.
}

// A7C 실측 (cmd_y, 카메라가 실제 놓은 y_num). 카메라가 cmd→실위치를 S커브로
// 매핑(중앙 압축)하므로, 클릭 ny를 박스 도달범위[28,297]에 선형 대응시키는 목표
// y_num을 역보간해 cmd_y를 구한다. FocusArea=M 기준 실측 (다른 크기도 근사 사용).
pub(crate) const A7C_Y_CAL: [(f64, f64); 5] =
    [(0.0, 28.0), (120.0, 66.0), (240.0, 162.0), (359.0, 256.0), (479.0, 297.0)];

/// 연결된 모델에 맞는 AF 보정. 미측정 바디는 선형 폴백.
pub(crate) fn af_calib(model: &str) -> AfCalib {
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

pub(crate) async fn af_point(State(s): State<AppState>, Json(b): Json<AfPoint>) -> impl IntoResponse {
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

