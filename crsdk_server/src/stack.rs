// crsdk_server/src/stack.rs — main.rs에서 기능 계통별로 분리 (동작 불변)


use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{
        IntoResponse, Json, Response,
    },
};
use tokio::sync::broadcast;
use crate::state::*;
use crate::storage::extract_embedded_jpeg;
use crate::stream::lv_producer;

// ── 라이브 스택 (별 정렬 + 프레임 누적) ────────────────────────────────────
/// 정렬 방식 파싱. "stars"(기본) | "centroid" | "roi"(roi=[cx,cy,half] 정규화).
pub(crate) fn parse_align(a: Option<&str>, roi: Option<[f32; 3]>) -> stacker::Align {
    match a {
        Some("centroid") => stacker::Align::Centroid,
        Some("roi") => match roi {
            Some(r) => stacker::Align::Roi { cx: r[0], cy: r[1], half: r[2] },
            None => stacker::Align::Stars,
        },
        _ => stacker::Align::Stars,
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct StackReq {
    mode: Option<String>,      // "average"(기본) | "lighten"
    align: Option<String>,     // "stars" | "centroid" | "roi"
    roi: Option<[f32; 3]>,     // roi 모드용 [cx, cy, half] (0..1)
}

/// 라이브스택 시작: 라이브뷰 프레임을 구독해 별 정렬 후 누적하고, 최신 스택본을 주기적으로
/// JPEG로 렌더해 stack_preview에 보관한다. 모든 CPU 작업(디코드/정렬/누적/인코드)은 blocking
/// 스레드에서 수행. 라이브뷰 프로듀서가 꺼져 있으면 함께 시작한다.
pub(crate) async fn stack_start(State(s): State<AppState>, Json(b): Json<StackReq>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let handle = {
        let g = s.camera.lock().await;
        match &*g {
            Some(c) => c.0.device_handle(),
            None => return (StatusCode::SERVICE_UNAVAILABLE, "not connected".to_string()),
        }
    };
    if s
        .stack_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "stack already running".to_string());
    }
    s.stack_cancel.store(false, Ordering::SeqCst);
    s.stack_count.store(0, Ordering::SeqCst);
    *s.stack_preview.lock().await = None;

    // 라이브뷰 프로듀서 보장(정지 상태면 시작) — liveview() 스타터와 동일.
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

    let mode = match b.mode.as_deref() {
        Some("lighten") => stacker::Mode::Lighten,
        _ => stacker::Mode::Average,
    };
    let al = parse_align(b.align.as_deref(), b.roi);
    let guard = RunGuard(s.stack_active.clone());
    let cancel = s.stack_cancel.clone();
    let count = s.stack_count.clone();
    let preview = s.stack_preview.clone();
    let lv_tx = s.lv_tx.clone();
    tokio::task::spawn_blocking(move || {
        let _g = guard; // 종료 시 stack_active 해제
        let mut rx = lv_tx.subscribe();
        let mut stk: Option<stacker::Stacker> = None;
        let mut dims: Option<(usize, usize)> = None;
        let mut last_render = std::time::Instant::now();
        let encode = |rgb: &[u8], w: usize, h: usize| -> Option<Vec<u8>> {
            let mut out = Vec::new();
            jpeg_encoder::Encoder::new(&mut out, 88)
                .encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
                .ok()?;
            Some(out)
        };
        loop {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let frame = match rx.blocking_recv() {
                Ok(f) => f,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(&frame[..]));
            let px = match dec.decode() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let info = match dec.info() {
                Some(i) => i,
                None => continue,
            };
            let (w, h) = (info.width as usize, info.height as usize);
            let rgb = match info.pixel_format {
                jpeg_decoder::PixelFormat::RGB24 => px,
                jpeg_decoder::PixelFormat::L8 => {
                    let mut r = vec![0u8; w * h * 3];
                    for i in 0..w * h {
                        r[i * 3] = px[i];
                        r[i * 3 + 1] = px[i];
                        r[i * 3 + 2] = px[i];
                    }
                    r
                }
                _ => continue,
            };
            match dims {
                None => {
                    dims = Some((w, h));
                    stk = Some(stacker::Stacker::new(w, h, mode).with_align(al));
                }
                Some((sw, sh)) if sw == w && sh == h => {}
                Some(_) => continue, // 해상도 바뀐 프레임은 스킵
            }
            let st = stk.as_mut().unwrap();
            if st.add(&rgb) {
                count.store(st.count(), Ordering::SeqCst);
                if last_render.elapsed() >= std::time::Duration::from_millis(400) {
                    if let Some(jpeg) = encode(&st.render(), w, h) {
                        *preview.blocking_lock() = Some(jpeg);
                    }
                    last_render = std::time::Instant::now();
                }
            }
        }
        // 종료 시 최종 렌더 반영.
        if let (Some(st), Some((w, h))) = (&stk, dims) {
            if let Some(jpeg) = encode(&st.render(), w, h) {
                *preview.blocking_lock() = Some(jpeg);
            }
        }
        tracing::info!("stack ended ({} frames)", count.load(Ordering::SeqCst));
    });
    (StatusCode::OK, "stack started".to_string())
}

pub(crate) async fn stack_stop(State(s): State<AppState>) -> impl IntoResponse {
    s.stack_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    (StatusCode::OK, "stopped".to_string())
}

pub(crate) async fn stack_status(State(s): State<AppState>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    Json(serde_json::json!({
        "running": s.stack_active.load(Ordering::SeqCst),
        "count": s.stack_count.load(Ordering::SeqCst),
    }))
}

pub(crate) async fn stack_preview(State(s): State<AppState>) -> Response {
    match s.stack_preview.lock().await.clone() {
        Some(jpeg) => ([(header::CONTENT_TYPE, "image/jpeg")], jpeg).into_response(),
        None => (StatusCode::NOT_FOUND, "no stack yet").into_response(),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct StackSaveReq {
    dir: Option<String>,
}

/// 현재 스택본(stack_preview JPEG)을 저장폴더(또는 dir)에 STACK_<epoch>.jpg로 저장.
pub(crate) async fn stack_save(State(s): State<AppState>, Json(b): Json<StackSaveReq>) -> impl IntoResponse {
    let jpeg = match s.stack_preview.lock().await.clone() {
        Some(j) => j,
        None => return (StatusCode::NOT_FOUND, "no stack to save".to_string()),
    };
    let dir = match b.dir {
        Some(d) if !d.trim().is_empty() => d,
        _ => s.save_path.lock().await.clone(),
    };
    if dir.is_empty() {
        return (StatusCode::BAD_REQUEST, "no folder (set save path or pass dir)".to_string());
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = std::path::Path::new(&dir).join(format!("STACK_{secs}.jpg"));
    match tokio::fs::write(&path, &jpeg).await {
        Ok(_) => (StatusCode::OK, path.to_string_lossy().to_string()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("save: {e}")),
    }
}

/// 저장 파일 한 장을 RGB8로 디코드. JPEG/HEIF는 직접, RAW(.arw)는 임베디드 JPEG로.
/// (진짜 RAW 디코드 = 16bit 선형은 추후 stacker `raw` feature의 rawloader로 대체.)
pub(crate) fn decode_to_rgb8(path: &std::path::Path, bytes: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    #[cfg(feature = "raw")]
    if ext == "arw" {
        // 진짜 RAW 디코드(rawler → 디모자이크 + WB, 반해상도) — 임베디드 JPEG보다 고품질.
        return stacker::decode_raw_rgb8(path.to_str()?);
    }
    let jpeg: std::borrow::Cow<[u8]> = if ext == "arw" {
        std::borrow::Cow::Owned(extract_embedded_jpeg(bytes)?)
    } else {
        std::borrow::Cow::Borrowed(bytes)
    };
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg.as_ref()));
    let px = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => px,
        jpeg_decoder::PixelFormat::L8 => {
            let mut r = vec![0u8; w * h * 3];
            for i in 0..w * h {
                r[i * 3] = px[i];
                r[i * 3 + 1] = px[i];
                r[i * 3 + 2] = px[i];
            }
            r
        }
        _ => return None,
    };
    Some((rgb, w, h))
}

#[derive(serde::Deserialize)]
pub(crate) struct FolderStackReq {
    mode: Option<String>,
    limit: Option<usize>,
    dir: Option<String>,  // 스택할 폴더(생략 시 저장 폴더)
    best: Option<f32>,    // lucky imaging: 선명한 상위 비율(0~1)만 스택. 생략=전부
    align: Option<String>, // "stars"(기본) | "centroid" | "roi"
    roi: Option<[f32; 3]>, // roi 모드용 [cx, cy, half] (0..1)
    #[cfg_attr(not(feature = "raw"), allow(dead_code))] // raw feature에서만 사용
    linear: Option<bool>, // true면 RAW를 선형광 f32로 누적(--features raw, ARW만). 고SNR.
}

/// 저장 폴더의 최근 촬영 프레임을 풀해상도로 포스트스택한다(라이브뷰보다 고화질). 파일을
/// mtime 최신순으로 최대 limit장 골라 오래된→최신으로 정렬 누적하고 결과를 stack_preview에
/// 넣는다(탭이 그대로 폴링). 카메라 불필요 — 파일만 읽는다. stack_active 가드로 라이브스택과
/// 상호배타.
pub(crate) async fn stack_folder(State(s): State<AppState>, Json(b): Json<FolderStackReq>) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let dir = match b.dir.clone() {
        Some(d) if !d.trim().is_empty() => d,
        _ => s.save_path.lock().await.clone(),
    };
    if dir.is_empty() {
        return (StatusCode::BAD_REQUEST, "no folder (set save path or pass dir)".to_string());
    }
    if s
        .stack_active
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::CONFLICT, "stack already running".to_string());
    }
    let mode = match b.mode.as_deref() {
        Some("lighten") => stacker::Mode::Lighten,
        _ => stacker::Mode::Average,
    };
    let limit = b.limit.unwrap_or(30).clamp(2, 200);
    let best = b.best;
    let al = parse_align(b.align.as_deref(), b.roi);
    #[cfg(feature = "raw")]
    let linear = b.linear.unwrap_or(false);
    s.stack_count.store(0, Ordering::SeqCst);
    s.stack_cancel.store(false, Ordering::SeqCst);
    *s.stack_preview.lock().await = None;
    let guard = RunGuard(s.stack_active.clone());
    let cancel = s.stack_cancel.clone();
    let count = s.stack_count.clone();
    let preview = s.stack_preview.clone();
    tokio::task::spawn_blocking(move || {
        let _g = guard;
        let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let ext = p.extension()?.to_str()?.to_lowercase();
                if matches!(ext.as_str(), "jpg" | "jpeg" | "arw") {
                    let m = e.metadata().ok()?.modified().ok()?;
                    Some((m, p))
                } else {
                    None
                }
            })
            .collect();
        files.sort_by(|a, b| b.0.cmp(&a.0)); // 최신 먼저
        files.truncate(limit); // 최근 limit장(버스트)
        // lucky imaging: 버스트 중 선명한 상위 비율만 선별(달·행성 대기 요동 극복).
        match best {
            Some(frac) if frac > 0.0 && frac < 1.0 => {
                let mut scored: Vec<(f64, std::path::PathBuf)> = files
                    .iter()
                    .filter_map(|(_, p)| {
                        let bytes = std::fs::read(p).ok()?;
                        let (rgb, w, h) = decode_to_rgb8(p, &bytes)?;
                        Some((stacker::sharpness(&rgb, w, h), p.clone()))
                    })
                    .collect();
                scored.sort_by(|a, b| {
                    b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
                });
                let keep = ((scored.len() as f32 * frac).ceil() as usize).max(2);
                scored.truncate(keep);
                // 선명한 순서 유지 → 가장 선명한 프레임이 정렬 기준.
                files = scored
                    .into_iter()
                    .map(|(_, p)| (std::time::SystemTime::UNIX_EPOCH, p))
                    .collect();
            }
            _ => files.reverse(), // 일반: 오래된→최신 (첫 장이 정렬 기준)
        }
        let mut stk: Option<stacker::Stacker> = None;
        let mut dims: Option<(usize, usize)> = None;
        for (_, path) in &files {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            // 선형 RAW 경로 (--features raw). ARW만, 감마 전 f32로 누적 → 고SNR.
            #[cfg(feature = "raw")]
            if linear {
                let is_arw = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("arw"))
                    .unwrap_or(false);
                if is_arw {
                    if let Some((rgb, w, h)) =
                        path.to_str().and_then(stacker::decode_raw_linear)
                    {
                        match dims {
                            None => {
                                dims = Some((w, h));
                                stk = Some(stacker::Stacker::new_linear(w, h, mode).with_align(al));
                            }
                            Some((sw, sh)) if sw == w && sh == h => {}
                            Some(_) => continue,
                        }
                        if stk.as_mut().unwrap().add_linear(&rgb) {
                            count.store(stk.as_ref().unwrap().count(), Ordering::SeqCst);
                        }
                    }
                }
                continue; // 선형 모드에선 8bit 경로 건너뜀
            }
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let (rgb, w, h) = match decode_to_rgb8(path, &bytes) {
                Some(v) => v,
                None => continue,
            };
            match dims {
                None => {
                    dims = Some((w, h));
                    stk = Some(stacker::Stacker::new(w, h, mode).with_align(al));
                }
                Some((sw, sh)) if sw == w && sh == h => {}
                Some(_) => continue,
            }
            if stk.as_mut().unwrap().add(&rgb) {
                count.store(stk.as_ref().unwrap().count(), Ordering::SeqCst);
            }
        }
        if let (Some(st), Some((w, h))) = (&stk, dims) {
            let out = st.render();
            let mut jpeg = Vec::new();
            if jpeg_encoder::Encoder::new(&mut jpeg, 92)
                .encode(&out, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
                .is_ok()
            {
                *preview.blocking_lock() = Some(jpeg);
            }
        }
        tracing::info!("post-stack done ({} frames)", count.load(Ordering::SeqCst));
    });
    (StatusCode::OK, "post-stack started".to_string())
}

