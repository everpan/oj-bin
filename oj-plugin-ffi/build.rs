//! 把 rustc 版本与 target triple 烧进构建指纹（HOST_FINGERPRINT 用，诊断而非门禁）。

fn main() {
    let out = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .expect("invoke rustc --version");
    println!(
        "cargo:rustc-env=CONST_RUSTC_VERSION={}",
        String::from_utf8_lossy(&out.stdout).trim()
    );
    println!(
        "cargo:rustc-env=CONST_TARGET_TRIPLE={}",
        std::env::var("TARGET").expect("TARGET set by cargo")
    );
}
