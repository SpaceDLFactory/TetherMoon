//! 별 정렬 + 프레임 스태킹 엔진.
//!
//! 순수 산술: 카메라·JPEG·서버에 의존하지 않는다. 입력은 디코드된 RGB8 프레임 + 크기.
//! 라이브뷰 JPEG 라이브스택(A)이 이걸 쓰고, 추후 RAW 포스트스택(B, `--features raw`)도
//! 같은 정렬 엔진을 재사용한다.
//!
//! 파이프라인: RGB → luma → 별 검출(밝은 국소최대의 서브픽셀 centroid) → 기준 프레임에
//! 이동 정합 → float 누적(평균 또는 lighten) → 렌더.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Star {
    pub x: f32,
    pub y: f32,
    pub flux: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    /// 정렬 후 평균 — SNR 향상(성운/딥스카이 라이브 미리보기).
    Average,
    /// 픽셀별 최댓값 — 밝은 것 누적(별궤적/합성).
    Lighten,
}

/// RGB8 → luma f32.
pub fn luma_from_rgb(rgb: &[u8], w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0f32; w * h];
    for i in 0..w * h {
        let j = i * 3;
        out[i] = 0.299 * rgb[j] as f32 + 0.587 * rgb[j + 1] as f32 + 0.114 * rgb[j + 2] as f32;
    }
    out
}

/// 밝은 별 최대 `max_stars`개 검출. 임계 = mean + 4·std, 3x3 국소최대, 반경 3 창의
/// 밝기가중 서브픽셀 centroid, flux 내림차순 + 최소 간격 `min_sep`로 솎음.
pub fn detect_stars(luma: &[f32], w: usize, h: usize, max_stars: usize, min_sep: f32) -> Vec<Star> {
    if w < 8 || h < 8 || luma.len() < w * h {
        return Vec::new();
    }
    let n = (w * h) as f32;
    let mean = luma.iter().sum::<f32>() / n;
    let var = luma.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    let thresh = mean + 4.0 * var.sqrt();
    let px = |x: i32, y: i32| luma[(y as usize) * w + x as usize];
    let r = 3i32;
    let mut cands: Vec<Star> = Vec::new();
    for y in r..(h as i32 - r) {
        for x in r..(w as i32 - r) {
            let v = px(x, y);
            if v < thresh {
                continue;
            }
            let mut ismax = true;
            'nb: for dy in -1..=1 {
                for dx in -1..=1 {
                    if (dx != 0 || dy != 0) && px(x + dx, y + dy) > v {
                        ismax = false;
                        break 'nb;
                    }
                }
            }
            if !ismax {
                continue;
            }
            let (mut sx, mut sy, mut sf) = (0f32, 0f32, 0f32);
            for dy in -r..=r {
                for dx in -r..=r {
                    let val = (px(x + dx, y + dy) - mean).max(0.0);
                    sx += (x + dx) as f32 * val;
                    sy += (y + dy) as f32 * val;
                    sf += val;
                }
            }
            if sf > 0.0 {
                cands.push(Star { x: sx / sf, y: sy / sf, flux: sf });
            }
        }
    }
    cands.sort_by(|a, b| b.flux.partial_cmp(&a.flux).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<Star> = Vec::new();
    for c in cands {
        if out
            .iter()
            .all(|s| ((s.x - c.x).powi(2) + (s.y - c.y).powi(2)).sqrt() >= min_sep)
        {
            out.push(c);
            if out.len() >= max_stars {
                break;
            }
        }
    }
    out
}

/// `cur`를 `reference`에 맞추는 이동 t 추정: `cur ≈ reference + t`. 밝은 별 쌍을 후보로
/// 각 t의 inlier(정합 별) 수를 세어 최대인 t를 고르고, inlier 오프셋 평균으로 정제한다.
/// 반환 t로 프레임을 `-t` 이동하면 기준에 정렬. 별 부족/정합 실패 시 None.
pub fn estimate_translation(reference: &[Star], cur: &[Star], tol: f32) -> Option<(f32, f32)> {
    if reference.len() < 2 || cur.len() < 2 {
        return None;
    }
    let top_r = reference.len().min(4);
    let top_c = cur.len().min(6);
    let mut best: Option<((f32, f32), usize, f32)> = None; // (t, inliers, err)
    for r in reference.iter().take(top_r) {
        for c in cur.iter().take(top_c) {
            let t = (c.x - r.x, c.y - r.y);
            let mut inl = 0usize;
            let mut err = 0f32;
            for rs in reference {
                let (px, py) = (rs.x + t.0, rs.y + t.1);
                let mut bd = f32::MAX;
                for cs in cur {
                    let d = (cs.x - px).powi(2) + (cs.y - py).powi(2);
                    if d < bd {
                        bd = d;
                    }
                }
                if bd <= tol * tol {
                    inl += 1;
                    err += bd.sqrt();
                }
            }
            let better = match best {
                None => true,
                Some((_, bi, be)) => inl > bi || (inl == bi && err < be),
            };
            if better && inl >= 2 {
                best = Some((t, inl, err));
            }
        }
    }
    let (t0, _, _) = best?;
    // 정제: t0로 매칭된 쌍들의 오프셋 평균.
    let (mut sx, mut sy, mut k) = (0f32, 0f32, 0f32);
    for rs in reference {
        let (px, py) = (rs.x + t0.0, rs.y + t0.1);
        let mut bd = f32::MAX;
        let mut best_c: Option<&Star> = None;
        for cs in cur {
            let d = (cs.x - px).powi(2) + (cs.y - py).powi(2);
            if d < bd {
                bd = d;
                best_c = Some(cs);
            }
        }
        if bd <= tol * tol {
            if let Some(cs) = best_c {
                sx += cs.x - rs.x;
                sy += cs.y - rs.y;
                k += 1.0;
            }
        }
    }
    if k > 0.0 {
        Some((sx / k, sy / k))
    } else {
        Some(t0)
    }
}

/// 2D 강체변환(회전+이동, 스케일=1). `cur ≈ R·ref + t`.
#[derive(Clone, Copy, Debug)]
pub struct Rigid {
    pub cos: f32,
    pub sin: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Rigid {
    pub fn identity() -> Self {
        Self { cos: 1.0, sin: 0.0, tx: 0.0, ty: 0.0 }
    }
    /// ref 좌표 → cur 좌표.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.cos * x - self.sin * y + self.tx,
            self.sin * x + self.cos * y + self.ty,
        )
    }
    /// 회전각(라디안).
    pub fn angle(&self) -> f32 {
        self.sin.atan2(self.cos)
    }
}

/// 대응쌍으로 2D 강체 최소자승(Kabsch/Umeyama, 스케일 1) 재추정. ref→cur.
fn kabsch_2d(pairs: &[((f32, f32), (f32, f32))]) -> Option<Rigid> {
    if pairs.len() < 2 {
        return None;
    }
    let n = pairs.len() as f32;
    let (mut rcx, mut rcy, mut ccx, mut ccy) = (0.0, 0.0, 0.0, 0.0);
    for &((rx, ry), (cx, cy)) in pairs {
        rcx += rx;
        rcy += ry;
        ccx += cx;
        ccy += cy;
    }
    rcx /= n;
    rcy /= n;
    ccx /= n;
    ccy /= n;
    // θ = atan2( Σ(r'x·c'y - r'y·c'x), Σ(r'x·c'x + r'y·c'y) )
    let (mut a, mut b) = (0.0f32, 0.0f32);
    for &((rx, ry), (cx, cy)) in pairs {
        let (rx, ry, cx, cy) = (rx - rcx, ry - rcy, cx - ccx, cy - ccy);
        a += rx * cx + ry * cy;
        b += rx * cy - ry * cx;
    }
    let th = b.atan2(a);
    let (cs, sn) = (th.cos(), th.sin());
    let rx = cs * rcx - sn * rcy;
    let ry = sn * rcx + cs * rcy;
    Some(Rigid { cos: cs, sin: sn, tx: ccx - rx, ty: ccy - ry })
}

/// `cur ≈ R·ref + t` 강체변환(회전+이동) 추정. 밝은 별에서 2점 대응 가설을 세우되 두 점
/// 사이 거리가 보존되는 쌍(강체)만 시도, inlier 최대 변환을 고른 뒤 inlier 대응으로 Kabsch
/// 정제. 이동만인 경우는 회전≈0으로 자연히 포함. 별 부족/정합 실패 시 None.
pub fn estimate_rigid(reference: &[Star], cur: &[Star], tol: f32) -> Option<Rigid> {
    if reference.len() < 2 || cur.len() < 2 {
        return None;
    }
    let rn = reference.len().min(6);
    let cn = cur.len().min(8);
    let mut best: Option<(Rigid, usize, f32)> = None;
    for i in 0..rn {
        for j in 0..rn {
            if i == j {
                continue;
            }
            let (r0, r1) = (reference[i], reference[j]);
            let rd = ((r1.x - r0.x).powi(2) + (r1.y - r0.y).powi(2)).sqrt();
            if rd < 3.0 {
                continue; // 너무 가까운 쌍 → 각도 불안정
            }
            let ra = (r1.y - r0.y).atan2(r1.x - r0.x);
            for a in 0..cn {
                for b in 0..cn {
                    if a == b {
                        continue;
                    }
                    let (c0, c1) = (cur[a], cur[b]);
                    let cd = ((c1.x - c0.x).powi(2) + (c1.y - c0.y).powi(2)).sqrt();
                    if (cd - rd).abs() > tol {
                        continue; // 거리 보존 안 됨 → 강체 대응 아님
                    }
                    let ca = (c1.y - c0.y).atan2(c1.x - c0.x);
                    let th = ca - ra;
                    let (cs, sn) = (th.cos(), th.sin());
                    let rx = cs * r0.x - sn * r0.y;
                    let ry = sn * r0.x + cs * r0.y;
                    let cand = Rigid { cos: cs, sin: sn, tx: c0.x - rx, ty: c0.y - ry };
                    let mut inl = 0usize;
                    let mut err = 0f32;
                    for rs in reference {
                        let (px, py) = cand.apply(rs.x, rs.y);
                        let mut bd = f32::MAX;
                        for cr in cur {
                            let d = (cr.x - px).powi(2) + (cr.y - py).powi(2);
                            if d < bd {
                                bd = d;
                            }
                        }
                        if bd <= tol * tol {
                            inl += 1;
                            err += bd.sqrt();
                        }
                    }
                    let better = match best {
                        None => true,
                        Some((_, bi, be)) => inl > bi || (inl == bi && err < be),
                    };
                    if better && inl >= 3 {
                        best = Some((cand, inl, err));
                    }
                }
            }
        }
    }
    let (b0, _, _) = best?;
    // inlier 대응 수집 → Kabsch 정제.
    let mut pairs: Vec<((f32, f32), (f32, f32))> = Vec::new();
    for rs in reference {
        let (px, py) = b0.apply(rs.x, rs.y);
        let mut bd = f32::MAX;
        let mut bc: Option<&Star> = None;
        for cr in cur {
            let d = (cr.x - px).powi(2) + (cr.y - py).powi(2);
            if d < bd {
                bd = d;
                bc = Some(cr);
            }
        }
        if bd <= tol * tol {
            if let Some(c) = bc {
                pairs.push(((rs.x, rs.y), (c.x, c.y)));
            }
        }
    }
    Some(kabsch_2d(&pairs).unwrap_or(b0))
}

/// 정렬 누적기. 첫 프레임을 기준으로 삼고 이후 프레임을 정합해 float 누적한다.
pub struct Stacker {
    w: usize,
    h: usize,
    mode: Mode,
    buf: Vec<f32>, // RGB 누적 (average=합, lighten=픽셀별 최대)
    wt: Vec<f32>,  // average용 픽셀별 기여수
    reference: Option<Vec<Star>>,
    count: u32,
}

impl Stacker {
    pub fn new(w: usize, h: usize, mode: Mode) -> Self {
        Self {
            w,
            h,
            mode,
            buf: vec![0.0; w * h * 3],
            wt: vec![0.0; w * h],
            reference: None,
            count: 0,
        }
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// RGB8 한 장 추가. 첫 장은 기준(정렬 없이 누적). 이후 장은 기준에 정합, 실패 시
    /// 프레임을 기각하고 false(누적 안 함). 성공 시 true.
    pub fn add(&mut self, rgb: &[u8]) -> bool {
        if rgb.len() < self.w * self.h * 3 {
            return false;
        }
        let luma = luma_from_rgb(rgb, self.w, self.h);
        let stars = detect_stars(&luma, self.w, self.h, 16, 8.0);
        let tf = match &self.reference {
            Some(ref_stars) => match estimate_rigid(ref_stars, &stars, 6.0) {
                Some(t) => t,
                None => return false, // 정합 실패(별 부족/불량 프레임) → 기각
            },
            None => {
                if stars.len() < 3 {
                    return false; // 기준 프레임에 별 3개 미만이면 시작 안 함
                }
                self.reference = Some(stars);
                Rigid::identity()
            }
        };
        self.accumulate(rgb, tf);
        self.count += 1;
        true
    }

    // 출력(기준좌표) 픽셀 (x,y) ← cur 좌표 tf.apply(x,y)를 bilinear 샘플. (cur ≈ R·ref + t.)
    fn accumulate(&mut self, rgb: &[u8], tf: Rigid) {
        let (w, h) = (self.w, self.h);
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = tf.apply(x as f32, y as f32);
                if sx < 0.0 || sy < 0.0 || sx > (w - 1) as f32 || sy > (h - 1) as f32 {
                    continue;
                }
                let x0 = sx.floor() as usize;
                let y0 = sy.floor() as usize;
                let x1 = (x0 + 1).min(w - 1);
                let y1 = (y0 + 1).min(h - 1);
                let fx = sx - x0 as f32;
                let fy = sy - y0 as f32;
                let di = (y * w + x) * 3;
                for c in 0..3 {
                    let p00 = rgb[(y0 * w + x0) * 3 + c] as f32;
                    let p10 = rgb[(y0 * w + x1) * 3 + c] as f32;
                    let p01 = rgb[(y1 * w + x0) * 3 + c] as f32;
                    let p11 = rgb[(y1 * w + x1) * 3 + c] as f32;
                    let v = p00 * (1.0 - fx) * (1.0 - fy)
                        + p10 * fx * (1.0 - fy)
                        + p01 * (1.0 - fx) * fy
                        + p11 * fx * fy;
                    match self.mode {
                        Mode::Average => self.buf[di + c] += v,
                        Mode::Lighten => {
                            if v > self.buf[di + c] {
                                self.buf[di + c] = v;
                            }
                        }
                    }
                }
                if self.mode == Mode::Average {
                    self.wt[y * w + x] += 1.0;
                }
            }
        }
    }

    /// 현재 스택본을 RGB8로 렌더.
    pub fn render(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.w * self.h * 3];
        for i in 0..self.w * self.h {
            let di = i * 3;
            match self.mode {
                Mode::Average => {
                    let wt = self.wt[i].max(1.0);
                    for c in 0..3 {
                        out[di + c] = (self.buf[di + c] / wt).round().clamp(0.0, 255.0) as u8;
                    }
                }
                Mode::Lighten => {
                    for c in 0..3 {
                        out[di + c] = self.buf[di + c].clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 검은 배경에 밝은 점들 찍은 RGB8 프레임 생성.
    fn frame(w: usize, h: usize, stars: &[(usize, usize)]) -> Vec<u8> {
        let mut rgb = vec![3u8; w * h * 3]; // 어두운 배경
        for &(sx, sy) in stars {
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let x = sx as i32 + dx;
                    let y = sy as i32 + dy;
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        let i = ((y as usize) * w + x as usize) * 3;
                        rgb[i] = 240;
                        rgb[i + 1] = 240;
                        rgb[i + 2] = 240;
                    }
                }
            }
        }
        rgb
    }

    #[test]
    fn detects_injected_stars() {
        let f = frame(64, 48, &[(10, 10), (40, 30), (55, 8)]);
        let luma = luma_from_rgb(&f, 64, 48);
        let s = detect_stars(&luma, 64, 48, 12, 4.0);
        assert_eq!(s.len(), 3, "별 3개를 찾아야 함");
        // centroid가 주입 위치 근처
        assert!(s.iter().any(|st| (st.x - 10.0).abs() < 1.0 && (st.y - 10.0).abs() < 1.0));
    }

    #[test]
    fn recovers_known_shift() {
        let a = frame(64, 48, &[(10, 10), (40, 30), (55, 8)]);
        let b = frame(64, 48, &[(15, 7), (45, 27), (60, 5)]); // +(5,-3)
        let sa = detect_stars(&luma_from_rgb(&a, 64, 48), 64, 48, 12, 4.0);
        let sb = detect_stars(&luma_from_rgb(&b, 64, 48), 64, 48, 12, 4.0);
        let t = estimate_translation(&sa, &sb, 3.0).expect("정합 성공해야");
        assert!((t.0 - 5.0).abs() < 0.6, "tx={}", t.0);
        assert!((t.1 - (-3.0)).abs() < 0.6, "ty={}", t.1);
    }

    #[test]
    fn average_aligns_star_not_smeared() {
        // 같은 별밭이 (4,-2) 밀린 두 장. 정렬 평균하면 기준 위치에 별이 선명해야(안 번짐).
        let a = frame(64, 48, &[(20, 20), (45, 30), (10, 8)]);
        let b = frame(64, 48, &[(24, 18), (49, 28), (14, 6)]);
        let mut st = Stacker::new(64, 48, Mode::Average);
        assert!(st.add(&a));
        assert!(st.add(&b));
        assert_eq!(st.count(), 2);
        let out = st.render();
        // 기준 위치 (20,20)은 밝고, 미정렬 위치 (24,18)은 배경 수준이어야.
        let at = |x: usize, y: usize| out[(y * 64 + x) * 3] as i32;
        assert!(at(20, 20) > 180, "정렬된 별 밝아야: {}", at(20, 20));
        assert!(at(24, 18) < 80, "정렬됐으면 이 위치는 어두워야: {}", at(24, 18));
    }

    #[test]
    fn lighten_keeps_brightest_of_two() {
        // 같은 위치의 별밭(정합 t≈0). 두 번째 프레임에 별 하나 추가 → lighten이 유지.
        let a = frame(48, 48, &[(12, 12), (36, 30), (20, 8)]);
        let b = frame(48, 48, &[(12, 12), (36, 30), (20, 8), (24, 40)]);
        let mut st = Stacker::new(48, 48, Mode::Lighten);
        assert!(st.add(&a));
        assert!(st.add(&b));
        assert_eq!(st.count(), 2);
        let out = st.render();
        // 공통 별 + b에만 있던 별 둘 다 밝게 남아야(밝은 값 누적).
        assert!(out[(12 * 48 + 12) * 3] > 180, "공통 별 유지");
        assert!(out[(40 * 48 + 24) * 3] > 180, "lighten이 b의 별도 살려야");
    }

    #[test]
    fn rejects_starless_frame() {
        let blank = vec![5u8; 64 * 48 * 3];
        let mut st = Stacker::new(64, 48, Mode::Average);
        assert!(!st.add(&blank), "별 없는 프레임은 기준으로 시작 안 함");
        assert_eq!(st.count(), 0);
    }

    #[test]
    fn estimate_rigid_recovers_rotation() {
        let refs = vec![
            Star { x: 10.0, y: 12.0, flux: 100.0 },
            Star { x: 40.0, y: 18.0, flux: 90.0 },
            Star { x: 25.0, y: 45.0, flux: 80.0 },
            Star { x: 52.0, y: 50.0, flux: 70.0 },
        ];
        let th = 0.15f32; // ~8.6°
        let (cs, sn) = (th.cos(), th.sin());
        let (tx, ty) = (3.0f32, -2.0f32);
        let cur: Vec<Star> = refs
            .iter()
            .map(|s| Star {
                x: cs * s.x - sn * s.y + tx,
                y: sn * s.x + cs * s.y + ty,
                flux: s.flux,
            })
            .collect();
        let tf = estimate_rigid(&refs, &cur, 2.0).expect("강체 정합");
        assert!((tf.angle() - th).abs() < 0.02, "angle={}", tf.angle());
        let (px, py) = tf.apply(refs[0].x, refs[0].y);
        assert!((px - cur[0].x).abs() < 0.5 && (py - cur[0].y).abs() < 0.5);
    }

    #[test]
    fn stacker_aligns_rotated_frame() {
        let (w, h) = (80usize, 64usize);
        let base = [(15.0f32, 15.0), (60.0, 20.0), (30.0, 50.0), (65.0, 55.0)];
        let a_pts: Vec<(usize, usize)> =
            base.iter().map(|&(x, y)| (x as usize, y as usize)).collect();
        let th = 0.12f32;
        let (cs, sn) = (th.cos(), th.sin());
        let (tx, ty) = (2.0f32, 1.0f32);
        let b_pts: Vec<(usize, usize)> = base
            .iter()
            .map(|&(x, y)| {
                ((cs * x - sn * y + tx).round() as usize, (sn * x + cs * y + ty).round() as usize)
            })
            .collect();
        let a = frame(w, h, &a_pts);
        let b = frame(w, h, &b_pts);
        let mut st = Stacker::new(w, h, Mode::Average);
        assert!(st.add(&a));
        assert!(st.add(&b), "회전된 프레임도 정합돼야");
        assert_eq!(st.count(), 2);
        let out = st.render();
        // 기준 위치 (15,15)의 별이 정렬 평균 후에도 밝아야(회전 보정됨).
        assert!(out[(15 * w + 15) * 3] as i32 > 150, "aligned {}", out[(15 * w + 15) * 3]);
    }
}
