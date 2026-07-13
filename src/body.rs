// src/body.rs — 바디별 정적 quirk 프로필.
//
// [[capability]](런타임: 바디가 무엇을 노출하나 — get_all로 탐지)의 짝.
// 프로필은 정적 지식이다: "어떻게 다루나". 흩어져 있던 A7C 하드코딩(AF 좌표 보정·그리드
// 범위·BULB 인코딩)을 한 곳으로 모은다.
//
// 원칙:
//   1. 미지 바디도 죽지 않게 — for_model은 안전한 기본값으로 degrade(크래시 아님).
//   2. capability(런타임)와 분리 유지 — 여긴 정적 지식만.
//   3. 동작 불변 — A7C 값은 이전과 동일. 유닛테스트로 고정.
//
// 확장점(바디 추가 시 채울 슬롯, 아직 미배선): capture_af 후 settle 타이밍,
// AF 영역 코드 매핑, 파일형식 기본값 등. 바디가 3개+ 되면 TOML 임베드로 옮길 후보.

/// AF 좌표 보정. 바디마다 라이브뷰 좌표계/매핑이 다르다.
/// x는 선형, y는 S커브 역보정표(`y_cal`)로 역보간. 표가 비면 선형 폴백.
pub struct AfCalib {
    /// X 좌표 최대 (0..=x_max)
    pub x_max: u32,
    /// Y 좌표 최대 (선형 폴백 시 사용)
    pub y_max: u32,
    /// (cmd_y, 실측 y_num) S커브 역보정표. 비면 선형.
    pub y_cal: &'static [(f64, f64)],
}

impl AfCalib {
    pub fn x(&self, nx: f64) -> u32 {
        (nx.clamp(0.0, 1.0) * self.x_max as f64).round() as u32
    }
    pub fn y(&self, ny: f64) -> u32 {
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

// A7C 실측 (cmd_y, 카메라가 실제 놓은 y_num). 카메라가 cmd→실위치를 S커브로 매핑(중앙
// 압축)하므로, 클릭 ny를 박스 도달범위[28,297]에 선형 대응시키는 목표 y_num을 역보간해
// cmd_y를 구한다. FocusArea=M 기준 실측(다른 크기도 근사 사용).
const A7C_Y_CAL: [(f64, f64); 5] =
    [(0.0, 28.0), (120.0, 66.0), (240.0, 162.0), (359.0, 256.0), (479.0, 297.0)];

/// BULB 노출을 이 바디에서 어떻게 지정하는가.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BulbEncoding {
    /// SHUTTER_SPEED = 0 이 BULB (A7C). 소프트웨어 벌브 타이머가 down/up 타이밍을 잰다.
    ShutterZero,
}

/// 연결된 바디의 정적 프로필.
pub struct BodyProfile {
    pub model: String,
    pub af_calib: AfCalib,
    pub bulb: BulbEncoding,
}

impl BodyProfile {
    /// 모델명 → 프로필. 미측정/미지 바디는 안전한 기본값(선형 AF, ShutterZero 시도)으로 degrade.
    pub fn for_model(model: &str) -> Self {
        if model.eq_ignore_ascii_case("ILCE-7C") {
            BodyProfile {
                model: model.to_string(),
                af_calib: AfCalib { x_max: 639, y_max: 479, y_cal: &A7C_Y_CAL },
                bulb: BulbEncoding::ShutterZero,
            }
        } else {
            BodyProfile {
                model: model.to_string(),
                // 미측정 바디: AF Y-보정 없이 선형. 그리드 범위는 SDK 공통 640×480 가정.
                af_calib: AfCalib { x_max: 639, y_max: 479, y_cal: &[] },
                bulb: BulbEncoding::ShutterZero,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a7c_profile_values() {
        let p = BodyProfile::for_model("ILCE-7C");
        assert_eq!(p.bulb, BulbEncoding::ShutterZero);
        assert_eq!(p.af_calib.x_max, 639);
        // x는 선형: nx=0.5 → ~320
        assert_eq!(p.af_calib.x(0.5), 320);
        // y는 S커브 역보정: 경계값 재현
        assert_eq!(p.af_calib.y(0.0), 0); // 도달 최소 → cmd_y 0
        assert_eq!(p.af_calib.y(1.0), 479); // 도달 최대 → cmd_y 479
        // S커브는 저단에서 선형과 어긋난다: ny=0.25는 선형이면 ~120이나 실측 역보정은 더 큼.
        assert!(p.af_calib.y(0.25) > 140, "S-curve deviates from linear: {}", p.af_calib.y(0.25));
    }

    #[test]
    fn unknown_body_degrades_linear() {
        let p = BodyProfile::for_model("ILCE-9999");
        assert!(p.af_calib.y_cal.is_empty());
        assert_eq!(p.af_calib.y(0.5), 240); // 선형 폴백: 0.5*479≈240
    }
}
