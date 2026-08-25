//! 烧入插件路径解析用的编译期常量：workspace root（dev 后备）与 target triple。

fn main() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-env=OJ_WORKSPACE_ROOT={}", manifest.display());
    println!(
        "cargo:rustc-env=OJ_TARGET_TRIPLE={}",
        std::env::var("TARGET").expect("TARGET set by cargo")
    );
}
