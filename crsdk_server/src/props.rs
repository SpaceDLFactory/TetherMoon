// crsdk_server/src/props.rs — main.rs에서 기능 계통별로 분리 (동작 불변)


use axum::{
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Json, Response,
    },
};
use crsdk::CameraEnumerator;
use serde::{Deserialize, Serialize};
use crate::state::*;

/// CrDataType base nibble → 비트폭. 0이면 미상으로 64 가정.
pub(crate) fn type_bits(value_type: u32) -> u32 {
    match value_type & crsdk::control::data_type::BASE_MASK {
        1 => 8, 2 => 16, 3 => 32, 4 => 64, 5 => 128, _ => 64,
    }
}

/// 비트폭 기준 부호 확장 → i64.
pub(crate) fn signext(v: u64, bits: u32) -> i64 {
    if bits >= 64 { return v as i64; }
    let mask = (1u64 << bits) - 1;
    let m = v & mask;
    let sb = 1u64 << (bits - 1);
    if m & sb != 0 { (m | !mask) as i64 } else { m as i64 }
}

#[derive(Serialize)]
pub(crate) struct ControlInfoDto {
    pub(crate) value_type: u32,
    pub(crate) is_range: bool,
    pub(crate) is_array: bool,
    pub(crate) is_signed: bool,
    /// 부호 비트 켜져 있으면 비트폭 기준 부호확장, 아니면 그대로 i64 변환.
    pub(crate) values: Vec<i64>,
}

/// 디버그: 카메라가 실제로 보고하는 모든 property code 목록 + 일부 메타.
/// 어떤 속성이 있는지 한눈에 보고 빠진 게 카메라 한계인지 판별용.
/// 네트워크 발견 진단 — EnumCameraObjects가 찾는 모든 카메라를 연결타입/ssh와 함께 덤프.
/// (A7C를 Wi-Fi PC Remote 모드로 두고 같은 네트워크에서 호출해 WiFi 발견 가능 여부 확인용.)
pub(crate) async fn debug_enum() -> Response {
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

pub(crate) async fn debug_all_codes(State(s): State<AppState>) -> Response {
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
pub(crate) async fn capabilities(State(s): State<AppState>) -> Response {
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

// ── 속성 읽기/쓰기 ───────────────────────────────────────────────────────
#[derive(Serialize)]
pub(crate) struct PropView {
    value: u64,
    editable: bool,
    allowed: Vec<u64>,
    value_type: u32, // CrDataType (Range 0x4000 등) — 프론트가 allowed 해석에 사용
}

#[derive(Serialize)]
pub(crate) struct PropertiesDto {
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

pub(crate) async fn properties(State(s): State<AppState>) -> Response {
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
pub(crate) struct SetProp {
    code: u32,
    value: u64,
}

pub(crate) async fn set_property(
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

