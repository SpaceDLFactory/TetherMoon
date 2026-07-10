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

/// 라이브뷰 JPEG 한 장에서 가장 밝은 지점(별·달·밝은 광원)의 정규화 좌표 (x,y) 반환.
/// 5x5 박스합의 최대 위치를 찾아 단일 핫픽셀/노이즈에 강건하게 만든다. 클라이언트가 이
/// 좌표로 SW-AF를 걸어 "가장 밝은 별에 합초"하는 데 쓴다. 장면이 노이즈 플로어 수준으로
/// 어두우면(별 없음) None — 어두운 하늘을 헛되이 좇지 않게 한다.
pub fn brightest_point(jpeg: &[u8]) -> Option<(f64, f64)> {
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
    let px = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w < 5 || h < 5 {
        return None;
    }
    let comps = match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => 1usize,
        jpeg_decoder::PixelFormat::RGB24 => 3usize,
        _ => return None,
    };
    if px.len() < w * h * comps {
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

    // 5x5 박스합의 최대 위치. (별은 작은 밝은 블롭 → 이웃 합이 큰 지점이 그 중심.)
    let r: isize = 2;
    let mut best = -1.0;
    let (mut bx, mut by) = (0usize, 0usize);
    for y in (r as usize)..(h - r as usize) {
        for x in (r as usize)..(w - r as usize) {
            let mut acc = 0.0;
            for dy in -r..=r {
                for dx in -r..=r {
                    acc += luma((x as isize + dx) as usize, (y as isize + dy) as usize);
                }
            }
            if acc > best {
                best = acc;
                bx = x;
                by = y;
            }
        }
    }
    // 최대 박스 평균이 저조도 노이즈 플로어(focus_measure 교훈: 캡=0, 노이즈=5~6)면 별 없음.
    let cells = ((2 * r + 1) * (2 * r + 1)) as f64;
    if best / cells < 12.0 {
        return None;
    }
    Some((bx as f64 / w as f64, by as f64 / h as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 합성 프레임: 검은 배경에 한 점만 밝게 → brightest_point가 그 지점을 찾는지.
    fn synth_jpeg(w: usize, h: usize, star_x: usize, star_y: usize) -> Vec<u8> {
        let mut rgb = vec![0u8; w * h * 3];
        // 노이즈 플로어(어두운 하늘)
        for p in rgb.iter_mut() {
            *p = 4;
        }
        // 3x3 밝은 별
        for dy in -1i32..=1 {
            for dx in -1i32..=1 {
                let x = star_x as i32 + dx;
                let y = star_y as i32 + dy;
                if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                    let i = ((y as usize) * w + x as usize) * 3;
                    rgb[i] = 250;
                    rgb[i + 1] = 250;
                    rgb[i + 2] = 250;
                }
            }
        }
        let mut out = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut out, 95);
        enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    #[test]
    fn finds_the_star() {
        let jpeg = synth_jpeg(320, 240, 220, 60);
        let (x, y) = brightest_point(&jpeg).expect("should find the star");
        // JPEG 손실 감안, 별 위치(0.6875, 0.25) 근처면 통과.
        assert!((x - 220.0 / 320.0).abs() < 0.05, "x={x}");
        assert!((y - 60.0 / 240.0).abs() < 0.05, "y={y}");
    }

    #[test]
    fn dark_scene_returns_none() {
        // 균일 노이즈 플로어(별 없음)
        let mut rgb = vec![5u8; 320 * 240 * 3];
        rgb.iter_mut().for_each(|p| *p = 5);
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 95)
            .encode(&rgb, 320, 240, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        assert!(brightest_point(&jpeg).is_none());
    }
}
