// crsdk_server/src/state.rs — main.rs에서 기능 계통별로 분리 (동작 불변)
use std::sync::{Arc, OnceLock};


use crsdk::{
    Camera,
    SdkSession,
};
use tokio::sync::{broadcast, Mutex};

// ── SDK 세션 'static화 ───────────────────────────────────────────────────
// Camera<'session>의 lifetime은 SdkSession을 따른다. Arc/Mutex에 담으려면
// 'static이 필요하므로 OnceLock으로 프로세스 수명만큼 살린다.
pub(crate) static SESSION: OnceLock<SdkSession> = OnceLock::new();

pub(crate) fn sdk_session() -> &'static SdkSession {
    SESSION.get_or_init(|| SdkSession::new(0).expect("SDK init"))
}

// ── Camera Send 어댑터 ──────────────────────────────────────────────────
// crsdk::Camera는 내부 DeviceCallback에 *mut c_void 를 들고 있어 기본적으로
// !Send이다. 그러나 그 포인터가 가리키는 C++ RustDeviceCallback의 모든 함수
// 슬롯은 std::atomic으로 보호되며, 객체 자체는 힙에서 절대 이동하지 않는다.
// 따라서 Camera 자체를 다른 스레드로 옮기는 것은 안전하다. crsdk lib을
// 건드리지 않기 위해 server 안에서만 newtype으로 unsafe impl Send.
pub(crate) struct CameraCell(pub(crate) Camera<'static>, pub(crate) String, pub(crate) String); // (camera, model명, lens_model)
unsafe impl Send for CameraCell {}

// ── 검출기(RT-DETR CoreML, 옵셔널) ───────────────────────────────────────
/// TETHERMOON_DETECTOR_MODEL(.mlpackage 경로) 환경변수로 1회 로드. 없거나 실패 시 None.
#[cfg(feature = "detector")]
pub(crate) fn load_detector() -> Option<Arc<detector::Detector>> {
    let path = std::env::var("TETHERMOON_DETECTOR_MODEL").ok()?;
    match detector::Detector::new(&path) {
        Some(d) => {
            tracing::info!("detector loaded: {path}");
            Some(Arc::new(d))
        }
        None => {
            tracing::warn!("detector model load failed: {path}");
            None
        }
    }
}

// ── App state ──────────────────────────────────────────────────────────
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) camera: Arc<Mutex<Option<CameraCell>>>,
    pub(crate) save_path: Arc<Mutex<String>>,
    pub(crate) events_tx: broadcast::Sender<String>, // JSON으로 직렬화된 CameraEvent fan-out
    pub(crate) last_image: Arc<Mutex<Option<String>>>, // 마지막 PC 저장 파일 경로 (미리보기)
    pub(crate) bulb_active: Arc<std::sync::atomic::AtomicBool>, // 벌브 타이머 노출 진행중 (중복 트리거 방지)
    pub(crate) interval_active: Arc<std::sync::atomic::AtomicBool>, // 인터벌 촬영 진행중 (단일 실행 가드, 소유자만 해제)
    pub(crate) interval_cancel: Arc<std::sync::atomic::AtomicBool>, // 인터벌 취소 신호 (stop이 set, 루프가 관측)
    pub(crate) lv_tx: broadcast::Sender<Arc<Vec<u8>>>, // LiveView 프레임 fan-out (다중 클라이언트)
    pub(crate) lv_running: Arc<std::sync::Mutex<bool>>, // LiveView 프로듀서 가동 여부 (시작/종료 race 방지용 락)
    pub(crate) af_active: Arc<std::sync::atomic::AtomicBool>, // SW-AF 스윕 진행중 (단일 실행 가드, 소유자만 해제)
    pub(crate) af_cancel: Arc<std::sync::atomic::AtomicBool>, // SW-AF 취소 신호 (cancel이 set, 스윕이 관측)
    pub(crate) af_target: Arc<std::sync::Mutex<(f64, f64, f64, f64)>>, // 추적AF 대상 ROI(cx,cy,w,h) — retarget이 갱신, 연속 루프가 매 사이클 관측
    pub(crate) me_active: Arc<std::sync::atomic::AtomicBool>, // 다중노출 시퀀스 진행중 (중복 트리거 방지)
    pub(crate) connecting: Arc<std::sync::atomic::AtomicBool>, // 연결 시도 진행중 (동시 connect 직렬화)
    pub(crate) stack_active: Arc<std::sync::atomic::AtomicBool>, // 라이브스택 세션 진행중 (단일 실행 가드)
    pub(crate) stack_cancel: Arc<std::sync::atomic::AtomicBool>, // 라이브스택 정지 신호
    pub(crate) stack_count: Arc<std::sync::atomic::AtomicU32>, // 현재까지 누적된 프레임 수
    pub(crate) stack_preview: Arc<Mutex<Option<Vec<u8>>>>, // 최신 스택본 JPEG (프리뷰 폴링용)
    #[cfg(feature = "detector")]
    pub(crate) detector: Option<Arc<detector::Detector>>, // RT-DETR CoreML(추적AF, 옵셔널). 모델 미로드시 None
}

/// 단일실행 AtomicBool 가드: 드롭 시 반드시 false로 되돌린다. 핸들러 future가 정상/에러
/// 종료뿐 아니라 **클라이언트 연결 끊김으로 취소(드롭)**될 때도 플래그를 해제해, 취소
/// 라우트가 없는 인라인 작업이 "already running"으로 영구 잠기는 것을 막는다.
pub(crate) struct RunGuard(pub(crate) Arc<std::sync::atomic::AtomicBool>);
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

