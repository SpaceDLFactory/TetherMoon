// src/connection.rs — RAII camera connection
//
// Drop order (critical for Use-After-Free safety):
//   1. deactivate_device_callback  ← silences SDK background thread
//   2. camera_disconnect
//   3. camera_release_device
//   4. DeviceCallback drops        ← destroy_callback + Box<CallbackInner>

use std::ffi::{c_void, CString};
use std::marker::PhantomData;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::callback::{CameraEvent, DeviceCallback};
use crate::error::{CrErrorCode, SdkError, SdkResult};
use crate::ffi;
use crate::session::SdkSession;

/// USB PTP 또는 WiFi(SSH) 연결 모드 선택.
pub enum ConnectMode<'a> {
    /// USB PTP — 인증 없음
    Usb,
    /// WiFi/IP — SSH 비밀번호 + 호스트 키 핑거프린트 필요
    Wifi {
        /// SSH 비밀번호 (카메라 설정 화면의 비밀번호)
        password: &'a str,
        /// `CameraEnumerator::get_fingerprint()`로 미리 가져온 SSH 호스트 키
        fingerprint: &'a [u8],
        /// 카메라 LCD에 표시할 기기 이름 (최초 페어링 시).
        /// 예: "CrSDK-Rust". 이미 페어링된 경우 무시됨.
        pairing_name: &'a str,
    },
}

pub struct Camera<'session> {
    handle:   i64,
    callback: DeviceCallback,
    events:   Option<mpsc::Receiver<CameraEvent>>,
    _session: PhantomData<&'session SdkSession>,
}

impl<'session> Camera<'session> {
    /// Connect to the camera at `cam_ptr` (from `CameraEnumerator::camera_ptr`).
    /// Blocks until `OnConnected` arrives or `timeout` elapses.
    ///
    /// # Why the loop?
    /// The SDK fires events asynchronously.  Hardware may emit
    /// `OnPropertyChanged` (battery status, etc.) *before* `OnConnected`
    /// arrives.  A single `recv_timeout` would treat those early events as
    /// connection failures.  We loop and skip everything that isn't
    /// `Connected` or `Error`, honouring the original deadline.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn connect(
        _session: &'session SdkSession,
        cam_ptr:  *const c_void,
        timeout:  Duration,
        mode:     ConnectMode<'_>,
    ) -> SdkResult<Self> {
        let (callback, rx) = DeviceCallback::new();

        // CString 수명을 connect 호출 전체에 걸쳐 유지
        let (user_id, password, fp_ptr, fp_len, pairing) = match &mode {
            ConnectMode::Usb => (
                CString::new("admin").unwrap(),
                CString::new("").unwrap(),
                std::ptr::null::<u8>(),
                0u32,
                CString::new("").unwrap(),
            ),
            ConnectMode::Wifi { password, fingerprint, pairing_name } => (
                CString::new("admin").unwrap(),
                CString::new(*password).unwrap(),
                fingerprint.as_ptr(),
                fingerprint.len() as u32,
                CString::new(*pairing_name).unwrap(),
            ),
        };

        let mut handle: i64 = 0;
        let err = unsafe {
            ffi::camera_connect(
                cam_ptr,
                callback.cb_ptr,
                &mut handle,
                0, // CrSdkControlMode_Remote
                1, // CrReconnecting_ON
                user_id.as_ptr(),
                password.as_ptr(),
                fp_ptr as *const std::os::raw::c_char,
                fp_len,
                pairing.as_ptr(),
            )
        };
        if CrErrorCode(err).is_error() {
            return Err(SdkError::ConnectFailed(CrErrorCode(err)));
        }

        // handle is live and the callback is registered in the SDK now. Wrap it in the
        // RAII Camera *before* the wait loop so any early return (timeout/error) runs the
        // ordered Drop teardown (deactivate→disconnect→release) instead of leaking a
        // registered callback + device handle (the SDK background thread could otherwise
        // call the just-freed callback → UAF).
        let cam = Self {
            handle,
            callback,
            events: Some(rx),
            _session: PhantomData,
        };

        // Wait for OnConnected, discarding spurious events (PropertyChanged,
        // LvPropertyChanged, Warning, ...) that may arrive first.
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(SdkError::ConnectTimeout)?; // drops `cam` → ordered teardown
            match cam.events.as_ref().unwrap().recv_timeout(remaining) {
                Ok(CameraEvent::Connected { .. }) => break,
                Ok(CameraEvent::Error(e)) => {
                    return Err(SdkError::ConnectFailed(CrErrorCode(e as i32)));
                }
                Ok(_) => continue, // spurious event — keep waiting
                Err(_) => return Err(SdkError::ConnectTimeout),
            }
        }

        Ok(cam)
    }

    pub fn device_handle(&self) -> i64 { self.handle }

    /// Access the raw event receiver (PropertyChanged notifications etc.).
    /// Panics if the receiver was already taken via `take_events`.
    pub fn events(&mut self) -> &mut mpsc::Receiver<CameraEvent> {
        self.events.as_mut().expect("event receiver was taken")
    }

    /// Move the event receiver out (first call returns it, later calls None).
    /// Lets a background task drain SDK events without holding the camera lock.
    /// The sender lives in `DeviceCallback`, so events keep flowing into it.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<CameraEvent>> {
        self.events.take()
    }
}

impl<'session> Drop for Camera<'session> {
    fn drop(&mut self) {
        unsafe {
            // 1. Null all C fn ptrs → lingering SDK callbacks become no-ops
            ffi::deactivate_device_callback(self.callback.cb_ptr);
            // 2. Disconnect
            ffi::camera_disconnect(self.handle);
            // 3. Release device handle
            ffi::camera_release_device(self.handle);
            // 4. DeviceCallback drops (destroy_callback + Box<CallbackInner>)
        }
    }
}

/// PC 저장 경로 설정 (SDK::SetSaveInfo).
///
/// `StillImageStoreDestination`이 HostPC를 포함하면 다운로드된 이미지가 `dir`에
/// 저장되고, 완료 시 `OnCompleteDownload(filename)` 콜백이 발생한다.
/// `start_no = -1`은 자동 번호(공식 샘플 `ImageSaveAutoStartNo`).
///
/// 공식 RemoteCli 샘플(`CameraDevice::set_save_info`)은 connect 직후 무조건
/// 호출하므로, 연결 후 한 번 설정해 두는 것이 스펙에 맞는 사용법이다.
pub fn set_save_info(handle: i64, dir: &str, prefix: &str, start_no: i32) -> SdkResult<()> {
    let c_dir = CString::new(dir).map_err(|_| SdkError::StringConversion)?;
    let c_prefix = CString::new(prefix).map_err(|_| SdkError::StringConversion)?;
    let err = unsafe {
        ffi::set_save_info(handle, c_dir.as_ptr(), c_prefix.as_ptr(), start_no)
    };
    let code = CrErrorCode(err);
    if code.is_error() && !code.is_warning() {
        Err(SdkError::Sdk(code))
    } else {
        Ok(())
    }
}
