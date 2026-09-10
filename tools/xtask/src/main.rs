//! cargo xtask：构建/拷贝/预检开发工具（spec §4 产物路径 + §决策表"插件独立编译"）。
//!
//!   cargo xtask bin                 编译 oj（release）+ 拷入 <repo>/bin/
//!   cargo xtask plugin <name>      编译 oj-<name>（release）+ 拷入 <repo>/bin/plugins/<triple>/
//!   cargo xtask plugin <name> --check   复用 PluginLoader 预检（ABI/身份/semver/按轴符号探测，
//!                                       输出 desc 与 provided axes）
//!   cargo xtask build              编译 oj + 全部第一方插件（release）并归置到 bin/
//!   cargo xtask smoke --bin <oj>   发行门禁：把构建机才有的 JS 源临时改名后跑最小
//!                                  `oj build`，验证产物不依赖构建机绝对路径
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

use only_js::bridge::plugin_loader::{AXES, PluginManifestEntry, host_context, load_manifest};
use std::env;
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

/// 以绝对路径声明扩展 JS 的依赖 crate（与根 crate `build.rs` 的 `CRATES` 同步）。
/// `cargo xtask smoke` 需要把它们从磁盘上「拿走」，以模拟非构建机环境。
const SMOKE_CRATES: [&str; 5] = [
    "deno_web",
    "deno_fetch",
    "deno_net",
    "deno_websocket",
    "deno_webidl",
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

/// 可执行产物的落盘拷贝：写临时文件后 rename 换 vnode。macOS 对已签名 Mach-O
/// 的就地截断改写会使该 vnode 的代码签名缓存永久失效——execve 直接 SIGKILL
/// （zsh: killed），而 codesign -v 读盘校验却通过，极难排查。
fn copy_bin(src: &Path, dst: &Path) -> Result<(), String> {
    let tmp = dst.with_extension("tmp");
    fs::copy(src, &tmp)
        .and_then(|_| fs::rename(&tmp, dst))
        .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
    println!("copied {} -> {}", src.display(), dst.display());
    Ok(())
}

/// 编译并拷贝主程序 oj -> bin/oj。
fn build_bin() -> Result<(), String> {
    build_workspace_release()?;
    let src = root().join("target").join("release").join(oj_exe_name());
    let dst = bin_dir().join(oj_exe_name());
    fs::create_dir_all(bin_dir()).map_err(|e| format!("mkdir {}: {e}", bin_dir().display()))?;
    copy_bin(&src, &dst)?;
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
    copy_bin(&src, &dst)?;
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

/// 归置 devkit（docs/devkit 三件 + docs/oidc-{integration,implementation}.md +
/// sample/global.d.ts）-> bin/devkit/。
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
    // OIDC 手册随包分发（api-manual §8 引用了它们；文件名保持与仓库 docs/ 一致）。
    for name in ["oidc-integration.md", "oidc-implementation.md"] {
        let src = root().join("docs").join(name);
        fs::copy(&src, dst_dir.join(name))
            .map_err(|e| format!("copy {} -> devkit: {e}", src.display()))?;
    }
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
    // 预检只验证可加载性（ABI/身份/semver/按轴符号探测）：需要装配期 cfg 的插件给占位值，
    // 真实 cfg 由服务器装配层注入（server_cmd::plugin_cfg）。
    let cfg_for = |name: &str| -> String {
        match name {
            "auth" => r#"{"jwt_secret":"precheck"}"#.to_string(),
            _ => "{}".to_string(),
        }
    };
    let loaded = load_manifest(&dir, &manifest, host, &cfg_for)
        .map_err(|e| format!("precheck failed: {e}"))?;
    let p = &loaded[0];
    let d = &p.descriptor;
    // registrations 由加载期 AXES 逐轴 dlsym 探测填充（plugin_loader::probe_axes），
    // 此处只按 AXES 顺序汇总为可读清单。
    let provided: Vec<&str> = AXES
        .iter()
        .copied()
        .filter(|a| match *a {
            "es" => p.registrations.es.is_some(),
            "db" => p.registrations.db.is_some(),
            "blob" => p.registrations.blob.is_some(),
            "bus" => p.registrations.bus.is_some(),
            "kv" => p.registrations.kv.is_some(),
            "auth" => p.registrations.auth.is_some(),
            _ => unreachable!("AXES 与 check 汇总分支不同步"),
        })
        .collect();
    println!(
        "ok: {} {} (abi {}) — {}",
        &d.name[..],
        &d.semver[..],
        d.abi_version,
        &d.desc[..]
    );
    println!("provided axes: [{}]", provided.join(", "));
    Ok(())
}

/// 发行门禁：验证打包二进制在「构建机才有的 JS 源」缺席时仍能初始化 JsRuntime。
///
/// 手段：把 deno_* 依赖源码目录与两个自身 bootstrap 临时改名，跑一次最小 `oj build`
/// （introspect 路径必然初始化 JsRuntime）——任何残留的 `LoadedFromFsDuringSnapshot`
/// 依赖都会在此 ENOENT；内嵌实现不受影响。改动见 CHANGELIST v0.1.12。
fn smoke(bin: &Path) -> Result<(), String> {
    let bin = fs::canonicalize(bin).map_err(|e| format!("resolve {}: {e}", bin.display()))?;
    if !bin.is_file() {
        return Err(format!("smoke: 不是文件：{}", bin.display()));
    }

    let probe = env::temp_dir().join(format!("oj-smoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&probe);
    let src = probe.join("src").join("web").join("hello");
    fs::create_dir_all(&src).map_err(|e| format!("mkdir {}: {e}", src.display()))?;
    fs::write(
        src.join("api.ts"),
        "export default { get() { json.ok({ ok: true }); } };\n",
    )
    .map_err(|e| format!("write api.ts: {e}"))?;
    fs::write(
        probe.join("src").join("web").join("manifest.yaml"),
        "name: web\ndesc: probe\nversion: 0.1.0\n",
    )
    .map_err(|e| format!("write manifest.yaml: {e}"))?;

    let targets = smoke_targets()?;
    println!(
        "smoke: hiding {} build-machine source path(s) ...",
        targets.len()
    );
    // 守护在函数返回/panic 展开时无条件还原。
    let _hider = SourceHider::new(targets)?;

    let out = Command::new(&bin)
        .current_dir(&probe)
        .args(["build", "-d", "src", "-o", "out"])
        .output()
        .map_err(|e| format!("run {}: {e}", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || !stdout.contains("module(s)") {
        return Err(format!(
            "发布门禁失败：{} 在「源文件缺席」环境无法完成 `oj build` —— 该产物仍依赖构建机路径。\n\
             exit={:?}\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}",
            bin.display(),
            out.status.code()
        ));
    }
    let _ = fs::remove_dir_all(&probe);
    println!(
        "smoke ok: {} 在源文件缺席环境完成 `oj build`",
        bin.display()
    );
    Ok(())
}

/// 门禁要临时隐藏的路径：两个自身 bootstrap + deno_* 依赖源码目录。
fn smoke_targets() -> Result<Vec<PathBuf>, String> {
    let mut targets = vec![
        root().join("src").join("bridge").join("bootstrap.js"),
        root()
            .join("oj")
            .join("src")
            .join("test_ext")
            .join("test_bootstrap.js"),
    ];
    let lock = fs::read_to_string(root().join("Cargo.lock"))
        .map_err(|e| format!("read Cargo.lock: {e}"))?;
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"));
    for krate in SMOKE_CRATES {
        let version = lock_version(&lock, krate)
            .ok_or_else(|| format!("Cargo.lock 中找不到 `{krate}` 的版本"))?;
        let dir = find_crate_dir(&cargo_home, krate, &version)
            .ok_or_else(|| format!("找不到 {krate}-{version} 源码目录（先 cargo fetch）"))?;
        targets.push(dir);
    }
    Ok(targets)
}

/// 临时改名守护：Drop 时还原（含 panic 展开；进程被硬杀除外 —— `new` 会先做残留恢复）。
struct SourceHider {
    /// (原路径, 备份路径)
    pairs: Vec<(PathBuf, PathBuf)>,
}

impl SourceHider {
    fn new(targets: Vec<PathBuf>) -> Result<Self, String> {
        let mut pairs = Vec::new();
        for orig in targets {
            let bak = bak_path(&orig);
            // 上次中断残留：先还原再隐藏，避免把仓库/registry 留在缺失状态。
            if !orig.exists() && bak.exists() {
                fs::rename(&bak, &orig).map_err(|e| format!("recover {}: {e}", orig.display()))?;
                println!("smoke: recovered leftover {}", orig.display());
            }
            if !orig.exists() {
                return Err(format!("smoke: 待隐藏路径不存在：{}", orig.display()));
            }
            fs::rename(&orig, &bak).map_err(|e| format!("hide {}: {e}", orig.display()))?;
            pairs.push((orig, bak));
        }
        Ok(Self { pairs })
    }
}

impl Drop for SourceHider {
    fn drop(&mut self) {
        for (orig, bak) in &self.pairs {
            if bak.exists() {
                let _ = fs::rename(bak, orig);
            }
        }
    }
}

fn bak_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.oj-smoke-bak"))
}

/// 从 Cargo.lock 取版本（与根 crate `build.rs` 同款朴素解析，避免额外依赖）。
fn lock_version(lock: &str, name: &str) -> Option<String> {
    let needle = format!("name = \"{name}\"");
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line.trim() != needle {
            continue;
        }
        for line in lines.by_ref() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("version = \"") {
                return Some(rest.trim_end_matches('"').to_string());
            }
            if line.starts_with("[[package]]") {
                break;
            }
        }
    }
    None
}

/// 依赖源码目录：优先 `vendor/`（cargo vendor），其次 CARGO_HOME registry（目录名带哈希）。
fn find_crate_dir(cargo_home: &Path, krate: &str, version: &str) -> Option<PathBuf> {
    let vendored = root().join("vendor").join(krate);
    if vendored.is_dir() {
        return Some(vendored);
    }
    let registry_src = cargo_home.join("registry").join("src");
    let mut found = None;
    for entry in fs::read_dir(registry_src).ok()?.flatten() {
        let candidate = entry.path().join(format!("{krate}-{version}"));
        if candidate.is_dir() {
            found = Some(candidate);
        }
    }
    found
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn usage() -> ! {
    eprintln!("usage: cargo xtask <bin | plugin <name> [--check] | build | smoke --bin <path>>");
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
        "smoke" => {
            let bin = args
                .iter()
                .position(|a| a == "--bin")
                .and_then(|i| args.get(i + 1))
                .ok_or_else(|| "smoke requires --bin <path>".to_string())?;
            smoke(Path::new(bin))
        }
        _ => {
            usage();
        }
    };
    result.map_err(|e| format!("xtask error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_first_party_plugins_when_listed_then_covers_all_axes() {
        // 业务约定：8 个第一方插件 = es/db×2/blob/bus×2/kv/auth 全轴覆盖。
        assert_eq!(PLUGINS.len(), 8);
        assert!(PLUGINS.contains(&"auth"));
        assert!(PLUGINS.contains(&"bus-kafka"));
    }

    #[test]
    fn given_host_rustc_when_triple_then_non_empty_and_matches_target_os() {
        let t = host_triple();
        assert!(!t.is_empty());
        if cfg!(target_os = "macos") {
            assert!(t.contains("apple"), "{t}");
        } else if cfg!(target_os = "windows") {
            assert!(t.contains("windows"), "{t}");
        }
    }

    #[test]
    fn given_artifact_and_file_names_when_resolved_then_platform_layout() {
        // 与 PluginLoader plugin_file_name 同形：存放名以 descriptor.name 命名，
        // 与 rustc 产物名（oj_<name>，- → _）解耦。
        if cfg!(target_os = "macos") {
            assert_eq!(build_artifact_name("kv-redis"), "liboj_kv_redis.dylib");
            assert_eq!(plugin_file_name("kv-redis"), "libkv-redis.dylib");
            assert_eq!(oj_exe_name(), "oj");
        } else if cfg!(target_os = "windows") {
            assert_eq!(build_artifact_name("kv-redis"), "oj_kv_redis.dll");
            assert_eq!(plugin_file_name("kv-redis"), "kv-redis.dll");
            assert_eq!(oj_exe_name(), "oj.exe");
        } else {
            assert_eq!(build_artifact_name("kv-redis"), "liboj_kv_redis.so");
            assert_eq!(plugin_file_name("kv-redis"), "libkv-redis.so");
            assert_eq!(oj_exe_name(), "oj");
        }
    }

    #[test]
    fn given_nested_tree_when_copy_dir_all_then_dirs_and_files_recursively_copied() {
        let src = std::env::temp_dir().join(format!("oj-xtask-cp-{}", std::process::id()));
        let dst = std::env::temp_dir().join(format!("oj-xtask-dst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::write(src.join("a/one.txt"), "1").unwrap();
        std::fs::write(src.join("a/b/two.md"), "2").unwrap();
        copy_dir_all(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("a/one.txt")).unwrap(), "1");
        assert_eq!(
            std::fs::read_to_string(dst.join("a/b/two.md")).unwrap(),
            "2"
        );
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn given_repo_docs_when_copy_devkit_then_bin_devkit_fresh_with_oidc_and_dts() {
        // 发行契约：devkit = docs/devkit 三件 + 两份 OIDC 手册 + sample/global.d.ts。
        copy_devkit().unwrap();
        let dk = bin_dir().join("devkit");
        assert!(dk.join("global.d.ts").exists());
        assert!(dk.join("oidc-integration.md").exists());
        assert!(dk.join("oidc-implementation.md").exists());
        assert!(dk.join("api-manual.md").exists());
    }

    #[test]
    fn given_hidden_paths_when_hider_drops_then_originals_restored() {
        let dir = std::env::temp_dir().join(format!("oj-xtask-hide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bootstrap.js");
        let sub = dir.join("deno_x-0.1.0");
        std::fs::write(&file, "x").unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        {
            let _hider = SourceHider::new(vec![file.clone(), sub.clone()]).unwrap();
            assert!(!file.exists() && !sub.exists());
            assert!(bak_path(&file).exists() && bak_path(&sub).exists());
        }
        assert!(file.exists() && sub.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn given_leftover_backup_when_hider_new_then_recovered_before_hiding() {
        let dir = std::env::temp_dir().join(format!("oj-xtask-recover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bootstrap.js");
        // 模拟上次中断：只剩 .oj-smoke-bak。
        std::fs::write(bak_path(&file), "x").unwrap();
        {
            let _hider = SourceHider::new(vec![file.clone()]).unwrap();
            assert!(!file.exists());
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
