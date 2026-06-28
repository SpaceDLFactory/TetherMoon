//! RT-DETR CoreML 검출기의 안전한 Rust 래퍼 (detector.h C ABI 위).
//! TetherMoon 추적AF용 옵셔널 모듈 — 입력 raw RGB(HWC), 출력 박스/점수/클래스.

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

#[repr(C)]
struct DetectorRaw {
    _private: [u8; 0],
}

extern "C" {
    fn detector_create(path: *const c_char) -> *mut DetectorRaw;
    fn detector_infer(
        d: *mut DetectorRaw,
        rgb: *const u8,
        width: c_int,
        height: c_int,
        score_thresh: f32,
        max_n: c_int,
        out_boxes: *mut f32,
        out_scores: *mut f32,
        out_classes: *mut c_int,
    ) -> c_int;
    fn detector_free(d: *mut DetectorRaw);
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub class: i32,
    pub score: f32,
    /// x0, y0, x1, y1 — 입력 이미지 픽셀 좌표
    pub bbox: [f32; 4],
}

pub struct Detector {
    raw: *mut DetectorRaw,
}

// CoreML MLModel은 스레드 안전(예측은 내부 직렬화). 서버에서 Arc로 공유 가능하게.
unsafe impl Send for Detector {}
unsafe impl Sync for Detector {}

impl Detector {
    /// `.mlpackage` 경로로 로드. 실패 시 None.
    pub fn new(mlpackage_path: &str) -> Option<Detector> {
        let c = CString::new(mlpackage_path).ok()?;
        let raw = unsafe { detector_create(c.as_ptr()) };
        if raw.is_null() {
            None
        } else {
            Some(Detector { raw })
        }
    }

    /// raw RGB(HWC uint8, width*height*3) 추론. 내부에서 모델 입력(640²)으로 리사이즈.
    pub fn infer(&self, rgb: &[u8], width: i32, height: i32, score_thresh: f32, max_n: usize) -> Vec<Detection> {
        assert!(rgb.len() >= (width as usize) * (height as usize) * 3, "rgb buffer too small");
        let mut boxes = vec![0f32; max_n * 4];
        let mut scores = vec![0f32; max_n];
        let mut classes = vec![0i32; max_n];
        let n = unsafe {
            detector_infer(
                self.raw,
                rgb.as_ptr(),
                width,
                height,
                score_thresh,
                max_n as c_int,
                boxes.as_mut_ptr(),
                scores.as_mut_ptr(),
                classes.as_mut_ptr(),
            )
        };
        if n < 0 {
            return Vec::new();
        }
        (0..n as usize)
            .map(|i| Detection {
                class: classes[i],
                score: scores[i],
                bbox: [boxes[i * 4], boxes[i * 4 + 1], boxes[i * 4 + 2], boxes[i * 4 + 3]],
            })
            .collect()
    }
}

impl Drop for Detector {
    fn drop(&mut self) {
        unsafe { detector_free(self.raw) }
    }
}
