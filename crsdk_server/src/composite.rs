// crsdk_server/src/composite.rs
//
// 다중노출/스태킹 합성: 동일 해상도 JPEG N장을 픽셀별로 합성해 한 장으로.
// A7C에 없는 기능(소프트웨어 다중노출)을 서버에서 구현. 소스는 다운로드된 캡처 JPEG,
// v1은 정렬(alignment) 없이 삼각대 가정.
//
// 블렌드 모드(다중노출):
//   Average — N장 가산평균(클래식 다중노출. 밝기 유지).
//   Lighten — 픽셀별 채널 최댓값(밝은 요소만 누적 → 라이트트레일·별·불꽃).
//   Add     — 픽셀별 합(255 클램프. 가산 노출).

use std::io::Cursor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    Average,
    Lighten,
    Add,
}

impl Blend {
    /// 요청 문자열 → 모드. 미지정/불명은 Average.
    pub fn from_str(s: &str) -> Blend {
        match s {
            "lighten" => Blend::Lighten,
            "add" => Blend::Add,
            _ => Blend::Average,
        }
    }
}

/// JPEG 한 장을 RGB8 평면으로 디코드. (w, h, rgb[ w*h*3 ]). L8(그레이)은 RGB로 확장.
fn decode_rgb(jpeg: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut dec = jpeg_decoder::Decoder::new(Cursor::new(jpeg));
    let px = dec.decode().map_err(|e| format!("decode: {e}"))?;
    let info = dec.info().ok_or("no image info")?;
    let (w, h) = (info.width as usize, info.height as usize);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => Ok((w, h, px)),
        jpeg_decoder::PixelFormat::L8 => {
            let mut rgb = Vec::with_capacity(w * h * 3);
            for v in px {
                rgb.extend_from_slice(&[v, v, v]);
            }
            Ok((w, h, rgb))
        }
        other => Err(format!("unsupported pixel format: {other:?}")),
    }
}

/// RGB8 평면 → JPEG 인코드(품질 92).
pub fn encode_jpeg(w: usize, h: usize, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 92)
        .encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| format!("encode: {e}"))?;
    Ok(out)
}

/// N장의 JPEG를 블렌드 합성 → 결과 JPEG. 최소 1장, 모든 장 동일 해상도여야 함.
pub fn blend_jpegs(jpegs: &[Vec<u8>], mode: Blend) -> Result<Vec<u8>, String> {
    if jpegs.is_empty() {
        return Err("no frames".into());
    }
    let (w, h, first) = decode_rgb(&jpegs[0])?;
    let n = w * h * 3;

    // 평균은 정밀도 위해 u32 누산기. lighten/add는 결과 버퍼에 바로 누적.
    let mut acc = vec![0u32; n]; // Average 누산 / (Add는 결과로 직접)
    let mut out = match mode {
        Blend::Lighten => first.clone(), // 첫 장으로 시작, 이후 max
        Blend::Add => first.clone(),     // 첫 장으로 시작, 이후 +clamp
        Blend::Average => vec![0u8; n],
    };
    if mode == Blend::Average {
        for (a, &p) in acc.iter_mut().zip(first.iter()) {
            *a += p as u32;
        }
    }

    for (idx, jpeg) in jpegs.iter().enumerate().skip(1) {
        let (jw, jh, px) = decode_rgb(jpeg)?;
        if jw != w || jh != h {
            return Err(format!(
                "frame {idx} size {jw}x{jh} != {w}x{h} (정렬/동일촬영 필요)"
            ));
        }
        match mode {
            Blend::Average => {
                for (a, &p) in acc.iter_mut().zip(px.iter()) {
                    *a += p as u32;
                }
            }
            Blend::Lighten => {
                for (o, &p) in out.iter_mut().zip(px.iter()) {
                    if p > *o {
                        *o = p;
                    }
                }
            }
            Blend::Add => {
                for (o, &p) in out.iter_mut().zip(px.iter()) {
                    *o = (*o as u16 + p as u16).min(255) as u8;
                }
            }
        }
    }

    if mode == Blend::Average {
        let cnt = jpegs.len() as u32;
        for (o, &a) in out.iter_mut().zip(acc.iter()) {
            *o = (a / cnt) as u8;
        }
    }

    encode_jpeg(w, h, &out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 단색 RGB → JPEG (JPEG 손실 있으니 비교는 허용오차).
    fn solid(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(w * h * 3);
        for _ in 0..w * h {
            buf.extend_from_slice(&rgb);
        }
        encode_jpeg(w, h, &buf).unwrap()
    }

    fn center_px(jpeg: &[u8]) -> [u8; 3] {
        let (w, h, px) = decode_rgb(jpeg).unwrap();
        let i = (h / 2 * w + w / 2) * 3;
        [px[i], px[i + 1], px[i + 2]]
    }

    fn close(a: u8, b: u8) -> bool {
        (a as i16 - b as i16).abs() <= 6 // JPEG 손실 허용
    }

    #[test]
    fn average_blends_to_mean() {
        let a = solid(16, 16, [0, 100, 200]);
        let b = solid(16, 16, [100, 100, 0]);
        let out = blend_jpegs(&[a, b], Blend::Average).unwrap();
        let [r, g, bl] = center_px(&out);
        assert!(close(r, 50), "r={r}");
        assert!(close(g, 100), "g={g}");
        assert!(close(bl, 100), "b={bl}");
    }

    #[test]
    fn lighten_takes_max_per_channel() {
        let a = solid(16, 16, [200, 10, 50]);
        let b = solid(16, 16, [30, 150, 50]);
        let out = blend_jpegs(&[a, b], Blend::Lighten).unwrap();
        let [r, g, bl] = center_px(&out);
        assert!(close(r, 200) && close(g, 150) && close(bl, 50), "{r},{g},{bl}");
    }

    #[test]
    fn add_sums_and_clamps() {
        let a = solid(16, 16, [200, 10, 0]);
        let b = solid(16, 16, [100, 20, 0]);
        let out = blend_jpegs(&[a, b], Blend::Add).unwrap();
        let [r, g, _] = center_px(&out);
        assert!(r >= 249, "r clamps to 255-ish, got {r}"); // 200+100 → 255
        assert!(close(g, 30), "g={g}");
    }

    #[test]
    fn size_mismatch_errors() {
        let a = solid(16, 16, [10, 10, 10]);
        let b = solid(8, 8, [10, 10, 10]);
        assert!(blend_jpegs(&[a, b], Blend::Average).is_err());
    }

    #[test]
    fn single_frame_passthrough() {
        let a = solid(16, 16, [123, 45, 67]);
        let out = blend_jpegs(&[a], Blend::Average).unwrap();
        let [r, g, b] = center_px(&out);
        assert!(close(r, 123) && close(g, 45) && close(b, 67));
    }
}
