// crsdk_server/src/swaf.rs — main.rs에서 기능 계통별로 분리 (동작 불변)
use std::sync::Arc;
use std::time::Duration;


use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{
        IntoResponse, Json, Response,
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use crate::state::*;
use crate::props::{type_bits, signext, ControlInfoDto};
use crate::autofocus;

/// 현재 라이브뷰 프레임에서 RT-DETR 검출 → 박스 JSON(추적AF의 검출 소스).
/// bbox는 라이브뷰 픽셀 좌표(x0,y0,x1,y1), img_w/h와 함께 반환 → UI가 정규화해 오버레이.
#[cfg(feature = "detector")]
pub(crate) async fn detect(State(s): State<AppState>) -> Response {
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

pub(crate) async fn focus_nearfar_info(State(s): State<AppState>) -> Response {
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
pub(crate) struct FocusStep {
    step: i32, // 부호=방향(음수=Near, 양수=Far), 크기=스텝
}

pub(crate) async fn focus_near_far(
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
pub(crate) async fn af_drive(handle: i64, step: i32) {
    let _ = tokio::task::spawn_blocking(move || crsdk::control::focus_near_far(handle, step)).await;
}

/// 한 지점 선명도: stale 프레임 비우고 fresh `frames`장 평균. 라이브뷰 없으면 None.
pub(crate) async fn af_grab(
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
pub(crate) async fn af_phase(
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
pub(crate) async fn af_move_near(handle: i64, step: i32, times: usize, settle: Duration) {
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
pub(crate) async fn measure_and_land(
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
pub(crate) struct SwAfReq {
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
    threshold: Option<f64>,  // (continuous) baseline 대비 이 비율 미만이면 재합초(기본 0.45)
    check_ms: Option<u64>,   // (continuous) 모니터 주기(기본 900)
    debounce: Option<u32>,   // (continuous) 재합초까지 필요한 연속 하락 횟수(기본 3)
    cooldown: Option<u32>,   // (continuous) 재합초 후 억제 사이클(기본 4)
}

#[derive(Serialize)]
pub(crate) struct SwAfResult {
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
pub(crate) struct SwAfParams {
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
pub(crate) async fn swaf_lock(
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
pub(crate) async fn current_f_number(handle: i64) -> Option<f64> {
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
pub(crate) fn aperture_roi(f: f64) -> f64 {
    (0.25 * f / 5.6).clamp(0.10, 0.40)
}

/// 요청이 ROI를 안 줬으면 현재 조리개로 박스 크기를 채운다(개방→작게). 명시값은 존중.
pub(crate) async fn apply_aperture_defaults(handle: i64, b: &mut SwAfReq) {
    if b.roi.is_some() || b.roi_w.is_some() || b.roi_h.is_some() {
        return;
    }
    if let Some(f) = current_f_number(handle).await {
        b.roi = Some(aperture_roi(f));
    }
}

pub(crate) async fn sw_autofocus(State(s): State<AppState>, Json(mut b): Json<SwAfReq>) -> Response {
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

/// 현재 라이브뷰에서 가장 밝은 지점(별·달)의 정규화 좌표를 반환. 클라이언트가 이 좌표로
/// SW-AF를 걸어 "가장 밝은 별에 합초"한다(암순간 수동 합초가 어려운 밤하늘용). 장면이
/// 너무 어두우면(별 없음) 404, 라이브뷰 미가동이면 428.
pub(crate) async fn brightest(State(s): State<AppState>) -> Response {
    if s.camera.lock().await.is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response();
    }
    let mut rx = s.lv_tx.subscribe();
    while rx.try_recv().is_ok() {} // 쌓인 오래된 프레임 폐기
    let mut frame = None;
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_millis(700), rx.recv()).await {
            Ok(Ok(f)) => {
                frame = Some(f);
                break;
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    let frame = match frame {
        Some(f) => f,
        None => {
            return (StatusCode::PRECONDITION_REQUIRED, "live view not running").into_response()
        }
    };
    match tokio::task::spawn_blocking(move || autofocus::brightest_point(&frame[..])).await {
        Ok(Some((x, y))) => Json(serde_json::json!({ "x": x, "y": y })).into_response(),
        _ => (StatusCode::NOT_FOUND, "no bright point (scene too dark)").into_response(),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct FocusScoreReq {
    x: Option<f64>,
    y: Option<f64>,
    roi: Option<f64>,
}

/// 라이브 초점 미터: 현재 프레임에서 (지정 지점 또는 가장 밝은 별) ROI의 선명도(라플라시안
/// 분산)를 한 번 측정해 반환. 클라가 짧은 주기로 폴링하며 MF를 돌리면 값이 오르내리고 피크가
/// 정확 초점이다 — Bahtinov 마스크 없이 수동 정밀 합초용. {score, x, y}.
pub(crate) async fn focus_score(State(s): State<AppState>, Json(b): Json<FocusScoreReq>) -> Response {
    if s.camera.lock().await.is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE, "not connected").into_response();
    }
    let mut rx = s.lv_tx.subscribe();
    while rx.try_recv().is_ok() {} // 쌓인 오래된 프레임 폐기
    let mut frame = None;
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_millis(700), rx.recv()).await {
            Ok(Ok(f)) => {
                frame = Some(f);
                break;
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    let frame = match frame {
        Some(f) => f,
        None => {
            return (StatusCode::PRECONDITION_REQUIRED, "live view not running").into_response()
        }
    };
    let roi = b.roi.unwrap_or(0.15).clamp(0.05, 0.5);
    let fixed = b.x.zip(b.y);
    let res = tokio::task::spawn_blocking(move || {
        // 지정 지점 없으면 가장 밝은 별을 자동 타깃.
        let (x, y) = match fixed {
            Some(p) => p,
            None => autofocus::brightest_point(&frame[..])?,
        };
        let score = autofocus::focus_measure(&frame[..], x, y, roi, roi)?;
        Some((score, x, y))
    })
    .await;
    match res {
        Ok(Some((score, x, y))) => {
            Json(serde_json::json!({ "score": score, "x": x, "y": y })).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "no measurable point").into_response(),
    }
}

/// 연속 AF: 초기 합초 후 모니터 루프 — ROI 선명도가 baseline 대비 threshold 미만으로
/// 떨어지면(피사체 이동/카메라 흔들림) 재합초. /cancel(af_cancel=true)로 정지.
/// 즉시 "started" 반환하고 백그라운드 진행, 상태는 /events SSE(af_continuous).
pub(crate) async fn sw_autofocus_continuous(State(s): State<AppState>, Json(mut b): Json<SwAfReq>) -> Response {
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
    let threshold = b.threshold.unwrap_or(0.45).clamp(0.3, 0.95); // 낮을수록 큰 디포커스만 재합초(hunting↓)
    let check = Duration::from_millis(b.check_ms.unwrap_or(900).clamp(150, 5000));
    let debounce = b.debounce.unwrap_or(3).clamp(1, 10);   // 재합초까지 연속 하락 횟수
    let cooldown_n = b.cooldown.unwrap_or(4).clamp(0, 20);  // 재합초 후 억제 사이클
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
        let mut low_streak = 0u32; // 연속 하락 횟수(순간 블러·움직임 debounce)
        let mut cooldown = 0u32;   // 재합초 직후 재트리거 억제 사이클
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
            if cooldown > 0 {
                cooldown -= 1;
            }
            if baseline > 0.0 && cur < baseline * threshold {
                low_streak += 1;
            } else {
                low_streak = 0;
            }
            // debounce연속 하락 + 쿨다운 아님일 때만 재합초 — 순간 블러·피사체 이동에 스윕 남발(hunting)을 막는다.
            if low_streak >= debounce && cooldown == 0 {
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
                low_streak = 0;
                cooldown = cooldown_n; // 재합초 후 몇 사이클 쉼(피사체 계속 이동 중 재트리거 방지)
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

pub(crate) async fn sw_autofocus_cancel(State(s): State<AppState>) -> impl IntoResponse {
    // 실행 가드(af_active)는 소유 태스크가 해제한다. 여기선 취소 신호만 올린다.
    s.af_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, "cancel")
}

#[derive(Deserialize)]
pub(crate) struct RetargetReq { x: f64, y: f64, w: Option<f64>, h: Option<f64> }

/// 추적AF: 진행 중인 연속 AF의 대상 ROI를 갱신(피사체가 움직이면 클라가 새 centroid 전송).
/// 좌표는 미회전 정규화(SW-AF와 동일). 연속 AF 미실행 중이면 다음 시작 때 덮여 무해.
pub(crate) async fn sw_autofocus_retarget(State(s): State<AppState>, Json(b): Json<RetargetReq>) -> impl IntoResponse {
    let mut g = s.af_target.lock().unwrap_or_else(|e| e.into_inner());
    g.0 = b.x.clamp(0.0, 1.0);
    g.1 = b.y.clamp(0.0, 1.0);
    if let Some(w) = b.w { g.2 = w.clamp(0.05, 0.9); }
    if let Some(h) = b.h { g.3 = h.clamp(0.05, 0.9); }
    (StatusCode::OK, "retargeted")
}

#[derive(Deserialize)]
pub(crate) struct SharpReq {
    x: Option<f64>,
    y: Option<f64>,
    roi: Option<f64>,
    img: Option<u8>, // 1이면 측정한 프레임(JPEG)을 그대로 반환(눈으로 확인용)
}

/// 진단: 현재 라이브뷰 프레임의 (x,y) ROI 라플라시안 분산을 측정.
/// img=1 → 측정에 쓴 그 JPEG을 반환(`X-Sharpness` 헤더에 점수). 아니면 JSON {score}.
/// "라플라시안 돌리고 이미지를 까봐서 정상값 확인"용.
pub(crate) async fn debug_sharpness(State(s): State<AppState>, Query(q): Query<SharpReq>) -> Response {
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

