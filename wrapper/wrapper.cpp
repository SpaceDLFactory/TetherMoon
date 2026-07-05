/* wrapper.cpp — C++ implementation of wrapper.h
 *
 * Design rules:
 *  - No C++ exceptions escape into C (all caught internally).
 *  - No C++ types (strings, vectors, etc.) cross the boundary.
 *  - RustDeviceCallback null-checks every fn ptr before calling.
 *  - deactivate_device_callback() atomically zeros all fn ptrs so
 *    any in-flight SDK thread that arrives after Rust's Drop begins
 *    finds nothing to call.
 */

#include "wrapper.h"

#include "CameraRemote_SDK.h"
#include "IDeviceCallback.h"
#include "CrImageDataBlock.h"
#include "CrDeviceProperty.h"

#include <stdint.h>
#include <new>      // std::nothrow
#include <cstring>  // strlen
#include <vector>   // UTF-8 → UTF-16 conversion
#include <atomic>   // std::atomic for thread-safe callback deactivation
#if defined(_WIN32)
#include <string>    // UTF-16 ↔ UTF-8 변환 버퍼 (Windows 전용 — macOS libc++ <charconv> 회피)
#include <windows.h> // WideCharToMultiByte / MultiByteToWideChar
#endif

using namespace SCRSDK;

/* ==================================================================
 * Internal helper: CrDeviceHandle ↔ int64_t cast
 * CrDeviceHandle = CrInt64 = int64_t (confirmed from CrDefines.h)
 * ================================================================== */
static inline CrDeviceHandle to_handle(int64_t h)
{
    return static_cast<CrDeviceHandle>(h);
}

/* ==================================================================
 * RustDeviceCallback — IDeviceCallback impl wired to fn pointers
 * ================================================================== */
/* Function pointer types for callback slots */
using FnConnected     = void (*)(void*, uint32_t);
using FnDisconnected  = void (*)(void*, uint32_t);
using FnPropChanged   = void (*)(void*);
using FnLvPropChanged = void (*)(void*);
using FnWarning       = void (*)(void*, uint32_t);
using FnWarningExt    = void (*)(void*, uint32_t, int32_t, int32_t, int32_t);
using FnError         = void (*)(void*, uint32_t);
using FnDownload      = void (*)(void*, const char*, uint32_t);

struct RustDeviceCallback : public IDeviceCallback {
    virtual ~RustDeviceCallback() = default;

    std::atomic<FnConnected>     fn_connected{nullptr};
    std::atomic<FnDisconnected>  fn_disconnected{nullptr};
    std::atomic<FnPropChanged>   fn_prop_changed{nullptr};
    std::atomic<FnLvPropChanged> fn_lv_prop_changed{nullptr};
    std::atomic<FnWarning>       fn_warning{nullptr};
    std::atomic<FnWarningExt>    fn_warning_ext{nullptr};
    std::atomic<FnError>         fn_error{nullptr};
    std::atomic<FnDownload>      fn_complete_download{nullptr};
    std::atomic<void*>           user_data{nullptr};

    void OnConnected(DeviceConnectionVersioin v) override {
        auto fn = fn_connected.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud, static_cast<uint32_t>(v));
    }
    void OnDisconnected(CrInt32u e) override {
        auto fn = fn_disconnected.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud, e);
    }
    void OnPropertyChanged() override {
        auto fn = fn_prop_changed.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud);
    }
    void OnLvPropertyChanged() override {
        auto fn = fn_lv_prop_changed.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud);
    }
    void OnWarning(CrInt32u w) override {
        auto fn = fn_warning.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud, w);
    }
    void OnWarningExt(CrInt32u w,
                      CrInt32 p1, CrInt32 p2, CrInt32 p3) override {
        auto fn = fn_warning_ext.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud, w, p1, p2, p3);
    }
    void OnError(CrInt32u e) override {
        auto fn = fn_error.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud, e);
    }
    void OnCompleteDownload(CrChar* filename, CrInt32u kind) override {
        auto fn = fn_complete_download.load(std::memory_order_acquire);
        auto ud = user_data.load(std::memory_order_acquire);
        if (fn && ud) fn(ud, static_cast<const char*>(filename),
                         static_cast<uint32_t>(kind));
    }
    /* remaining virtuals have default no-op bodies in IDeviceCallback */
};

/* ==================================================================
 * Internal struct for liveview block management
 * ================================================================== */
struct LiveViewBlock {
    CrImageDataBlock block;
    uint8_t*         buf;
};

/* ==================================================================
 * SDK lifecycle
 * ================================================================== */

extern "C" {

int32_t sdk_init(int32_t log_type)
{
    bool ok = Init(static_cast<CrInt32u>(log_type));
    return ok ? 0 : -1;
}

int32_t sdk_release(void)
{
    bool ok = Release();
    return ok ? 0 : -1;
}

uint32_t get_sdk_version(void) { return GetSDKVersion(); }
uint32_t get_sdk_serial(void)  { return GetSDKSerial();  }

/* ==================================================================
 * Enumeration
 * ================================================================== */

int32_t enum_cameras(void** out_handle, uint8_t timeout_sec)
{
    ICrEnumCameraObjectInfo* p = nullptr;
    CrError err = EnumCameraObjects(&p, timeout_sec);
    *out_handle = static_cast<void*>(p);
    return static_cast<int32_t>(err);
}

uint32_t enum_get_count(const void* handle)
{
    const auto* p =
        static_cast<const ICrEnumCameraObjectInfo*>(handle);
    return p ? p->GetCount() : 0u;
}

const void* enum_get_camera_ptr(const void* handle, uint32_t index)
{
    const auto* p =
        static_cast<const ICrEnumCameraObjectInfo*>(handle);
    if (!p) return nullptr;
    return static_cast<const void*>(p->GetCameraObjectInfo(index));
}

void enum_release(void* handle)
{
    auto* p = static_cast<ICrEnumCameraObjectInfo*>(handle);
    if (p) p->Release();
}

/* ------------------------------------------------------------------
 * Camera info accessors
 * ------------------------------------------------------------------ */

#if defined(_WIN32)
// Windows의 Cr_Core.dll은 UNICODE 빌드라 CrChar* 문자열이 실제로는 UTF-16다.
// 우리 래퍼는 UNICODE 미정의(CrChar=char)로 컴파일되므로, 받은 포인터를 wchar_t*로
// 재해석해 UTF-8로 변환한다. read_cchar(Rust)가 호출 직후 즉시 복사하므로 thread_local 재사용 안전.
static const char* w_to_utf8(const char* cr)
{
    thread_local std::string buf;
    if (!cr) { buf.clear(); return nullptr; }
    const wchar_t* ws = reinterpret_cast<const wchar_t*>(cr);
    int n = WideCharToMultiByte(CP_UTF8, 0, ws, -1, nullptr, 0, nullptr, nullptr);
    if (n <= 1) { buf.clear(); return buf.c_str(); }
    buf.assign(static_cast<size_t>(n - 1), '\0');
    WideCharToMultiByte(CP_UTF8, 0, ws, -1, &buf[0], n, nullptr, nullptr);
    return buf.c_str();
}
#endif

const char* camera_get_name_ptr(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    if (!c) return nullptr;
#if defined(_WIN32)
    return w_to_utf8(c->GetName());
#else
    return c->GetName();
#endif
}

uint32_t camera_get_name_size(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    return c ? c->GetNameSize() : 0u;
}

const char* camera_get_model_ptr(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    if (!c) return nullptr;
#if defined(_WIN32)
    return w_to_utf8(c->GetModel());
#else
    return c->GetModel();
#endif
}

uint32_t camera_get_model_size(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    return c ? c->GetModelSize() : 0u;
}

uint16_t camera_get_usb_pid(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    return c ? static_cast<uint16_t>(c->GetUsbPid()) : 0u;
}

uint32_t camera_get_connection_status(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    return c ? c->GetConnectionStatus() : 0u;
}

uint32_t camera_get_ssh_support(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    return c ? c->GetSSHsupport() : 0u;
}

// 연결 타입명 문자열 (예: "USB", "ETHERNET"). 네트워크 발견 진단용.
const char* camera_get_connection_type_name_ptr(const void* cam_ptr)
{
    const auto* c =
        static_cast<const ICrCameraObjectInfo*>(cam_ptr);
    if (!c) return nullptr;
#if defined(_WIN32)
    return w_to_utf8(reinterpret_cast<const char*>(c->GetConnectionTypeName()));
#else
    return reinterpret_cast<const char*>(c->GetConnectionTypeName());
#endif
}

uint32_t camera_get_fingerprint(const void* cam_ptr, char* buf, uint32_t buf_size)
{
    auto* cam = static_cast<ICrCameraObjectInfo*>(
                    const_cast<void*>(cam_ptr));
    if (!cam || !buf || buf_size == 0) return 0u;
    CrInt32u size = buf_size;
    CrError err = GetFingerprint(cam, buf, &size);
    return (err == CrError_None) ? size : 0u;
}

/* ==================================================================
 * Callback lifecycle
 * ================================================================== */

void* create_callback(
    void (*fn_connected)         (void*, uint32_t),
    void (*fn_disconnected)      (void*, uint32_t),
    void (*fn_prop_changed)      (void*),
    void (*fn_lv_prop_changed)   (void*),
    void (*fn_warning)           (void*, uint32_t),
    void (*fn_warning_ext)       (void*, uint32_t, int32_t, int32_t, int32_t),
    void (*fn_error)             (void*, uint32_t),
    void (*fn_complete_download) (void*, const char*, uint32_t),
    void* user_data)
{
    auto* cb = new (std::nothrow) RustDeviceCallback();
    if (!cb) return nullptr;
    cb->fn_connected.store(reinterpret_cast<FnConnected>(fn_connected), std::memory_order_release);
    cb->fn_disconnected.store(reinterpret_cast<FnDisconnected>(fn_disconnected), std::memory_order_release);
    cb->fn_prop_changed.store(reinterpret_cast<FnPropChanged>(fn_prop_changed), std::memory_order_release);
    cb->fn_lv_prop_changed.store(reinterpret_cast<FnLvPropChanged>(fn_lv_prop_changed), std::memory_order_release);
    cb->fn_warning.store(reinterpret_cast<FnWarning>(fn_warning), std::memory_order_release);
    cb->fn_warning_ext.store(reinterpret_cast<FnWarningExt>(fn_warning_ext), std::memory_order_release);
    cb->fn_error.store(reinterpret_cast<FnError>(fn_error), std::memory_order_release);
    cb->fn_complete_download.store(reinterpret_cast<FnDownload>(fn_complete_download), std::memory_order_release);
    cb->user_data.store(user_data, std::memory_order_release);
    return cb;
}

void deactivate_device_callback(void* cb_ptr)
{
    auto* cb = static_cast<RustDeviceCallback*>(cb_ptr);
    if (!cb) return;
    /* Atomically null all slots so any SDK thread that fires after this
     * point calls nothing and touches no Rust memory. */
    cb->user_data.store(nullptr, std::memory_order_release);
    cb->fn_connected.store(nullptr, std::memory_order_release);
    cb->fn_disconnected.store(nullptr, std::memory_order_release);
    cb->fn_prop_changed.store(nullptr, std::memory_order_release);
    cb->fn_lv_prop_changed.store(nullptr, std::memory_order_release);
    cb->fn_warning.store(nullptr, std::memory_order_release);
    cb->fn_warning_ext.store(nullptr, std::memory_order_release);
    cb->fn_error.store(nullptr, std::memory_order_release);
    cb->fn_complete_download.store(nullptr, std::memory_order_release);
}

void destroy_callback(void* cb_ptr)
{
    delete static_cast<RustDeviceCallback*>(cb_ptr);
}

/* ==================================================================
 * Connection
 * ================================================================== */

int32_t camera_connect(
    const void* cam_ptr,
    void*       cb_ptr,
    int64_t*    out_handle,
    int32_t     open_mode,
    int32_t     reconnect,
    const char* user_id,
    const char* password,
    const char* fingerprint,
    uint32_t    fingerprint_size,
    const char* pairing_display_name)
{
    auto* cam = static_cast<ICrCameraObjectInfo*>(
                    const_cast<void*>(cam_ptr));
    auto* cb  = static_cast<IDeviceCallback*>(cb_ptr);

    // ASCII → UTF-16LE (null-terminated) for pairingDisplayName
    // SDK requires const CrInt16u* (= const uint16_t*)
    // NOTE: ASCII-only. Non-ASCII pairing names are not currently needed
    // (hardcoded "CrSDK-Rust"). Extend to full UTF-8 if that changes.
    std::vector<CrInt16u> utf16_name;
    const CrInt16u* pairing_ptr = nullptr;
    if (pairing_display_name && pairing_display_name[0] != '\0') {
        for (const char* s = pairing_display_name; *s; ++s)
            utf16_name.push_back(static_cast<CrInt16u>(
                static_cast<unsigned char>(*s)));
        utf16_name.push_back(0); // null terminator
        pairing_ptr = utf16_name.data();
    }

    CrDeviceHandle h = 0;
    CrError err = Connect(
        cam, cb, &h,
        static_cast<CrSdkControlMode>(open_mode),
        static_cast<CrReconnectingSet>(reconnect),
        user_id,
        password         ? password         : "",
        fingerprint      ? fingerprint      : "",
        fingerprint_size,
        pairing_ptr);
    *out_handle = static_cast<int64_t>(h);
    return static_cast<int32_t>(err);
}

int32_t camera_disconnect(int64_t handle)
{
    return static_cast<int32_t>(Disconnect(to_handle(handle)));
}

int32_t camera_release_device(int64_t handle)
{
    return static_cast<int32_t>(ReleaseDevice(to_handle(handle)));
}

/* ==================================================================
 * Commands
 * ================================================================== */

int32_t camera_send_command(int64_t  handle,
                             uint32_t command_id,
                             uint16_t command_param)
{
    return static_cast<int32_t>(
        SendCommand(to_handle(handle),
                    static_cast<CrInt32u>(command_id),
                    static_cast<CrCommandParam>(command_param)));
}

/* ==================================================================
 * LiveView
 * ================================================================== */

int32_t liveview_get_buffer_size(int64_t handle, uint32_t* out_buf_size)
{
    CrImageInfo info;
    CrError err = GetLiveViewImageInfo(to_handle(handle), &info);
    *out_buf_size = (err == CrError_None) ? info.GetBufferSize() : 0u;
    return static_cast<int32_t>(err);
}

void* liveview_alloc_block(uint32_t buf_size, uint8_t** out_buf)
{
    auto* lvb = new (std::nothrow) LiveViewBlock();
    if (!lvb) { *out_buf = nullptr; return nullptr; }

    lvb->buf = new (std::nothrow) uint8_t[buf_size]();
    if (!lvb->buf) { delete lvb; *out_buf = nullptr; return nullptr; }

    lvb->block.SetSize(buf_size);
    lvb->block.SetData(lvb->buf);
    *out_buf = lvb->buf;
    return static_cast<void*>(lvb);
}

int32_t liveview_fetch(int64_t handle, void* block_ptr,
                        uint32_t* out_image_size,
                        const uint8_t** out_image_data)
{
    auto* lvb = static_cast<LiveViewBlock*>(block_ptr);
    CrError err = GetLiveViewImage(to_handle(handle), &lvb->block);
    if (err == CrError_None) {
        *out_image_size  = lvb->block.GetImageSize();
        *out_image_data  = lvb->block.GetImageData();
    } else {
        *out_image_size  = 0;
        *out_image_data  = nullptr;
    }
    return static_cast<int32_t>(err);
}

void liveview_free_block(void* block_ptr, uint8_t* buf)
{
    delete static_cast<LiveViewBlock*>(block_ptr);
    delete[] buf;
}

int32_t liveview_get_level(int64_t handle, CrLevelSimple* out)
{
    out->state = 0; out->x = 0; out->y = 0; out->z = 0;
    CrInt32 num = 0;
    CrLiveViewProperty* props = nullptr;
    CrInt32u code = CrLiveViewProperty_Level;
    CrError err = GetSelectLiveViewProperties(to_handle(handle), 1, &code, &props, &num);
    if (err != CrError_None) return static_cast<int32_t>(err);
    if (props && num >= 1 && props[0].GetFrameInfoType() == CrFrameInfoType_Level) {
        auto* info = (CrLevelInfo*)props[0].GetValue();
        if (info) {
            out->state = static_cast<int32_t>(info->state);
            out->x = info->x;
            out->y = info->y;
            out->z = info->z;
        }
    }
    if (props) ReleaseLiveViewProperties(to_handle(handle), props);
    return 0;
}

int32_t liveview_get_af_frame(int64_t handle, CrAfFrameSimple* out)
{
    out->valid = 0; out->x_num = 0; out->x_deno = 0; out->y_num = 0;
    out->y_deno = 0; out->width = 0; out->height = 0;
    CrInt32 num = 0;
    CrLiveViewProperty* props = nullptr;
    CrInt32u code = CrLiveViewProperty_AF_Area_Position;
    CrError err = GetSelectLiveViewProperties(to_handle(handle), 1, &code, &props, &num);
    if (err != CrError_None) return static_cast<int32_t>(err);
    if (props && num >= 1 && props[0].GetFrameInfoType() == CrFrameInfoType_FocusFrameInfo) {
        int sz = static_cast<int>(props[0].GetValueSize());
        int count = sz / static_cast<int>(sizeof(CrFocusFrameInfo));
        auto* fi = (CrFocusFrameInfo*)props[0].GetValue();
        if (fi && count >= 1) {
            out->valid   = 1;
            out->x_num   = fi[0].xNumerator;
            out->x_deno  = fi[0].xDenominator;
            out->y_num   = fi[0].yNumerator;
            out->y_deno  = fi[0].yDenominator;
            out->width   = fi[0].width;
            out->height  = fi[0].height;
        }
    }
    if (props) ReleaseLiveViewProperties(to_handle(handle), props);
    return 0;
}

/* ==================================================================
 * Device properties
 * ================================================================== */

int32_t get_device_properties(int64_t           handle,
                               CrPropertySimple** out_props,
                               uint32_t*          out_count)
{
    *out_props = nullptr;
    *out_count = 0;

    CrDeviceProperty* sdk_props = nullptr;
    CrInt32 count = 0;
    CrError err = GetDeviceProperties(to_handle(handle),
                                      &sdk_props, &count);
    if (err != CrError_None || count <= 0 || !sdk_props) {
        return static_cast<int32_t>(err);
    }

    auto* flat = new (std::nothrow) CrPropertySimple[
                     static_cast<size_t>(count)];
    if (!flat) {
        ReleaseDeviceProperties(to_handle(handle), sdk_props);
        return -1;
    }

    for (CrInt32 i = 0; i < count; ++i) {
        flat[i].code          = sdk_props[i].GetCode();
        flat[i].value_type    = static_cast<uint32_t>(
                                    sdk_props[i].GetValueType());
        flat[i].current_value = sdk_props[i].GetCurrentValue();

        /* is_editable: SDK reports whether this property accepts writes now */
        flat[i].is_editable = sdk_props[i].IsSetEnableCurrentValue() ? 1u : 0u;

        /* Allowed/settable values.
         * GetSetValueSize() → element count (for array types) or byte length
         * (for string types).  GetValues() → raw byte pointer.
         *
         * CrDataType layout:
         *   ArrayBit = 0x2000; base = dt & ~0x2000
         *   0x0001/0x0005 → UInt8/Int8   (1 B)
         *   0x0002/0x0006 → UInt16/Int16  (2 B)
         *   0x0003/0x0007 → UInt32/Int32  (4 B)
         *   0x0004/0x0008 → UInt64/Int64  (8 B)
         *   0x000B        → STR (string)  — skip, not an enumerable array
         *
         * Only array types (dt & 0x2000) have meaningful allowed_values. */
        /* allowed_values 파싱.
         *   Array(0x2000): 설정 가능한 원소들 (GetSetValueSize 개).
         *   Range(0x4000): [min, max, step] (GetValueSize, 실측 A7C 색온도; 뒤는 0 패딩). 슬라이더 범위용.
         *   String(0x000B)/스칼라: 파싱 안 함.
         * (값 타입은 base nibble만; Sign/Array/Range/Custom 비트는 esz에 무관) */
        flat[i].allowed_count = 0;
        uint32_t dt   = flat[i].value_type;
        uint32_t base = dt & 0x000Fu;
        size_t   esz  = (base == 1) ? 1u : (base == 2) ? 2u : (base == 3) ? 4u : 8u;

        uint32_t nbytes = 0;
        if (base != 0x000Bu) {
            if (dt & 0x2000u)      nbytes = sdk_props[i].GetSetValueSize(); /* array: 설정가능 값 배열 바이트 크기 */
            else if (dt & 0x4000u) nbytes = sdk_props[i].GetValueSize();    /* range: [min,max,step] 바이트 크기 */
        }
        /* GetSetValueSize/GetValueSize = 원소 수가 아니라 바이트 크기 → 원소 수 = bytes/esz.
           바이트를 원소 수로 쓰면 esz>1(2·4바이트) 타입에서 esz배 과다 루프 = 힙 오버리드 +
           allowed에 쓰레기값 유입(프론트가 라벨 디코드 실패로 걸러 시각상 안 보였음). A7C 하드웨어 확정. */
        uint32_t n = (esz > 0) ? nbytes / (uint32_t)esz : 0;
        if (n > 0) {
            const uint8_t* raw =
                static_cast<const uint8_t*>(sdk_props[i].GetValues());
            if (raw) {
                uint32_t cap = (n < CR_PROPERTY_MAX_ALLOWED)
                                ? n
                                : (uint32_t)CR_PROPERTY_MAX_ALLOWED;
                for (uint32_t j = 0; j < cap; ++j) {
                    uint64_t v = 0;
                    switch (esz) {
                        case 1: v = raw[j]; break;
                        case 2: { uint16_t t; memcpy(&t, raw + j * 2, 2); v = t; } break;
                        case 4: { uint32_t t; memcpy(&t, raw + j * 4, 4); v = t; } break;
                        default: memcpy(&v, raw + j * 8, 8); break;
                    }
                    flat[i].allowed_values[j] = v;
                }
                flat[i].allowed_count = cap;
            }
        }
    }

    ReleaseDeviceProperties(to_handle(handle), sdk_props);

    *out_props = flat;
    *out_count = static_cast<uint32_t>(count);
    return 0;
}

int32_t release_device_properties_simple(CrPropertySimple* props)
{
    delete[] props;
    return 0;
}

int32_t set_device_property(int64_t handle,
                             const CrPropertySimple* prop)
{
    /* Fetch-Modify-Set pattern:
     *
     * Constructing a bare CrDeviceProperty and calling SetDeviceProperty
     * with it technically works (the C++ default-ctor + SetCode/SetValueType/
     * SetCurrentValue path is valid), but some firmware revisions reject
     * property objects that weren't originally retrieved from the device —
     * they cross-check internal state bits we cannot reconstruct.
     *
     * Safer approach: re-fetch the live array, find the matching entry,
     * overwrite just current_value, and pass the original object back.
     * Side effect: we also get a definitive "property not found" error. */

    CrDeviceProperty* sdk_props = nullptr;
    CrInt32 count = 0;
    CrError fetch_err = GetDeviceProperties(to_handle(handle),
                                            &sdk_props, &count);
    if (fetch_err != CrError_None || count <= 0 || !sdk_props) {
        if (sdk_props)
            ReleaseDeviceProperties(to_handle(handle), sdk_props);
        return static_cast<int32_t>(fetch_err != CrError_None
                                    ? fetch_err : -1);
    }

    CrError set_err = static_cast<CrError>(-1); /* property not found */
    for (CrInt32 i = 0; i < count; ++i) {
        if (sdk_props[i].GetCode() ==
                static_cast<CrDevicePropertyCode>(prop->code)) {
            sdk_props[i].SetCurrentValue(prop->current_value);
            set_err = SetDeviceProperty(to_handle(handle), &sdk_props[i]);
            break;
        }
    }

    ReleaseDeviceProperties(to_handle(handle), sdk_props);
    return static_cast<int32_t>(set_err);
}

/* ==================================================================
 * Device settings / save path
 * ================================================================== */

int32_t get_device_setting(int64_t handle, uint32_t key,
                            uint32_t* value)
{
    return static_cast<int32_t>(
        GetDeviceSetting(to_handle(handle),
                         static_cast<CrInt32u>(key),
                         reinterpret_cast<CrInt32u*>(value)));
}

int32_t set_device_setting(int64_t handle, uint32_t key,
                            uint32_t value)
{
    return static_cast<int32_t>(
        SetDeviceSetting(to_handle(handle),
                         static_cast<CrInt32u>(key),
                         static_cast<CrInt32u>(value)));
}

int32_t set_save_info(int64_t handle, const char* path,
                       const char* prefix, int32_t no)
{
#if defined(_WIN32)
    // DLL은 CrChar=wchar_t를 기대하므로 UTF-8(char*) → UTF-16(wchar_t*) 변환 후 전달.
    auto to_w = [](const char* s) -> std::wstring {
        if (!s) return std::wstring();
        int n = MultiByteToWideChar(CP_UTF8, 0, s, -1, nullptr, 0);
        if (n <= 1) return std::wstring();
        std::wstring w(static_cast<size_t>(n - 1), L'\0');
        MultiByteToWideChar(CP_UTF8, 0, s, -1, &w[0], n);
        return w;
    };
    std::wstring wpath = to_w(path);
    std::wstring wprefix = to_w(prefix);
    return static_cast<int32_t>(
        SetSaveInfo(to_handle(handle),
                    reinterpret_cast<CrChar*>(const_cast<wchar_t*>(wpath.c_str())),
                    reinterpret_cast<CrChar*>(const_cast<wchar_t*>(wprefix.c_str())),
                    static_cast<CrInt32>(no)));
#else
    return static_cast<int32_t>(
        SetSaveInfo(to_handle(handle),
                    const_cast<CrChar*>(path),
                    const_cast<CrChar*>(prefix),
                    static_cast<CrInt32>(no)));
#endif
}

int32_t execute_control_code_value(int64_t handle, uint32_t code, uint64_t value)
{
    return static_cast<int32_t>(
        ExecuteControlCodeValue(to_handle(handle),
                                static_cast<CrControlCode>(code),
                                static_cast<CrInt64u>(value)));
}

int32_t get_control_code_info(int64_t handle, uint32_t code, CrControlInfoSimple* out)
{
    out->value_type = 0;
    out->count = 0;
    for (uint32_t i = 0; i < CR_CONTROL_MAX_VALUES; ++i) out->values[i] = 0;

    CrControlCodeInfo* info = nullptr;
    CrError err = GetSelectControlCode(to_handle(handle),
                                       static_cast<CrControlCode>(code),
                                       &info);
    if (err != CrError_None || !info) {
        if (info) ReleaseControlCodes(to_handle(handle), info);
        return static_cast<int32_t>(err);
    }

    uint32_t dt = static_cast<uint32_t>(info->GetValueType());
    out->value_type = dt;

    /* base 타입의 원소 바이트 크기 (1=u8, 2=u16, 3=u32, 4=u64).
     * SignBit/ArrayBit/RangeBit/CustomBit 은 무시하고 하위 nibble만. */
    uint32_t base = dt & 0x000Fu;
    size_t esz = (base == 1) ? 1u
               : (base == 2) ? 2u
               : (base == 3) ? 4u
               : 8u;

    /* GetValueSize: Range 면 3(min/step/max), Array 면 원소 수. */
    uint32_t vsize = info->GetValueSize();
    uint32_t cap = (vsize < CR_CONTROL_MAX_VALUES) ? vsize : CR_CONTROL_MAX_VALUES;
    const uint8_t* raw = info->GetValues();
    if (raw) {
        for (uint32_t i = 0; i < cap; ++i) {
            uint64_t v = 0;
            switch (esz) {
                case 1: v = raw[i]; break;
                case 2: { uint16_t t; memcpy(&t, raw + i * 2, 2); v = t; } break;
                case 4: { uint32_t t; memcpy(&t, raw + i * 4, 4); v = t; } break;
                default: memcpy(&v, raw + i * 8, 8); break;
            }
            out->values[i] = v;
        }
        out->count = cap;
    }

    ReleaseControlCodes(to_handle(handle), info);
    return 0;
}

int32_t get_property_string(int64_t handle, uint32_t code, char* buf, uint32_t buf_size)
{
    if (!buf || buf_size == 0) return 0;
    buf[0] = '\0';

    CrDeviceProperty* sdk_props = nullptr;
    CrInt32 count = 0;
    CrError err = GetDeviceProperties(to_handle(handle), &sdk_props, &count);
    if (err != CrError_None || count <= 0 || !sdk_props) {
        if (sdk_props) ReleaseDeviceProperties(to_handle(handle), sdk_props);
        return 0;
    }

    int32_t out = 0;
    for (CrInt32 i = 0; i < count; ++i) {
        if (static_cast<uint32_t>(sdk_props[i].GetCode()) != code) continue;
        CrInt16u* u16 = sdk_props[i].GetCurrentStr();
        if (!u16) break;
        // UTF-16 → ASCII (비-ASCII 는 '?'). 길이 제한.
        uint32_t pos = 0;
        while (u16[pos] != 0 && static_cast<uint32_t>(out) + 1 < buf_size) {
            uint16_t cp = u16[pos++];
            buf[out++] = (cp < 0x80) ? static_cast<char>(cp) : '?';
        }
        buf[out] = '\0';
        break;
    }
    ReleaseDeviceProperties(to_handle(handle), sdk_props);
    return out;
}

} /* extern "C" */
