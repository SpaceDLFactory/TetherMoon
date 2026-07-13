// crsdk_server/src/storage.rs — main.rs에서 기능 계통별로 분리 (동작 불변)


use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{
        IntoResponse, Json, Response,
    },
};
use serde::Deserialize;
use crate::state::*;

#[derive(Deserialize)]
pub(crate) struct SetSavePath {
    path: String,
    #[serde(default)]
    prefix: String, // 파일명 접두사 (빈 문자열이면 카메라 기본 DSC)
}

pub(crate) async fn set_save_path(
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
pub(crate) async fn browse_save_path(State(s): State<AppState>) -> impl IntoResponse {
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

// ── 촬영 미리보기: 마지막 PC 저장 이미지 반환 ────────────────────────────
/// RAW(Sony ARW = TIFF 컨테이너)에 박힌 JPEG 프리뷰를 뽑아낸다. 파일에서 가장 큰 JPEG
/// 세그먼트(SOI `FF D8 FF` … EOI `FF D9`)를 찾아 반환 — ARW는 풀사이즈 JPEG 프리뷰를
/// 포함하므로 이걸 브라우저에 그대로 그릴 수 있다. 없으면 None.
pub(crate) fn extract_embedded_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    // 모든 SOI(FF D8 FF)…EOI(FF D9) 후보를 수집한다.
    let mut cands: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i + 2 < bytes.len() {
        if bytes[i] == 0xFF && bytes[i + 1] == 0xD8 && bytes[i + 2] == 0xFF {
            let mut j = i + 2;
            let mut found = false;
            while j + 1 < bytes.len() {
                if bytes[j] == 0xFF && bytes[j + 1] == 0xD9 {
                    cands.push((i, j + 2 - i));
                    i = j + 2;
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                break; // 닫히지 않은 SOI
            }
        } else {
            i += 1;
        }
    }
    // 큰 것부터, 실제 JPEG 헤더로 파싱되는 첫 후보를 반환 — ARW의 raw 데이터에 우연히
    // 생기는 가짜 FF D8…FF D9 세그먼트를 걸러낸다(가짜는 헤더 파싱 실패).
    cands.sort_by(|a, b| b.1.cmp(&a.1));
    for (s, l) in cands {
        let seg = &bytes[s..s + l];
        let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(seg));
        if dec.read_info().is_ok() {
            return Some(seg.to_vec());
        }
    }
    None
}

pub(crate) async fn last_image(State(s): State<AppState>) -> Response {
    let path = match s.last_image.lock().await.clone() {
        Some(p) => p,
        None => return (StatusCode::NOT_FOUND, "no image").into_response(),
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let lp = path.to_lowercase();
            if lp.ends_with(".heif") || lp.ends_with(".heic") {
                ([(header::CONTENT_TYPE, "image/heif")], bytes).into_response()
            } else if lp.ends_with(".jpg") || lp.ends_with(".jpeg") {
                ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response()
            } else if let Some(jpeg) = extract_embedded_jpeg(&bytes) {
                // RAW(.arw 등): 박힌 JPEG 프리뷰를 뽑아 미리보기로 그린다.
                ([(header::CONTENT_TYPE, "image/jpeg")], jpeg).into_response()
            } else {
                // 프리뷰 못 뽑은 RAW/미지원 → octet-stream, UI는 onerror로 스킵.
                ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
            }
        }
        Err(_) => (StatusCode::NOT_FOUND, "read fail").into_response(),
    }
}

#[cfg(test)]
mod jpeg_extract_tests {
    use super::extract_embedded_jpeg;

    fn real_jpeg(w: u16, h: u16) -> Vec<u8> {
        let rgb = vec![128u8; (w as usize) * (h as usize) * 3];
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90)
            .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    #[test]
    fn extracts_the_real_preview_over_bogus() {
        // ARW 흉내: TIFF 헤더 + 진짜 JPEG 프리뷰 + raw 데이터에 우연히 생긴 "더 큰" 가짜 세그먼트.
        let jpeg = real_jpeg(32, 24);
        let mut buf = vec![0x49, 0x49, 0x2A, 0x00]; // "II*\0"
        buf.extend_from_slice(&jpeg);
        // 진짜보다 더 큰 가짜 FF D8 FF … FF D9 (JPEG 헤더로 파싱 안 됨)
        let mut bogus = vec![0xFF, 0xD8, 0xFF];
        bogus.extend(std::iter::repeat(0x5A).take(jpeg.len() + 100));
        bogus.extend_from_slice(&[0xFF, 0xD9]);
        buf.extend_from_slice(&bogus);
        let out = extract_embedded_jpeg(&buf).expect("should find the real jpeg");
        assert_eq!(out, jpeg, "가짜(더 큰) 세그먼트가 아니라 진짜 JPEG을 뽑아야 함");
    }

    #[test]
    fn none_when_no_jpeg() {
        assert!(extract_embedded_jpeg(&[0x49, 0x49, 0x2A, 0x00, 1, 2, 3, 4, 5]).is_none());
    }

    #[test]
    fn none_on_unclosed_soi() {
        assert!(extract_embedded_jpeg(&[0x00, 0xFF, 0xD8, 0xFF, 0x11, 0x22]).is_none());
    }
}

