//! cargo xtask：构建/拷贝/预检开发工具（spec §4 产物路径 + §决策表"插件独立编译"）。
//!
//!   cargo xtask bin                 编译 oj（release）+ 拷入 <repo>/bin/
//!   cargo xtask plugin <name>      编译 oj-<name>（release）+ 拷入 <repo>/bin/plugins/<triple>/
//!   cargo xtask plugin <name> --check   复用 PluginLoader 预检（ABI/身份/semver/符号）
//!   cargo xtask build              编译 oj + 全部第一方插件（release）并归置到 bin/
//!
//! 所有产物统一归置到 <repo>/bin/：
//!   - 主程序 oj            -> bin/oj
//!   - 插件 cdylib 构件     -> bin/plugins/<host-triple>/
//!   - DevKit 文档        -> bin/devkit/（docs/devkit + sample/global.d.ts）
//!
//! 发行布局与插件加载器默认发现路径（<exe>/plugins、<workspace_root>/bin/plugins）同形。
//!
//! --check 在本子进程跑，PluginLoader 的 forget 语义无碍（进程退出即回收）；
//! 复用 Task 3.2 同一加载入口保证预检与真实装配一致。

use only_js::bridge::plugin_loader::{PluginManifestEntry, host_context, load_manifest};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 全部第一方插件名（与 plugins/ 下 crate 对应）。
const PLUGINS: &[&str] = &[
    "es",
    "db-mysql",
    "db-postgres",
    "blob-s3",
    "bus-kafka",
    "bus-rabbitmq",
    "kv-redis",
    "auth",
];

fn root() -> PathBuf {
    // tools/xtask -> 仓库根（向上两级）。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn host_triple() -> String {
    let out = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("run rustc -vV");
    let stdout = String::from_utf8(out.stdout).expect("rustc -vV utf8");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .expect("host line in rustc -vV")
        .to_string()
}

fn bin_dir() -> PathBuf {
    root().join("bin")
}

/// 主程序可执行文件名（按平台带后缀）。
fn oj_exe_name() -> String {
    if cfg!(target_os = "windows") {
        "oj.exe".to_string()
    } else {
        "oj".to_string()
    }
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

/// 编译整个 workspace（release）。
///
/// 必须用 `--workspace` 而非 `-p oj` / `-p oj-<name>`：deno_core/v8 的 feature 归一化在
/// `-p` 构建与 `--workspace` 构建间不同，会导致 v8 被按不同 fingerprint 重编，进而 rusty_v8
/// 静态库（target/release/gn_out/obj/librusty_v8.a）找不到。`--workspace` 构建是权威的
/// release 构建路径（CLAUDE.md 亦以 `cargo build --workspace --release` 为准）。
///
/// 必须带 `--exclude xtask`：xtask 经 `cargo run -p xtask` 启动后是**运行中的进程**，其 exe
/// 在 Windows 上被锁定；而 `-p` 与 `--workspace` 的 feature 归一化不同会使 cargo 认为 xtask
/// 需要 relink，进而尝试删除运行中的 xtask.exe → "Access is denied (os error 5)"。xtask 自身
/// 也不是发行产物（bin/ 只放 oj + 插件），排除后其余成员的归一化不受影响（xtask 未对共享
/// 依赖启用额外 feature）。
fn build_workspace_release() -> Result<(), String> {
    let status = Command::new("cargo")
        // --exclude oj-cert：独立签名工具（tools/，不随发行包、不进 bin/），无需随
        // 每次 xtask 构建连带编译 rsa/clap 依赖树。
        .args([
            "build",
            "--workspace",
            "--exclude",
            "xtask",
            "--exclude",
            "oj-cert",
            "--release",
        ])
        .status()
        .map_err(|e| format!("spawn cargo build --workspace --release: {e}"))?;
    if !status.success() {
        return Err("cargo build --workspace --release failed".to_string());
    }
    Ok(())
}

/// 编译并拷贝主程序 oj -> bin/oj。
fn build_bin() -> Result<(), String> {
    build_workspace_release()?;
    let src = root().join("target").join("release").join(oj_exe_name());
    let dst = bin_dir().join(oj_exe_name());
    fs::create_dir_all(bin_dir()).map_err(|e| format!("mkdir {}: {e}", bin_dir().display()))?;
    fs::copy(&src, &dst)
        .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    println!("copied {} -> {}", src.display(), dst.display());
    Ok(())
}

fn build_and_copy(name: &str) -> Result<(), String> {
    build_workspace_release()?;
    let triple = host_triple();
    let dst_dir = bin_dir().join("plugins").join(&triple);
    fs::create_dir_all(&dst_dir).map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
    let src = root()
        .join("target")
        .join("release")
        .join(build_artifact_name(name));
    let dst = dst_dir.join(plugin_file_name(name));
    fs::copy(&src, &dst)
        .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    println!("copied {} -> {}", src.display(), dst.display());
    Ok(())
}

/// 递归拷贝目录（std 的 `fs::copy_dir_all` 尚未稳定，此处最小实现）。
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// 归置 devkit（docs/devkit 三件 + sample/global.d.ts）-> bin/devkit/。
/// 仅 `build` 全量归置时调用；`bin`/`plugin` 单体子命令不拖文档。
fn copy_devkit() -> Result<(), String> {
    let src_dir = root().join("docs").join("devkit");
    let dst_dir = bin_dir().join("devkit");
    // 旧拷贝整体替换，避免残留已从源里删除的文件。
    if dst_dir.exists() {
        fs::remove_dir_all(&dst_dir).map_err(|e| format!("rm -rf {}: {e}", dst_dir.display()))?;
    }
    copy_dir_all(&src_dir, &dst_dir)
        .map_err(|e| format!("copy {} -> {}: {e}", src_dir.display(), dst_dir.display()))?;
    let dts_src = root().join("sample").join("global.d.ts");
    let dts_dst = dst_dir.join("global.d.ts");
    fs::copy(&dts_src, &dts_dst)
        .map_err(|e| format!("copy {} -> {}: {e}", dts_src.display(), dts_dst.display()))?;
    println!("copied devkit -> {}", dst_dir.display());
    Ok(())
}

/// 预检：经 PluginLoader（与真实装配同一入口）加载校验。cfg 传 `{}`（端点配置校验在
/// 服务器装配层做，此处只验证可加载性）。
fn check(name: &str) -> Result<(), String> {
    let dir = bin_dir().join("plugins").join(host_triple());
    let manifest = vec![PluginManifestEntry {
        name: name.to_string(),
        semver_pin: None,
    }];
    let host = host_context();
    // 预检只验证可加载性（ABI/身份/semver/符号）：需要装配期 cfg 的插件给占位值，
    // 真实 cfg 由服务器装配层注入（server_cmd::plugin_cfg_json）。
    let cfg_for = |name: &str| -> String {
        match name {
            "auth" => r#"{"jwt_secret":"precheck"}"#.to_string(),
            _ => "{}".to_string(),
        }
    };
    let loaded = load_manifest(&dir, &manifest, host, &cfg_for)
        .map_err(|e| format!("precheck failed: {e}"))?;
    let d = &loaded[0].descriptor;
    println!(
        "ok: {} {} (abi {})",
        &d.name[..],
        &d.semver[..],
        d.abi_version
    );
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: cargo xtask <bin | plugin <name> [--check] | build>");
    std::process::exit(2)
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    let cmd = &args[1];
    let result: Result<(), String> = match cmd.as_str() {
        "bin" => build_bin(),
        "build" => {
            build_bin()?;
            for p in PLUGINS {
                build_and_copy(p)?;
            }
            copy_devkit()
        }
        "plugin" => {
            if args.len() < 3 {
                usage();
            }
            let name = &args[2];
            let do_check = args.iter().skip(3).any(|a| a == "--check");
            if do_check {
                check(name)
            } else {
                build_and_copy(name)
            }
        }
        _ => {
            usage();
        }
    };
    result.map_err(|e| format!("xtask error: {e}"))
}
