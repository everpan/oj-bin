//! cargo xtask：插件构建/拷贝/预检开发工具（spec §4 产物路径 + §决策表"插件独立编译"）。
//!
//!   cargo xtask plugin <name>         编译 oj-<name>（release）+ 拷入 <repo>/plugins/<triple>/
//!   cargo xtask plugin <name> --check 复用 PluginLoader 预检（ABI/身份/semver/符号）
//!
//! --check 在本子进程跑，PluginLoader 的 forget 语义无碍（进程退出即回收）；
//! 复用 Task 3.2 同一加载入口保证预检与真实装配一致。

use mdm_base_rust::bridge::plugin_loader::{host_context, load_manifest, PluginManifestEntry};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn host_triple() -> String {
    let out = Command::new("rustc").arg("-vV").output().expect("run rustc -vV");
    let stdout = String::from_utf8(out.stdout).expect("rustc -vV utf8");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .expect("host line in rustc -vV")
        .to_string()
}

/// 编译产物名（crate `oj-<name>` 的 rustc 产物名，`-`→`_`）。
fn build_artifact_name(name: &str) -> String {
    let lib = format!("oj_{}", name.replace('-', "_"));
    if cfg!(target_os = "windows") {
        format!("{lib}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{lib}.dylib")
    } else {
        format!("lib{lib}.so")
    }
}

/// 插件存放文件名（= loader `plugin_file_name`，以 descriptor.name 为名，与产物名解耦）。
fn plugin_file_name(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

fn build_and_copy(name: &str) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(["build", "-p", &format!("oj-{name}"), "--release"])
        .status()
        .map_err(|e| format!("spawn cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build -p oj-{name} --release failed"));
    }
    let triple = host_triple();
    let src = root().join("target").join("release").join(build_artifact_name(name));
    let dst_dir = root().join("plugins").join(&triple);
    fs::create_dir_all(&dst_dir).map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
    let dst = dst_dir.join(plugin_file_name(name));
    fs::copy(&src, &dst).map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    println!("copied {} -> {}", src.display(), dst.display());
    Ok(())
}

/// 预检：经 PluginLoader（与真实装配同一入口）加载校验。cfg 传 `{}`（端点配置校验在
/// 服务器装配层做，此处只验证可加载性）。
fn check(name: &str) -> Result<(), String> {
    let dir = root().join("plugins").join(host_triple());
    let manifest = vec![PluginManifestEntry { name: name.to_string(), semver_pin: None }];
    let host = host_context();
    let loaded = load_manifest(&dir, &manifest, host, &|_| "{}".to_string())
        .map_err(|e| format!("precheck failed: {e}"))?;
    let d = &loaded[0].descriptor;
    println!("ok: {} {} (abi {})", &d.name[..], &d.semver[..], d.abi_version);
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: cargo xtask plugin <name> [--check]");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args[1] != "plugin" {
        usage();
    }
    if args.len() < 3 {
        usage();
    }
    let name = &args[2];
    let do_check = args.iter().skip(3).any(|a| a == "--check");
    if let Err(e) = if do_check { check(name) } else { build_and_copy(name) } {
        eprintln!("xtask error: {e}");
        std::process::exit(1);
    }
}
