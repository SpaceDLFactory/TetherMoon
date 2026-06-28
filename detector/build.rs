// detector.mm(Obj-C++ CoreML 샘) 컴파일·링크. crsdk의 wrapper.cpp+build.rs 패턴과 동일 발상.
// macOS 전용(CoreML). 비-macOS에선 빈 빌드(이 크레이트는 옵셔널 feature로만 쓰임).
fn main() {
    if !cfg!(target_os = "macos") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .file("native/detector.mm")
        .include("native")
        .flag("-std=c++17")
        // detector.mm은 수동 retain/release → ARC 미사용(붙이면 컴파일 거부됨)
        .compile("detector");

    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=CoreML");
    println!("cargo:rerun-if-changed=native/detector.mm");
    println!("cargo:rerun-if-changed=native/detector.h");
}
