// crsdk_server/src/autofocus.rs
//
// 소프트웨어 컨트라스트 검출 AF의 선명도 측정.
// 참조 알고리즘(AXIS/OpenCV): 중앙 ROI 라플라시안 분산. 여기서는 ROI 중심을
// 정규화 좌표 (cx, cy)로 받아 사용자가 찍은 포커스 지점 주변을 측정한다.
//
// 측정값 = ROI 내부 3x3 라플라시안 응답의 분산(= OpenCV stddev^2). 합초일수록 큼.

/// 라이브뷰 JPEG 한 장에서 (cx,cy) 중심 ROI의 라플라시안 분산을 계산.
/// cx,cy,roi_w,roi_h 는 0..1 정규화(roi_w/h = ROI 가로/세로 비율 — 직사각형 박스 지원).
/// 디코드/측정 실패 시 None.
pub fn focus_measure(jpeg: &[u8], cx: f64, cy: f64, roi_w: f64, roi_h: f64) -> Option<f64> {
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
    let px = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w < 3 || h < 3 {
        return None;
    }
    let comps = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1usize,
        jpeg_decoder::PixelFormat::RGB24 => 3usize,
        _ => return None, // L16/CMYK32 등은 라이브뷰에서 안 나옴
    };
    if px.len() < w * h * comps {
        return None;
    }

    // ROI 박스 (중심 cx,cy, 가로 roi_w·세로 roi_h). 경계 클램프.
    let cxp = (cx.clamp(0.0, 1.0) * w as f64) as isize;
    let cyp = (cy.clamp(0.0, 1.0) * h as f64) as isize;
    let half_w = ((roi_w.clamp(0.02, 1.0) * w as f64) / 2.0).max(2.0) as isize;
    let half_h = ((roi_h.clamp(0.02, 1.0) * h as f64) / 2.0).max(2.0) as isize;
    // 라플라시안은 이웃을 보므로 안쪽 1px 여유.
    let x0 = (cxp - half_w).clamp(1, w as isize - 2) as usize;
    let x1 = (cxp + half_w).clamp(1, w as isize - 2) as usize;
    let y0 = (cyp - half_h).clamp(1, h as isize - 2) as usize;
    let y1 = (cyp + half_h).clamp(1, h as isize - 2) as usize;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    let luma = |x: usize, y: usize| -> f64 {
        let i = (y * w + x) * comps;
        if comps == 1 {
            px[i] as f64
        } else {
            0.299 * px[i] as f64 + 0.587 * px[i + 1] as f64 + 0.114 * px[i + 2] as f64
        }
    };

    // 4-이웃 라플라시안 응답의 평균/분산.
    let mut sum = 0.0;
    let mut sumsq = 0.0;
    let mut n = 0.0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let lap = -4.0 * luma(x, y)
                + luma(x - 1, y)
                + luma(x + 1, y)
                + luma(x, y - 1)
                + luma(x, y + 1);
            sum += lap;
            sumsq += lap * lap;
            n += 1.0;
        }
    }
    if n < 1.0 {
        return None;
    }
    let mean = sum / n;
    Some((sumsq / n - mean * mean).max(0.0))
}
