//! oj build：src → dist（按模块）。转译 .ts（默认 minify，`--no-minify` 关）、剥 `.route`、
//! 补相对 import 后缀；产物保留原名与目录结构（api.ts → 同目录 api.js），
//! 产出 `dist/<module>-<version>/`（routes.js + manifest.yaml）
//! 与 `dist/manifests.yaml`、`<module>-<version>.tgz`（spec §2）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::import_scan::{
    is_alias, is_local, is_relative, rewrite_specifiers, specifier_spans,
};
use only_js::bridge::{
    Bridge, Extras, InMemoryKV, LoaderShared, ModuleIndex, SchemaRegistry, transpile,
};
use serve::routes;

use crate::args::BuildArgs;
use crate::hex_encode;

/// 构建入口：单模块或全部（None）。内省器自带独立线程 runtime；async 仅因内存库初始化。
pub async fn run(a: &BuildArgs) -> Result<(), String> {
    // canonicalize 解析 8.3 短名（Windows `RUNNER~1` ↔ 长名）并归一；strip_verbatim 去
    // `\\?\` 前缀，使后续所有本地路径（含 `resolve_to_segs` 的 `strip_prefix`、loader 的
    // `module_root_of`）与 referrer 目录词法一致，避免 Windows 上别名解析误判「未找到模块根」。
    // `--module-root` 与 `-d` 互斥（clap）：两者都是「源码树根」，取其一。
    let src_arg = a.module_root.as_deref().unwrap_or(a.dir.as_str());
    let src =
        only_js::bridge::strip_verbatim(&PathBuf::from(src_arg).canonicalize().map_err(|e| {
            format!("src dir '{src_arg}': {e}\n  下一步：--module-root 须指向存在的后端源码树根")
        })?);
    let out = PathBuf::from(&a.out);
    // 跨模块导入的版本视图：单模块 = 锁；全量 = 锁 ∪ src 各模块 manifest（src 在建，覆盖锁）。
    let tasks_dir = tasks_dir_of(a.config.as_deref());
    // 模块索引（仅 `--module-root` 时构造）：模块根可不在首层，发现 / 别名 / 跨模块
    // 归属 / 路由段都从它取——「哪个目录是模块」不允许有两处判定。
    let index = match a.module_root.as_deref() {
        Some(_) => {
            // 产物树（release dist）没有「模块根」概念：它的 `manifests.yaml` 是版本锁，
            // 子目录是 `<name>-<version>/` 产物而非模块。指到这里会静默产出一堆空壳
            // 版本目录（实测：0 api file 却「构建成功」），故 fail-fast。
            if src.join("manifests.yaml").is_file() {
                return Err(format!(
                    "--module-root is a source-tree option: {} is a release tree (contains \
                     manifests.yaml) — point it at the src tree, or drop --module-root",
                    src.display()
                ));
            }
            Some(only_js::bridge::resolve_roots(
                &src,
                true,
                Some(&tasks_dir),
            )?)
        }
        None => None,
    };
    let mut view = crate::manifest::load_lock(&out.join("manifests.yaml"))?;
    let mut names: Vec<String> = match &a.module {
        Some(m) => {
            crate::manifest::validate_module(m)?;
            // 索引下按模块名定位（`[MODULE]` 位置参数是身份，不是目录名）。
            let mf = match index.as_ref().and_then(|i| i.get(m)) {
                Some(e) => e.root.join("manifest.yaml"),
                None => src.join(m).join("manifest.yaml"),
            };
            if !mf.is_file() {
                let known = index
                    .as_ref()
                    .map(|i| i.names().map(String::as_str).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                let hint = if known.is_empty() {
                    String::new()
                } else {
                    format!("\n  下一步：可选模块名（manifest.yaml 的 name）：{known}")
                };
                return Err(format!(
                    "module {m:?}: no manifest.yaml under {}{hint}",
                    src.display()
                ));
            }
            view.insert(m.clone(), crate::manifest::parse_one(&mf)?.version);
            vec![m.clone()]
        }
        None => crate::manifest::load_modules(&src, Some(&tasks_dir), index.as_ref())?
            .into_iter()
            .map(|m| {
                view.insert(m.name.clone(), m.version);
                m.name
            })
            .collect(),
    };
    names.sort(); // read_dir 顺序不定；构建顺序确定 → 控制台/lock 写入顺序稳定
    // view 全量（锁∪计划）{m}-{v} 不单射（a v1-x 与 a-1 vx 同落一个版本目录，后者清场前者；锁内陈旧条目同理）→ fail-fast
    let mut vdirs = std::collections::HashSet::new();
    for (m, v) in &view {
        let vd = format!("{m}-{v}");
        if !vdirs.insert(vd.clone()) {
            return Err(format!("version dir collision: {vd}"));
        }
    }
    // 检查体系（§5.2）：构建即检查，S001–S008 违规 fail build；--check 只校验不落盘。
    // sql_guard 活跃时追加 tenant 声明校验（schema.yaml 缺 tenant_id 列 fail build）。
    crate::checks::run(
        &src,
        &names,
        &view,
        sql_guard_of_config(a.config.as_deref()),
        index.as_ref(),
    )?;
    if a.check {
        println!("oj build --check: {} module(s) OK", names.len());
        return Ok(());
    }
    for name in &names {
        build_one(&src, &out, name, &view, a.minify, index.as_ref()).await?;
    }
    // tasks 目录转译镜像（T10，评审 F2）：非版本化资产，不进锁/tgz。
    mirror_tasks(&src, &out, &tasks_dir, a.minify)?;
    // 产物自洽断言：只扫**本次构建产出**的目录，本地 specifier 必须都能落到已落盘文件
    // （跨模块目标此时也已由本次构建产出，或来自锁指向的既有版本目录）。
    let mut roots: Vec<PathBuf> = names
        .iter()
        .filter_map(|m| view.get(m).map(|v| out.join(format!("{m}-{v}"))))
        .collect();
    let tdir = out.join(&tasks_dir);
    if tdir.is_dir() {
        roots.push(tdir);
    }
    assert_dist_consistent(&roots)?;
    println!("oj build: {} module(s) → {}", names.len(), out.display());
    // --bundle：把整棵 dist（manifests.yaml + 各 module-version/）打成「dist 根 tarball」
    // （空前缀，根 = manifests.yaml + 子目录），供 mounts[].api = URL 远程挂载。
    if a.bundle {
        bundle_dist(&out)?;
    }
    Ok(())
}

/// 打包 dist 根为内容寻址 tarball：整目录以空前缀打成 `<out>/<name>.tgz`（根 =
/// manifests.yaml + module-version/），并打印 sha256 摘要，供远程 URL 挂载声明完整性锚。
/// 跳过既有 `*.tgz`（含上轮 bundle 产物）以防自包含递归。
fn bundle_dist(out: &Path) -> Result<String, String> {
    if !out.join("manifests.yaml").is_file() {
        return Err(
            "oj build --bundle: out dir must contain manifests.yaml (run a normal build first, \
             or point -o at an existing dist)"
                .into(),
        );
    }
    let name = out
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("app")
        .to_string();
    let tmp = std::env::temp_dir().join(format!(".oj-bundle-{name}-{}.tgz", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    {
        let file = std::fs::File::create(&tmp)
            .map_err(|e| format!("bundle: create {tmp}: {e}", tmp = tmp.display()))?;
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(enc);
        append_tree(&mut builder, out, out)?;
        let enc = builder
            .into_inner()
            .map_err(|e| format!("bundle: into_inner: {e}"))?;
        enc.finish()
            .map_err(|e| format!("bundle: gzip finish: {e}"))?;
    }
    let sha = {
        // 流式读回哈希（整读进内存只为算摘要，产物大时白趟 RSS 尖峰）。
        let mut f = std::fs::File::open(&tmp)
            .map_err(|e| format!("bundle: read back {tmp}: {e}", tmp = tmp.display()))?;
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        std::io::copy(&mut f, &mut h)
            .map_err(|e| format!("bundle: read back {tmp}: {e}", tmp = tmp.display()))?;
        hex_encode(&h.finalize())
    };
    let final_tgz = out.join(format!("{name}.tgz"));
    std::fs::copy(&tmp, &final_tgz).map_err(|e| {
        format!(
            "bundle: write {final_tgz}: {e}",
            final_tgz = final_tgz.display()
        )
    })?;
    let _ = std::fs::remove_file(&tmp);
    println!(
        "oj build --bundle: wrote {} (sha256 {sha})",
        final_tgz.display()
    );
    Ok(sha)
}

/// 递归把 `dir` 的内容（相对 `root`）写入 tar；跳过 `*.tgz` 防自包含递归。
fn append_tree<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    dir: &Path,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| format!("bundle: read_dir {dir}: {e}", dir = dir.display()))?
    {
        let entry = entry.map_err(|e| format!("bundle: read_dir entry: {e}"))?;
        let path = entry.path();
        let name = path
            .strip_prefix(root)
            .map_err(|e| format!("bundle: strip prefix: {e}"))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("bundle: file_type {path}: {e}", path = path.display()))?;
        if ft.is_dir() {
            builder
                .append_dir(name, &path)
                .map_err(|e| format!("bundle: append_dir {name}: {e}", name = name.display()))?;
            append_tree(builder, root, &path)?;
        } else if ft.is_file() {
            if path.extension().and_then(|e| e.to_str()) == Some("tgz") {
                continue; // 跳过既有 bundle / 每模块 tgz，避免自包含与冗余
            }
            let mut f = std::fs::File::open(&path)
                .map_err(|e| format!("bundle: open {path}: {e}", path = path.display()))?;
            builder
                .append_file(name, &mut f)
                .map_err(|e| format!("bundle: append_file {name}: {e}", name = name.display()))?;
        }
        // 符号链接 / 特殊文件：打包态不存在（构建产物皆为常规文件/目录），跳过。
    }
    Ok(())
}

/// 读 config（build 专用）：解析失败**不致命**（build 不装配服务，缺文件也照建），但
/// 调用方必须**出声**——静默回落会悄悄改掉行为（见下面两处消费者）。
/// `config` None → 统一搜索（CWD 逐级向上 + $HOME/.oj），未找到静默用默认 Config
/// （build 本就要求无 config 也能跑）。
fn load_config_for_build(config: Option<&str>) -> Result<only_js::config::Config, String> {
    match only_js::config::find_path(config)? {
        Some(p) => {
            let dir = p
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            only_js::config::load_from(dir, p.file_name().and_then(|s| s.to_str()))
        }
        None => Ok(only_js::config::Config::default()),
    }
}

/// 构建期 sql_guard 模式（与 server 装配同一配置源）。
///
/// **v0.1.23 起配置解析失败会打 warn**（此前静默回落 `Off`）：`sql_guard` 非 Off 的项目
/// 靠它给 `S*` 检查加「schema 声明必须有 tenant_id 列」等校验，静默 Off 等于 **CI 门禁失效**。
/// 注意 `oj build`（本函数）与 `oj server`/`oj test`（`App::from_config`，fail-fast）口径不同，
/// 这是有意的：build 连 `config.yaml` 不存在都要能跑（见上方函数注释）。
fn sql_guard_of_config(config: Option<&str>) -> only_js::bridge::SqlGuard {
    match load_config_for_build(config) {
        Ok(c) => c.tenant.sql_guard,
        Err(e) => {
            eprintln!(
                "warn: 读配置 {} 失败 → 本次 build 按 sql_guard=off 处理（tenant 声明校验不生效）：{e}",
                config.unwrap_or("<auto-search>")
            );
            only_js::bridge::SqlGuard::Off
        }
    }
}

fn tasks_dir_of(config: Option<&str>) -> String {
    match load_config_for_build(config) {
        Ok(c) => c.tasks.dir,
        Err(e) => {
            eprintln!(
                "warn: 读配置 {} 失败 → 镜像目录回落默认 \"tasks\"：{e}",
                config.unwrap_or("<auto-search>")
            );
            "tasks".to_string()
        }
    }
}

/// 递归收集 dir 下全部 .ts/.js（相对 dir 的路径）。
fn walk_ts_js(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in rd {
        let p = entry.map_err(|e| format!("readdir: {e}"))?.path();
        if p.is_dir() {
            walk_ts_js(&p, out)?;
        } else if matches!(
            p.extension().and_then(|s| s.to_str()),
            Some("ts") | Some("js")
        ) {
            out.push(p);
        }
    }
    Ok(())
}

// specifier 扫描/改写（`specifier_spans` / `rewrite_specifiers` / `is_local`|`is_alias`
// |`is_relative`）在 core `only_js::bridge::import_scan`：**唯一实现**，与 `oj/src/checks.rs`
// 的 S008 共用（避免「检查放行、构建改不动」的分叉），同 `bridge::guard::extract_tables` 先例。

/// 残留别名断言（单文件）：别名必须全部实化（产物内不得再有 `#` specifier）。
/// 把「扫描器漏检」从**运行期静默炸**降级为**构建期显式失败**——字符级扫描天花板的兜底。
/// 覆盖面更广的产物自洽断言见 `assert_dist_consistent`。
fn assert_no_aliases(js: &str, what: &str) -> Result<(), String> {
    for (_, _, spec) in specifier_spans(js) {
        if is_alias(&spec) {
            return Err(format!(
                "{what}: 别名未实化（改写器漏检）：{spec:?}\n  \
                 下一步：改用相对路径绕过，并报此缺陷（扫描器未覆盖该写法）"
            ));
        }
    }
    Ok(())
}

/// 产物自洽断言：每一个**本地** specifier 都必须能落到已落盘的文件。
/// 单文件的别名断言只护别名，漏改写的**相对** specifier（扫描器未覆盖的写法，如正则
/// 字面量导致错位）此前会静默进产物、release 运行期才炸；这里把整类问题收敛为构建期失败。
///
/// `roots` = **本次构建产出的目录**（各模块版本目录 + tasks 镜像），不扫整个 `dist`：
/// 陈旧的他人产物（旧版 oj 构建、或已不存在的模块）不该让本次构建失败。
fn assert_dist_consistent(roots: &[PathBuf]) -> Result<(), String> {
    let mut stack: Vec<PathBuf> = roots.to_vec();
    let mut bad: Vec<String> = Vec::new();
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !p.extension().is_some_and(|x| x == "js") {
                continue;
            }
            let js =
                std::fs::read_to_string(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
            let dir = p.parent().unwrap_or(Path::new(""));
            for (_, _, spec) in specifier_spans(&js) {
                if !is_local(&spec) {
                    continue;
                }
                if is_alias(&spec) {
                    bad.push(format!("  {}: 别名未实化 {spec:?}", p.display()));
                    continue;
                }
                let target = dir.join(&spec);
                if !target.is_file() && !target.join("index.js").is_file() {
                    bad.push(format!(
                        "  {}: {spec:?} → {} 不存在",
                        p.display(),
                        target.display()
                    ));
                }
            }
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "构建产物自洽检查失败（import 目标未落盘；release 会在运行期才炸）：\n{}\n  \
             下一步：把这些 specifier 改成 `.ts` 目标的本地导入（只有 .ts 会被转译落盘），\
             或确认构建器覆盖了该写法（报此缺陷）",
            bad.join("\n")
        ))
    }
}
/// tasks 池镜像（spec §6/T10）：`<src>/<tasks.dir>` → `<out>/<tasks.dir>`，递归。
/// .ts → 转译 .js（相对 import `./x.ts` → `./x.js`）；.js → 原样转译直通；
/// 其余扩展名跳过。目录不存在 = 跳过（空池）。
fn mirror_tasks(src: &Path, out: &Path, tasks_dir: &str, minify: bool) -> Result<(), String> {
    let from = src.join(tasks_dir);
    if !from.is_dir() {
        return Ok(());
    }
    let mut files = Vec::new();
    walk_ts_js(&from, &mut files)?;
    let to = out.join(tasks_dir);
    for f in &files {
        let rel = f.strip_prefix(&from).unwrap_or(f);
        let dst_dir = to.join(rel.parent().unwrap_or(Path::new("")));
        std::fs::create_dir_all(&dst_dir)
            .map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
        let js = transpile::cached_transpile(f)
            .map_err(|e| format!("transpile {}: {e}", f.display()))?;
        // 相对 import 落 .js 后缀（任务池内互导）。两条硬边界：
        // ① 别名一律拒绝——任务池是非版本化资产（不进锁 / 不打 tgz，只镜像到 dist/tasks），
        //    无力绑定模块版本；
        // ② 相对 import 不得越过任务池根——池外目标（尤其跨模块）在产物里必然不存在，
        //    此前会静默悬空到运行期。
        let depth = rel.parent().map(|p| p.components().count()).unwrap_or(0);
        let pool = to.display().to_string();
        let js = rewrite_specifiers(&js, |spec| {
            if is_alias(spec) {
                return Err(format!(
                    "tasks: 别名 {spec:?} 不受支持（任务池非版本化，无法绑定模块版本）\n  \
                     下一步：改用相对路径，或把共享代码放进模块内"
                ));
            }
            if !is_relative(spec) {
                return Ok(None);
            }
            if spec.split('/').take_while(|s| *s == "..").count() > depth {
                return Err(format!(
                    "tasks: import {spec:?} 越过任务池根（产物只镜像 {pool}，非版本化资产）\n  \
                     下一步：把目标文件放进任务池内（如 {tasks_dir}/_shared/），\
                     或把逻辑内联进任务文件"
                ));
            }
            if let Some(stem) = spec.strip_suffix(".ts") {
                Ok(Some(format!("{stem}.js")))
            } else if !spec.ends_with(".js") && !spec.ends_with(".mjs") && !spec.ends_with(".json")
            {
                Ok(Some(format!("{spec}.js")))
            } else {
                Ok(None)
            }
        })?;
        let js = if minify {
            transpile::minify_js(f, &js).map_err(|e| format!("minify {}: {e}", f.display()))?
        } else {
            js
        };
        let dst = dst_dir.join(rel.with_extension("js").file_name().unwrap());
        std::fs::write(&dst, js).map_err(|e| format!("write {}: {e}", dst.display()))?;
    }
    if !files.is_empty() {
        println!(
            "oj build: tasks mirror ({} file(s)) → {}",
            files.len(),
            to.display()
        );
    }
    Ok(())
}

/// JSON 字符串字面量（转义交给 serde_json，pattern 里可安全含引号）。
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// rel 路径的目录段（正斜杠；模块根下为 ""）。
fn rel_dir(rel: &Path) -> String {
    rel.parent()
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .replace('\\', "/")
}

/// 单模块构建：清场同名版本目录 → 落盘 → 内省产 routes.js → lock upsert → tgz。
/// view = 跨模块导入目标的版本表（run 构建）；minify = 转译产物压缩开关。
/// `index`（`--module-root`）：模块根与 URL 前缀的来源（模块根可不在首层）。
async fn build_one(
    src: &Path,
    out: &Path,
    module: &str,
    view: &std::collections::BTreeMap<String, String>,
    minify: bool,
    index: Option<&ModuleIndex>,
) -> Result<(), String> {
    crate::manifest::validate_module(module)?; // 两路径共用的白名单（全量路径同样过）
    // 模块根：索引下由 name 定位（目录名不参与身份）；否则今日 `src/<name>/`。
    let mdir = match index.and_then(|i| i.get(module)) {
        Some(e) => e.root.clone(),
        None => src.join(module),
    };
    // URL 前缀：manifest 的 `prefix`（缺省 = 模块名）。release 的 rel_pattern 以它为
    // 模块段，dev 侧由 `routes::dev_rel` 取同一份——两侧 URL 同源。
    let prefix = index
        .and_then(|i| i.get(module))
        .map(|e| e.prefix.clone())
        .unwrap_or_else(|| module.to_string());
    let m = crate::manifest::parse_one(&mdir.join("manifest.yaml"))?;
    if m.name != module {
        return Err(format!("manifest name {:?} != module {:?}", m.name, module));
    }
    crate::manifest::validate_version(&m.version)?;
    let vdir = out.join(format!("{module}-{}", m.version));
    // 清场：同版本重建先删（旧产物残留根治，spec §2.3）
    if vdir.exists() {
        std::fs::remove_dir_all(&vdir).map_err(|e| format!("clean {}: {e}", vdir.display()))?;
    }
    std::fs::create_dir_all(&vdir).map_err(|e| format!("mkdir {}: {e}", vdir.display()))?;

    // 1. 收集 + api.ts 不可被 import 守卫
    let files = collect_module(&mdir)?;
    let sources: Vec<(String, String)> = files
        .iter()
        .filter(|(rel, _)| rel.extension().is_some_and(|e| e == "ts"))
        .map(|(rel, _)| {
            let text = std::fs::read_to_string(mdir.join(rel))
                .map_err(|e| format!("read {}: {e}", rel.display()))?;
            Ok((rel.to_string_lossy().into_owned(), text))
        })
        .collect::<Result<_, String>>()?;
    guard_no_api_imports(&sources)?;

    // 2. 落盘：全部 .ts 原路径换 .js 扩展（api.ts 同名 api.js，仅多一步剥 .route）；
    //    补相对 import 后缀后按需 minify；manifest.yaml 原样复制。
    for (rel, is_api) in &files {
        let dir = rel
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(""));
        let dst_dir = vdir.join(dir);
        std::fs::create_dir_all(&dst_dir)
            .map_err(|e| format!("mkdir {}: {e}", dst_dir.display()))?;
        if rel.extension().is_some_and(|e| e == "yaml" || e == "sql") {
            let dst = dst_dir.join(rel.file_name().unwrap());
            std::fs::copy(mdir.join(rel), &dst)
                .map_err(|e| format!("copy {}: {e}", dst.display()))?;
        } else {
            let js = transpile::cached_transpile(&mdir.join(rel))
                .map_err(|e| format!("transpile {}: {e}", rel.display()))?;
            let stripped; // 生命周期：strip 产物要活过 fix_import_specifiers 调用
            let js = fix_import_specifiers(
                src,
                if *is_api {
                    stripped = strip_route_decls(&js);
                    &stripped
                } else {
                    &js
                },
                module,
                &m.version,
                &rel_dir(rel),
                view,
                index,
                // 报错带违规文件路径（三要素：文件 + 原因 + 下一步；原因带候选清单）
            )
            .map_err(|e| format!("{}: {e}", rel.display()))?;
            let js = if minify {
                transpile::minify_js(&mdir.join(rel), &js)
                    .map_err(|e| format!("minify {}: {e}", rel.display()))?
            } else {
                js
            };
            // 残留别名断言：对**最终产物**（minify 之后）校验，别名必须已全部实化。
            assert_no_aliases(&js, &rel.display().to_string())?;
            let name = rel
                .with_extension("js")
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let dst = dst_dir.join(&name);
            std::fs::write(&dst, js).map_err(|e| format!("write {}: {e}", dst.display()))?;
        }
    }

    // 3. 内省（内存库）→ 校验/告警 → routes.js：pattern 无 base 含模块段（rel_pattern），
    //    file = 同名产物相对版本目录根（正斜杠；根级 api.ts 为裸 api.js）
    let decls = introspect_module_files(src, &mdir, &files, index).await?;
    let n_api = decls.len();
    // v0.1.27 告警：`_name_` 形态模块名 → 路由参数段，同位异名双模块会在部署启动期结构冲突。
    if routes::looks_like_underscore_param(module) {
        eprintln!(
            "warn: module name {module:?} looks like `_name_` fs-param syntax; it becomes a route param segment (two such modules with different names at the same slot will conflict at deployment startup)"
        );
    }
    // (file, method, handler 链, pattern, 契约) —— 契约与链一并落进 routes.js，
    // 使 release 免内省也能校验入参、派发嵌套 handler。
    let mut rows_out: Vec<(String, String, String, String, Option<serde_json::Value>)> = Vec::new();
    let mut pats: Vec<String> = Vec::new();
    for (dir, rows) in &decls {
        let file = if dir.is_empty() {
            "api.js".to_string()
        } else {
            format!("{dir}/api.js")
        };
        for (chain, route, schema) in rows {
            // 动词 = 链尾（内省保证只有 verb 键成 leaf；此处同口径推导）。
            let method = chain.rsplit('.').next().unwrap_or(chain);
            if let Some(r) = route.as_deref().map(str::trim).filter(|r| !r.is_empty())
                && r.split('/').any(routes::looks_like_underscore_param)
            {
                eprintln!(
                    "warn: {file}: .route value {r:?} looks like `_name_` fs-param syntax; in .route it stays a literal segment (use {{name}} for params)"
                );
            }
            let pat = rel_pattern(&prefix, dir, route.as_deref());
            pats.push(pat.clone());
            rows_out.push((
                file.clone(),
                method.to_string(),
                chain.clone(),
                pat,
                schema.clone(),
            ));
        }
    }
    // `.schema` 声明合法性审计（v0.1.70）：与 serve 装配**同口径**（同一个
    // `contract::audit_schema`）。落盘前拦住，否则非法声明会先写进 routes.js，
    // 直到部署启动才爆——而 openapi.json 现在是下游生成 __client 的唯一输入。
    for (file, method, chain, pat, schema) in &rows_out {
        if let Some(sv) = schema {
            only_js::contract::audit_schema(sv).map_err(|e| {
                format!("invalid `.schema` on {method} {pat} (handler `{chain}`, {file}): {e}")
            })?;
        }
    }
    // 构建期 fail-fast：非法 pattern / 同位异名参数在 build 期报错，免得到部署启动才爆。
    routes::check_patterns(&pats)?;
    let mut js = String::from("// 由 oj build 生成；勿手改。\nexport default [\n");
    for (file, method, chain, pat, schema) in &rows_out {
        // schema 仅在声明时写入（旧产物无该字段 = 不校验，不得拒启）；handler 链仅在
        // 与 method 不同（嵌套组）时写入——顶层动词行保持旧产物字节形态。
        let mut row = format!(
            "  {{ method: {}, pattern: {}, file: {}",
            q(method),
            q(pat),
            q(file)
        );
        if chain != method {
            row.push_str(&format!(", handler: {}", q(chain)));
        }
        if let Some(sv) = schema {
            row.push_str(&format!(
                ", schema: {}",
                serde_json::to_string(sv).map_err(|e| format!("serialize .schema: {e}"))?
            ));
        }
        row.push_str(" },\n");
        js.push_str(&row);
    }
    js.push_str("];\n");
    std::fs::write(vdir.join("routes.js"), js).map_err(|e| format!("write routes.js: {e}"))?;

    // 4. manifests.yaml：读旧（缺失=空表；坏锁 Err 不静默重置）→ upsert → 原子写
    let lock_path = out.join("manifests.yaml");
    let mut lock = crate::manifest::load_lock(&lock_path)?;
    lock.insert(module.to_string(), m.version.clone());
    crate::manifest::save_lock(&lock_path, &lock)?;

    // 5. tgz
    crate::pack::write_tgz(
        &vdir,
        &out.join(format!("{module}-{}.tgz", m.version)),
        &format!("{module}-{}", m.version),
    )?;
    println!(
        "oj build: {module} v{} → {} ({} api file(s))",
        m.version,
        vdir.display(),
        n_api
    );
    Ok(())
}

/// 模块内省：每个 api.ts 一线程 + current_thread runtime（Bridge !Send），内存库
/// 零磁盘副作用（同 dev 内省管道）。project_root 取 src 父目录（= 项目根，同 dev 的
/// config_dir）：bare import 要沿 node_modules 向上解析到项目根（src 下没有）。
/// 返回 (rel 目录, 该 api.ts 的 (handler 链, 有效 route, schema) 行)，顺序随
/// collect_module 确定。v0.1.64：链为 `introspect_module` 扁平 leaf 的 chain。
async fn introspect_module_files(
    src: &Path,
    mdir: &Path,
    files: &[(PathBuf, bool)],
    index: Option<&ModuleIndex>,
) -> Result<
    Vec<(
        String,
        Vec<(String, Option<String>, Option<serde_json::Value>)>,
    )>,
    String,
> {
    // `src` 在 `run` 入口已 canonicalize + strip_verbatim（Windows 去 `\\?\` 前缀，长名），
    // 故 project_root 与 loader 经 `versioned_specifier` 给出的 referrer 目录词法可比，
    // `module_root_of` 不会再误判「未找到模块根」（见 alias_build_materializes_to_versioned_relative_paths）。
    let root = src.parent().unwrap_or(src).to_path_buf();
    // ext_boot：与 dev 同源探测（src 父目录 = 项目根），保证 dev/build/release 三处一致。
    let boot = crate::app::ext_boot_spec(&root)?;
    let mut dbs: HashMap<String, Arc<dyn only_js::bridge::DataAccessor>> = HashMap::new();
    dbs.insert(
        "default".into(),
        only_js::bridge::DbBackendRegistry::builtin()
            .connect("sqlite::memory:", &root)
            .await
            .map_err(|e| format!("open build db: {e}"))?,
    );
    // 索引要先克隆成 owned Arc：内省闭包是 'static 的，抓不住借用。
    let index_arc: Option<Arc<ModuleIndex>> = index.map(|i| Arc::new(i.clone()));
    let make = {
        let dbs = dbs.clone();
        let index_arc = index_arc.clone();
        move || {
            Bridge::with_dbs_and_loader(
                dbs.clone(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: true,
                })),
                Extras {
                    boot: boot.clone(),
                    // 与 dev 同源的模块索引：内省期解析 `#/m/x` 要靠它（模块根可不在
                    // 首层，`module_root.parent()` 在嵌套下算不出 src 根）。
                    module_index: index_arc.clone(),
                    ..Default::default()
                },
            )
        }
    };
    let introspect = routes::bridge_introspector(make);
    let mut out = Vec::new();
    for (rel, is_api) in files {
        if !is_api {
            continue;
        }
        let rows = introspect(&mdir.join(rel))
            .map_err(|e| format!("introspect {}: {e}", rel.display()))?
            .into_iter()
            .collect::<Vec<_>>();
        out.push((rel_dir(rel), rows));
    }
    Ok(out)
}

/// 递归收集模块内 .ts 与 manifest.yaml（相对模块根，确定性排序）。
/// is_api = 文件名是 api.ts（剥 .route + 进 routes.js）。
fn collect_module(root: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let mut acc = Vec::new();
    walk(root, root, &mut acc)?;
    Ok(acc)
}

fn walk(root: &Path, dir: &Path, acc: &mut Vec<(PathBuf, bool)>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            // 演示数据不进产物（spec §4.5/P0），由 oj test fixture 灌入；
            // node_modules 与运行期解析口径一致地排除（运行期 resolve_inner 不对
            // node_modules 内的文件启用别名/本地语义），否则会把第三方源码打进产物。
            if name == "fixtures" || name == "node_modules" {
                continue;
            }
            walk(root, &p, acc)?;
        } else {
            let is_api = name == "api.ts";
            // P0 白名单扩展：模块自带 SQL（seed/schema/migrations）与 schema.yaml 进 dist。
            if is_api
                || name.ends_with(".ts")
                || name == "manifest.yaml"
                || name == "schema.yaml"
                || name.ends_with(".sql")
            {
                acc.push((p.strip_prefix(root).unwrap().to_path_buf(), is_api));
            }
        }
    }
    Ok(())
}

/// 剥离转译产物中的 `.route` 赋值整行（`fn.route = "...";`）。
/// ponytail: 行级匹配语句起始的标准写法；表达式中间的 `.route` 读取不受影响。
fn strip_route_decls(src: &str) -> String {
    let kept: Vec<&str> = src
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.contains('=')
                && t.split(|c: char| c.is_whitespace() || c == '=')
                    .next()
                    .unwrap_or("")
                    .ends_with(".route"))
        })
        .collect();
    let mut out = kept.join("\n");
    out.push('\n');
    out
}

/// 本地 specifier（相对 / 别名）改写为 dist 产物路径（spec §2.4）：
/// 归一解析后仍在 `src/<m>/` 内 → 模块内重算相对路径（版本目录布局下原 specifier
/// 上溯会落到无版本段的 `dist/<m>/…` 悬空）；越界 → 跨模块，查版本视图得 v_t，
/// 指向 `dist/<m_t>-<v_t>/`，视图缺 m_t fail-fast。
/// npm 裸包名不改写（交运行期 resolve_bare）。**全部**本地 specifier 都过探针校验
/// （含 `.js`/`.json` 等非 `.ts` 目标 → 直接报错，见 `resolve_to_segs`）——
/// 「dev 能跑、release 悬空」不再有静默通道。
fn fix_import_specifiers(
    src_root: &Path,
    src: &str,
    module: &str,
    version: &str,
    rel_dir: &str,
    view: &std::collections::BTreeMap<String, String>,
    index: Option<&ModuleIndex>,
) -> Result<String, String> {
    rewrite_specifiers(src, |spec| {
        if !is_local(spec) {
            return Ok(None);
        }
        let segs = resolve_to_segs(src_root, module, rel_dir, spec, index)?;
        let target_dir = if segs[0] == module {
            format!("{module}-{version}")
        } else {
            let m_t = &segs[0];
            let v_t = view.get(m_t).ok_or_else(|| {
                format!(
                    "cross-module import {spec:?} → module {m_t:?} version unknown \
                     (not in dist/manifests.yaml) — run `oj build {m_t}` first"
                )
            })?;
            format!("{m_t}-{v_t}")
        };
        let to = std::iter::once(target_dir)
            .chain(segs[1..].iter().cloned())
            .collect();
        Ok(Some(product_spec(module, version, rel_dir, to)))
    })
}

/// 本地 specifier → src_root 下的段列表（**末段为真实文件名**）。
/// 相对导入与别名**共用运行期的同一份探针**（`resolve_relative` / `resolve_alias`），
/// 于是 dev 与 release 对同一 specifier 必然命中同一文件——「dev 能跑、release 悬空」
/// 的一类缺陷在结构上被消掉（此前构建期只做字符串补后缀，命中不了目录索引）。
fn resolve_to_segs(
    src_root: &Path,
    module: &str,
    rel_dir: &str,
    spec: &str,
    index: Option<&ModuleIndex>,
) -> Result<Vec<String>, String> {
    // 索引下模块根由 name 定位（`src/<name>/` 在嵌套布局下不是模块根）；否则今日语义。
    let mroot = match index.and_then(|i| i.get(module)) {
        Some(e) => e.root.clone(),
        None => src_root.join(module),
    };
    let from_dir = if rel_dir.is_empty() {
        mroot
    } else {
        mroot.join(rel_dir)
    };
    let file = if is_alias(spec) {
        only_js::bridge::resolve_alias(spec, &from_dir, src_root, true, index)?
    } else {
        only_js::bridge::resolve_relative(&from_dir, spec, true)?
    };
    // 段列表（首段 = 目标模块名）：索引下 = [模块名, 模块根内相对路径…]——目录名
    // （`foo/api`）不进产物路径，落盘又以模块根为基，故两侧必须同一口径，否则悬空。
    let segs: Vec<String> = match index.and_then(|i| i.locate(&file)) {
        Some((name, _, rel)) => std::iter::once(name.to_string())
            .chain(
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned()),
            )
            .collect(),
        // 模块外（tasks 池、src 根散文件）→ 回落今日语义与原文案。
        None => {
            // `resolve_alias` 给出的 file 是 canonical 形态（module_root_of 双侧
            // canonicalize），src_root 却可能是原样 CLI 路径（macOS /tmp → /private/tmp
            // 即词法不一致）——先按 canonical 剥，不行再按原样剥。
            // Windows 专项：canonicalize 带 `\\?\` 前缀，不剥就永远配不上一旁非
            // verbatim 的 file（url crate 的 to_file_path 侧已剥）——与 module_root_of
            // 同口径先 strip_verbatim 再比。
            let src_canon = only_js::bridge::strip_verbatim(
                &std::fs::canonicalize(src_root).unwrap_or_else(|_| src_root.to_path_buf()),
            );
            let rel = file
                .strip_prefix(&src_canon)
                .or_else(|_| file.strip_prefix(src_root))
                .map_err(|_| format!("import {spec:?} escapes src/ (from {module}/{rel_dir})"))?;
            rel.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        }
    };
    if segs.is_empty() {
        return Err(format!("import {spec:?} resolves to src/ root"));
    }
    // 只有 .ts 会被转译落盘（collect_module 白名单）：目标若是 .js/.json 等，dev 能跑
    // 但产物里没有该文件——此前静默产出悬空 specifier，release 才炸。这里 fail-fast。
    if !file.extension().is_some_and(|e| e == "ts") {
        return Err(format!(
            "import {spec:?} → {} 的扩展名不会进产物（只有 .ts 被转译落盘）\n  \
             下一步：把该文件改成 .ts，或把内容内联/搬进 .ts 模块",
            file.display()
        ));
    }
    Ok(segs)
}

/// 产物相对 specifier：从 `dist/<module>-<version>/<rel_dir>/`（当前产物文件位置）到
/// `to`（首段为目标版本目录）的相对路径；`.ts` 改 `.js`（其余后缀原样——探针给的是
/// 真实文件名，不再猜后缀）；无上溯时必须带 `./` 前缀（ESM 裸 specifier 会被当包名解析）。
fn product_spec(module: &str, version: &str, rel_dir: &str, mut to: Vec<String>) -> String {
    let from: Vec<String> = std::iter::once(format!("{module}-{version}"))
        .chain(
            rel_dir
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        )
        .collect();
    let mut i = 0;
    while i < from.len() && i < to.len() && from[i] == to[i] {
        i += 1;
    }
    let last = to.len() - 1;
    to[last] = match to[last].strip_suffix(".ts") {
        Some(stem) => format!("{stem}.js"),
        None => to[last].clone(),
    };
    let mut parts: Vec<String> = vec!["..".into(); from.len() - i];
    parts.extend(to[i..].iter().cloned());
    let joined = parts.join("/");
    if i == from.len() {
        format!("./{joined}")
    } else {
        joined
    }
}

/// 相对 pattern（spec §2.1）：无首斜杠无 base，含模块段。
/// None/空 route → 目录镜像（v0.1.27：模块段与 rel_dir 逐段过 `fs_seg_to_pattern`，
/// `_name_` → `{name}`）；相对声明 → 目录 + route；根级声明（/ 开头）→ 剥首斜杠
/// 不加模块段。route 值**不转换**（matchit 语法，`{name}` 才是参数）。
///
/// 模块段取 `prefix`（v0.1.85）：`manifest.yaml` 声明了 `prefix` 就用它，否则等于
/// 模块名（`resolve_prefix` 归一）。dev 侧 `routes::dev_rel` 取同一份值——dev 与
/// release 的 URL 由此同源；`prefix` 可含 `/`（逐段转换）。
fn rel_pattern(prefix: &str, rel_dir: &str, route: Option<&str>) -> String {
    let m = prefix
        .split('/')
        .map(routes::fs_seg_to_pattern)
        .collect::<Vec<_>>()
        .join("/");
    let conv_dir = |d: &str| {
        d.split('/')
            .map(routes::fs_seg_to_pattern)
            .collect::<Vec<_>>()
            .join("/")
    };
    match route.map(str::trim).filter(|r| !r.is_empty()) {
        None => {
            if rel_dir.is_empty() {
                m
            } else {
                format!("{m}/{}", conv_dir(rel_dir))
            }
        }
        Some(r) if r.starts_with('/') => r.trim_start_matches('/').to_string(),
        Some(r) => {
            if rel_dir.is_empty() {
                format!("{m}/{r}")
            } else {
                format!("{m}/{}/{r}", conv_dir(rel_dir))
            }
        }
    }
}

/// api.ts 只许作路由入口（spec §2.5）：它是 routes.js 的聚合单元而非可复用模块，
/// 被导入会把路由副作用（.route 声明、默认导出的 handler 表）拖进普通模块。
/// 目标 basename（剥扩展）== "api" 即拒绝——宁枉勿纵，报错给全部违规。
/// 扫描面 = 全部**本地** specifier（相对 + 别名）：`#user/item/api`、`#/user/item/api`
/// 与相对写法同等拦截（此前只看相对写法，换个前缀即可绕过）。
/// npm 裸包名不在守卫面（`import x from "pkg/api"` 是包内子路径，不是本项目的 api.ts）。
fn guard_no_api_imports(files: &[(String, String)]) -> Result<(), String> {
    let mut bad = Vec::new();
    for (rel, src) in files {
        for (_, _, spec) in specifier_spans(src) {
            if !is_local(&spec) {
                continue;
            }
            let target = spec.rsplit('/').next().unwrap_or("");
            let stem = target
                .strip_suffix(".ts")
                .or_else(|| target.strip_suffix(".js"))
                .unwrap_or(target);
            if stem == "api" {
                bad.push(format!(
                    "  {rel} imports {spec:?} (api.ts 是路由入口，不可被 import)"
                ));
            }
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("api.ts 不可被模块内 import：\n{}", bad.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_route_removes_assignments_only() {
        let src = "function get() {}\nget.route = \"{id}\";\nconst r = x.route;\nexport default { get };\n";
        let out = strip_route_decls(src);
        assert!(!out.contains(".route ="), "{out}");
        assert!(out.contains("function get()"), "{out}");
        assert!(out.contains("x.route"), "{out}"); // 读取不剥
    }

    /// 测试辅助：搭一棵真实 src 树（改写要探盘，字符串单测已不成立）。
    /// 返回 (src 根, 临时根)。
    fn fix_fixture(tag: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let t = std::env::temp_dir().join(format!(
            "oj-fix-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&t);
        let src = t.join("src");
        for (rel, content) in files {
            let p = src.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        (src, t)
    }

    /// 测试辅助：模块 a（0.1.0）+ b（0.2.0）双模块夹具，覆盖索引目录/别名/跨模块。
    fn two_modules(tag: &str) -> (PathBuf, PathBuf) {
        fix_fixture(
            tag,
            &[
                ("a/manifest.yaml", "name: a\ndesc: d\nversion: 0.1.0\n"),
                ("b/manifest.yaml", "name: b\ndesc: d\nversion: 0.2.0\n"),
                ("b/util.ts", "export const v = 1;\n"),
                ("a/_shared/validate.ts", "export const v = 1;\n"),
                ("a/_shared/mod/index.ts", "export const x = 1;\n"),
                ("a/y/g.ts", "export const g = 1;\n"),
                ("a/sub/_shared/deep.ts", "export const d = 1;\n"),
            ],
        )
    }

    /// 测试辅助：在模块 a 的某目录下改写（模块 a 0.1.0、给定版本视图）。
    fn fix_in(
        src_root: &Path,
        src: &str,
        rel_dir: &str,
        view: &std::collections::BTreeMap<String, String>,
    ) -> String {
        fix_import_specifiers(src_root, src, "a", "0.1.0", rel_dir, view, None).unwrap()
    }

    /// 版本视图 {b: 0.2.0}。
    fn view_b() -> std::collections::BTreeMap<String, String> {
        [("b".to_string(), "0.2.0".to_string())]
            .into_iter()
            .collect()
    }

    #[test]
    fn relative_imports_rewritten_to_product_paths() {
        let (src_root, t) = fix_fixture(
            "rel",
            &[
                ("a/manifest.yaml", "name: a\ndesc: d\nversion: 0.1.0\n"),
                ("a/_shared/validate.ts", "export const v = 1;\n"),
                ("a/_shared/deep.ts", "export const d = 1;\n"),
            ],
        );
        let src = "import { v } from \"../_shared/validate\";\nimport p from \"pkg\";\nexport { v } from \"../_shared/validate.ts\";\nconst s = \"from \\\"../_shared/validate\\\"\";\n";
        let out = fix_in(&src_root, src, "item", &Default::default());
        assert!(out.contains("\"../_shared/validate.js\""), "{out}");
        assert!(out.contains("from \"pkg\""), "{out}"); // npm 裸包名不动
        // 普通字符串里的 `from "…"` 不动（非导入位置——此前行级口径会误伤）
        assert!(
            out.contains(r#"const s = "from \"../_shared/validate\"";"#),
            "{out}"
        );
        // 深层目录：跨目录上溯仍正确
        let out = fix_in(
            &src_root,
            "import { d } from \"../../../_shared/deep\";\n",
            "item/x/y",
            &Default::default(),
        );
        assert!(out.contains("\"../../../_shared/deep.js\""), "{out}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 非 `.ts` 本地目标（`.js`/`.json`）不进产物（collect_module 白名单只收 `.ts`），
    /// 此前会静默产出悬空 specifier、release 才炸 → 现构建期 fail-fast。
    #[test]
    fn non_ts_local_target_fails_the_build() {
        let (src_root, t) = fix_fixture(
            "ext",
            &[
                ("a/manifest.yaml", "name: a\ndesc: d\nversion: 0.1.0\n"),
                ("a/item/plain.js", "export const p = 1;\n"),
                ("a/plain.js", "export const p = 1;\n"),
                ("a/item/d.json", "{}\n"),
            ],
        );
        for spec in ["./plain.js", "./d.json", "#plain.js"] {
            let e = fix_import_specifiers(
                &src_root,
                &format!("import x from \"{spec}\";\n"),
                "a",
                "0.1.0",
                "item",
                &Default::default(),
                None,
            )
            .unwrap_err();
            assert!(e.contains("不会进产物") && e.contains(spec), "{spec}: {e}");
        }
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 产物自洽断言：漏改写的**本地** specifier（扫描器未覆盖的写法）必须在构建期暴露，
    /// 而不是留到 release 运行期才炸。
    #[test]
    fn dist_consistency_assertion_catches_dangling_local_specifier() {
        let t = std::env::temp_dir().join(format!("oj-dist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let vdir = t.join("dist/m-0.1.0");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(
            vdir.join("api.js"),
            "import x from \"./missing.js\";\nimport y from \"pkg\";\n",
        )
        .unwrap();
        // 裸包名不在自洽面（运行期 node_modules 解析）
        assert!(assert_dist_consistent(std::slice::from_ref(&vdir)).is_err());
        std::fs::write(vdir.join("missing.js"), "export const x = 1;\n").unwrap();
        assert!(assert_dist_consistent(std::slice::from_ref(&vdir)).is_ok());
        // 残留别名同样被兜住
        std::fs::write(vdir.join("api.js"), "import x from \"#_shared/a\";\n").unwrap();
        let e = assert_dist_consistent(std::slice::from_ref(&vdir)).unwrap_err();
        assert!(e.contains("别名未实化"), "{e}");
        // 未参与本次构建的目录不扫（陈旧产物不该让本次构建失败）
        assert!(assert_dist_consistent(&[]).is_ok());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 回归：目录索引导入（`../_shared/mod` → `_shared/mod/index.ts`）。
    /// 修复前构建期只做字符串补后缀 → 产出悬空的 `../_shared/mod.js`（dev 能跑、release 炸）。
    #[test]
    fn directory_index_import_maps_to_index_js() {
        let (src_root, t) = two_modules("index");
        let out = fix_in(
            &src_root,
            "import { x } from \"../_shared/mod\";\n",
            "item",
            &Default::default(),
        );
        assert!(out.contains("\"../_shared/mod/index.js\""), "{out}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 别名实化：`#` 模块根 / `#/` src 根，产物内只剩相对 specifier。
    #[test]
    fn alias_imports_materialized_to_product_paths() {
        let (src_root, t) = two_modules("alias");
        let src = "import { v } from \"#_shared/validate\";\nimport { d } from \"#_shared/mod\";\nimport { u } from \"#/b/util\";\n";
        let out = fix_in(&src_root, src, "item/x/y", &view_b());
        // 本模块根锚点：与深度无关地落到同模块版本目录
        assert!(out.contains("\"../../../_shared/validate.js\""), "{out}");
        // 索引目录
        assert!(out.contains("\"../../../_shared/mod/index.js\""), "{out}");
        // src 根锚点 → 跨模块按 view 钉版本目录
        assert!(out.contains("\"../../../../b-0.2.0/util.js\""), "{out}");
        assert!(!out.contains('#'), "{out}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 副作用 import 与动态 import 一并改写（修复前只有行级 `from ` 口径会漏）。
    #[test]
    fn side_effect_and_dynamic_imports_are_rewritten() {
        let (src_root, t) = two_modules("dyn");
        let src = "import \"#_shared/validate\";\nconst m = await import(\"#_shared/mod\");\nimport(\"../_shared/validate\");\nconst route = `#/dashboard`;\n";
        let out = fix_in(&src_root, src, "item", &Default::default());
        assert!(out.contains("import \"../_shared/validate.js\""), "{out}");
        assert!(out.contains("import(\"../_shared/mod/index.js\")"), "{out}");
        assert!(!out.contains("#_shared"), "{out}");
        // 模板串里的 `#/…` 形状文本不动（非导入位置；hash 路由等业务字符串常见）
        assert!(out.contains("`#/dashboard`"), "{out}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn cross_module_import_rewrites_to_versioned_path() {
        let (src_root, t) = two_modules("xmod");
        // ① 模块根出发：../b/util → dist/b-0.2.0/util.js
        let out = fix_in(
            &src_root,
            "import { v } from \"../b/util\";\n",
            "",
            &view_b(),
        );
        assert!(out.contains("\"../b-0.2.0/util.js\""), "{out}");
        // ①' 子目录出发：../../b/util → 同样落到 dist/b-0.2.0/
        let out = fix_in(
            &src_root,
            "import { v } from \"../../b/util\";\n",
            "sub",
            &view_b(),
        );
        assert!(out.contains("\"../../b-0.2.0/util.js\""), "{out}");
        // ③ 嵌套 rel_dir：src/a/x/y/f.ts 导入 ../../../b/util
        let out = fix_in(
            &src_root,
            "import { v } from \"../../../b/util\";\n",
            "x/y",
            &view_b(),
        );
        assert!(out.contains("\"../../../b-0.2.0/util.js\""), "{out}");
        // 显式 .ts 后缀目标 → .js
        let out = fix_in(
            &src_root,
            "export { v } from \"../b/util.ts\";\n",
            "",
            &view_b(),
        );
        assert!(out.contains("\"../b-0.2.0/util.js\""), "{out}");
        // 模块内绕出再绕回（../../a/y/g 从 x/ 出发）→ 产物路径不悬空
        let out = fix_in(
            &src_root,
            "import { v } from \"../../a/y/g\";\n",
            "x",
            &Default::default(),
        );
        assert!(out.contains("\"../y/g.js\""), "{out}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn cross_module_import_without_version_fails_fast() {
        let (src_root, t) = two_modules("nover");
        // ② 视图缺 b → Err 报目标模块并提示先构建
        let e = fix_import_specifiers(
            &src_root,
            "import { v } from \"../b/util\";\n",
            "a",
            "0.1.0",
            "",
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(e.contains("b") && e.contains("oj build"), "{e}");
        // 别名跨模块同样受版本门禁（`#/b/…` 与相对写法等价）
        let e = fix_import_specifiers(
            &src_root,
            "import { v } from \"#/b/util\";\n",
            "a",
            "0.1.0",
            "",
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(e.contains("b") && e.contains("oj build"), "{e}");
        // 目标不存在 → Err 含尝试过的候选（探针口径）
        let e = fix_import_specifiers(
            &src_root,
            "import { v } from \"../b/nope\";\n",
            "a",
            "0.1.0",
            "",
            &view_b(),
            None,
        )
        .unwrap_err();
        assert!(e.contains("tried"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn rel_pattern_rules() {
        // 镜像行：模块名 + 目录段
        assert_eq!(rel_pattern("user", "account", None), "user/account");
        assert_eq!(rel_pattern("user", "", None), "user");
        assert_eq!(
            rel_pattern("user", "profile/detail", None),
            "user/profile/detail"
        );
        // 相对 .route 声明
        assert_eq!(rel_pattern("user", "item", Some("{id}")), "user/item/{id}");
        assert_eq!(rel_pattern("user", "", Some("{id}")), "user/{id}");
        // 根级声明（/ 开头）：剥首斜杠，不加模块段
        assert_eq!(
            rel_pattern("user", "item", Some("/v2/user/{id}")),
            "v2/user/{id}"
        );
        // 空 route 视同未挂
        assert_eq!(rel_pattern("user", "item", Some("")), "user/item");
    }

    #[test]
    fn rel_pattern_converts_underscore_dirs_and_module() {
        // 目录镜像：rel_dir 与模块段逐段转换（v0.1.27 `_name_` → `{name}`）
        assert_eq!(rel_pattern("user", "_id_", None), "user/{id}");
        assert_eq!(rel_pattern("_mod_", "x", None), "{mod}/x");
        assert_eq!(rel_pattern("_mod_", "_id_", None), "{mod}/{id}");
        // 相对 .route 拼在转换后的目录后
        assert_eq!(
            rel_pattern("user", "_id_", Some("{sub}")),
            "user/{id}/{sub}"
        );
        // 根级 .route 不吃目录转换
        assert_eq!(rel_pattern("user", "_id_", Some("/v2/x")), "v2/x");
        // .route 值不转换：`_name_` 在其中是字面段
        assert_eq!(rel_pattern("user", "item", Some("_id_")), "user/item/_id_");
        // 不转换的形态保持字面
        assert_eq!(rel_pattern("user", "_shared", None), "user/_shared");
        assert_eq!(rel_pattern("user", "__x__", None), "user/__x__");
    }

    #[test]
    fn residual_alias_fails_the_build() {
        // 残留断言（扫描器天花板兜底）：产物里还有 `#` specifier → 显式失败
        assert!(assert_no_aliases("import x from \"#_shared/a\";\n", "m/api.js").is_err());
        assert!(assert_no_aliases("import x from \"./a.js\";\n", "m/api.js").is_ok());
        // 注释里的 `#` 不算（非导入位置）
        assert!(assert_no_aliases("// import x from \"#a\"\n", "m/api.js").is_ok());
    }

    #[test]
    fn guard_rejects_api_imports() {
        let files = vec![
            (
                "_shared/util.ts".into(),
                "import { g } from \"../account/api\";\n".into(),
            ),
            (
                "account/api.ts".into(),
                "import { v } from \"../_shared/validate\";\n".into(),
            ),
        ];
        let e = guard_no_api_imports(&files).unwrap_err();
        assert!(
            e.contains("_shared/util.ts") && e.contains("../account/api"),
            "{e}"
        );
        // 无违规
        assert!(guard_no_api_imports(&[("x.ts".into(), "import m from \"pkg\";".into())]).is_ok());
    }

    /// 别名写法不得绕过 api.ts 禁令（此前守卫只看相对 specifier，换个前缀即可溜过）。
    #[test]
    fn guard_rejects_api_imports_via_alias() {
        let files = vec![
            (
                "_shared/util.ts".into(),
                "import { g } from \"#item/api\";\n".into(),
            ),
            (
                "account/api.ts".into(),
                "import { g } from \"#/user/item/api\";\n".into(),
            ),
        ];
        let e = guard_no_api_imports(&files).unwrap_err();
        assert!(
            e.contains("_shared/util.ts") && e.contains("#item/api"),
            "{e}"
        );
        assert!(e.contains("#/user/item/api"), "{e}");
        // npm 包内子路径 `pkg/api` 不在守卫面（那是第三方包的入口，不是本项目的 api.ts）
        assert!(
            guard_no_api_imports(&[("x.ts".into(), "import a from \"pkg/api\";".into())]).is_ok()
        );
        // 名字含 api 但不是 api.ts（api-helper）不误伤
        assert!(
            guard_no_api_imports(&[(
                "x.ts".into(),
                "import a from \"#_shared/api-helper\";".into()
            )])
            .is_ok()
        );
    }

    /// 测试辅助：目录下唯一文件的文件名（String）。
    fn only_file(dir: &std::path::Path) -> String {
        let mut it = std::fs::read_dir(dir).unwrap();
        let name = it.next().unwrap().unwrap().file_name();
        assert!(
            it.next().is_none(),
            "expected exactly one file in {}",
            dir.display()
        );
        name.to_string_lossy().into_owned()
    }

    /// 测试辅助：摆一个 src（user 带 .route + _shared，other 纯镜像）。
    fn src_fixture(t: &std::path::Path) {
        let src = t.join("src");
        for d in ["user/item", "user/_shared", "other/list"] {
            std::fs::create_dir_all(src.join(d)).unwrap();
        }
        std::fs::write(
            src.join("user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            src.join("other/manifest.yaml"),
            "name: other\ndesc: d\nversion: 0.9.0\n",
        )
        .unwrap();
        std::fs::write(
            src.join("user/_shared/validate.ts"),
            "export const v = 1;\n",
        )
        .unwrap();
        std::fs::write(src.join("user/item/api.ts"),
            "import { v } from \"../_shared/validate\";\nfunction get(){ json.ok({v}); }\nget.route = \"{id}\";\nexport default { get };\n").unwrap();
        std::fs::write(
            src.join("other/list/api.ts"),
            "function get(){ json.ok({}); }\nexport default { get };\n",
        )
        .unwrap();
    }

    fn build_args(t: &std::path::Path, module: Option<&str>) -> BuildArgs {
        BuildArgs {
            module: module.map(str::to_string),
            config: Some(t.join("config.yaml").display().to_string()),
            dir: t.join("src").display().to_string(),
            module_root: None,
            out: t.join("dist").display().to_string(),
            minify: true,
            check: false,
            bundle: false,
        }
    }

    /// `--module-root` 形态的 BuildArgs（与 `-d` 互斥，故 dir 字段不再参与）。
    fn module_root_args(t: &std::path::Path, module: Option<&str>) -> BuildArgs {
        BuildArgs {
            module: module.map(str::to_string),
            config: Some(t.join("config.yaml").display().to_string()),
            dir: String::new(),
            module_root: Some(t.join("src").display().to_string()),
            out: t.join("dist").display().to_string(),
            minify: true,
            check: false,
            bundle: false,
        }
    }

    /// 嵌套模块根夹具：模块根在第二层（目录名 ≠ 模块名），m2 声明了 URL 前缀。
    /// m1 跨模块引用 m2（走 `#/m2/…` 别名，已声明 deps）。
    fn nested_fixture(t: &std::path::Path) {
        let src = t.join("src");
        std::fs::create_dir_all(src.join("foo/api/xxx/yyy")).unwrap();
        std::fs::create_dir_all(src.join("bar/api/shared")).unwrap();
        std::fs::write(
            src.join("foo/api/manifest.yaml"),
            "name: m1\ndesc: d\nversion: 0.1.0\ndeps:\n  m2: \"^0.2.0\"\n",
        )
        .unwrap();
        std::fs::write(
            src.join("bar/api/manifest.yaml"),
            "name: m2\ndesc: d\nversion: 0.2.0\nprefix: sys/user\n",
        )
        .unwrap();
        std::fs::write(src.join("bar/api/shared/u.ts"), "export const u = 1;\n").unwrap();
        std::fs::write(
            src.join("bar/api/api.ts"),
            "function get(){ json.ok({}); }\nexport default { get };\n",
        )
        .unwrap();
        std::fs::write(
            src.join("foo/api/xxx/yyy/api.ts"),
            "import { u } from \"#/m2/shared/u\";\nfunction get(){ json.ok({u}); }\nexport default { get };\n",
        )
        .unwrap();
    }

    /// routes.js 里的 pattern 清单（产物形态：`pattern: "xxx"`）。
    fn routes_patterns(vdir: &std::path::Path) -> Vec<String> {
        let txt = std::fs::read_to_string(vdir.join("routes.js")).unwrap();
        txt.match_indices("pattern: \"")
            .map(|(i, _)| {
                let rest = &txt[i + "pattern: \"".len()..];
                rest[..rest.find('"').unwrap()].to_string()
            })
            .collect()
    }

    /// BDD：模块根可不在首层时，URL 的模块段取 `manifest.yaml` 的 `prefix`
    /// （缺省 = name），目录名（`foo/api`）不进 URL；产物路径以模块根为基。
    #[tokio::test]
    async fn given_nested_module_root_when_build_then_url_uses_prefix_and_dir_names_absent() {
        let t = std::env::temp_dir().join(format!("oj-build-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        nested_fixture(&t);
        run(&module_root_args(&t, None)).await.unwrap();

        // Then ①：URL 前缀 = manifest 的 prefix（缺省 = name），目录名不出现
        let m1 = std::fs::read_to_string(t.join("dist/m1-0.1.0/routes.js")).unwrap();
        assert!(m1.contains("pattern: \"m1/xxx/yyy\""), "{m1}");
        assert!(m1.contains("file: \"xxx/yyy/api.js\""), "{m1}");
        let m2 = std::fs::read_to_string(t.join("dist/m2-0.2.0/routes.js")).unwrap();
        assert!(m2.contains("pattern: \"sys/user\""), "{m2}");

        // Then ②：跨模块别名按**模块名**解析并落到目标版本目录（目录名不进产物路径）
        let api = std::fs::read_to_string(t.join("dist/m1-0.1.0/xxx/yyy/api.js")).unwrap();
        assert!(api.contains("m2-0.2.0/shared/u.js"), "{api}");
        assert!(!api.contains("bar"), "{api}");
        assert!(!api.contains('#'), "{api}");

        // Then ③：位置参数是**模块名**（`oj build m1`），不是目录名
        std::fs::remove_dir_all(t.join("dist")).unwrap();
        // 先出 m1 的依赖（deps 校验口径），再单出 m1
        run(&module_root_args(&t, Some("m2"))).await.unwrap();
        run(&module_root_args(&t, Some("m1"))).await.unwrap();
        assert!(t.join("dist/m1-0.1.0").is_dir());
        // 单模块构建不牵连他人：版本锁保留既有条目（PRD §7-7）
        let lock = std::fs::read_to_string(t.join("dist/manifests.yaml")).unwrap();
        assert!(lock.contains("m2: 0.2.0"), "{lock}");
        assert!(lock.contains("m1: 0.1.0"), "{lock}");
        // 目录名当模块名用不了（索引里只有 m1/m2）
        let e = run(&module_root_args(&t, Some("foo"))).await.unwrap_err();
        assert!(e.contains("foo"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD（PRD §7-1 兼容）：**旧布局**下 `--module-root src` 的产物与 `-d src` 逐字节一致
    /// （oj 为确定性构建；`.tgz` 含打包时间戳，只比存在性与文件名）。
    #[tokio::test]
    async fn given_flat_layout_when_module_root_then_dist_is_byte_identical_to_plain_d() {
        let t = std::env::temp_dir().join(format!("oj-build-compat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);

        run(&build_args(&t, None)).await.unwrap();
        let a = dist_snapshot(&t.join("dist"));
        std::fs::remove_dir_all(t.join("dist")).unwrap();

        run(&module_root_args(&t, None)).await.unwrap();
        let b = dist_snapshot(&t.join("dist"));

        assert!(!a.is_empty(), "夹具应产出文件");
        assert_eq!(a, b, "`--module-root` 在旧布局下不得改变产物");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD（PRD §7-6）：`--module-root` 命中 0 个模块 → 非零（Err）+ 可读下一步，
    /// 不得像默认口径那样静默成功（静默会产出空 dist 并让下游误判「构建通过」）。
    #[tokio::test]
    async fn given_module_root_without_any_manifest_when_build_then_fails_with_next_step() {
        let t = std::env::temp_dir().join(format!("oj-build-nohit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/web")).unwrap();
        std::fs::write(t.join("src/web/index.ts"), "export const a = 1;\n").unwrap();

        let e = run(&module_root_args(&t, None)).await.unwrap_err();
        assert!(
            e.contains("no manifest.yaml") && e.contains("--module-root"),
            "{e}"
        );
        assert!(e.contains("下一步"), "{e}");
        // 对照：默认口径（`-d`）对空树仍是今日语义（0 模块亦成功），故不在此断言。
        assert!(!t.join("dist").exists(), "失败不得落盘半成品");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD（评审 R2 / 实测事故）：`--module-root` 指到 **release 产物树** → fail-fast。
    /// 产物树的 `manifests.yaml` 是版本锁、子目录是 `<name>-<version>/`，按模块根扫会
    /// 静默产出一堆空壳版本目录（0 api file 却「构建成功」）。
    #[tokio::test]
    async fn given_release_tree_when_module_root_then_fails_fast() {
        let t = std::env::temp_dir().join(format!("oj-build-reltree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, None)).await.unwrap();

        let mut a = module_root_args(&t, None);
        a.module_root = Some(t.join("dist").display().to_string());
        let e = run(&a).await.unwrap_err();
        assert!(e.contains("--module-root is a source-tree option"), "{e}");
        assert!(e.contains("release tree"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// dist 快照：相对路径 → 内容（`.tgz` 只记文件名，内含打包时间戳不可比）。
    fn dist_snapshot(dist: &std::path::Path) -> std::collections::BTreeMap<String, String> {
        let mut m = std::collections::BTreeMap::new();
        let mut stack = vec![dist.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let rel = p.strip_prefix(dist).unwrap().display().to_string();
                let v = if rel.ends_with(".tgz") {
                    String::from("<tgz>")
                } else {
                    std::fs::read_to_string(&p).unwrap_or_default()
                };
                m.insert(rel, v);
            }
        }
        m
    }

    /// BDD：dev 路由表与 release 产物 URL **逐条一致**（评审发现丙：两者会分叉）。
    #[tokio::test]
    async fn given_nested_module_root_when_serve_dev_then_urls_match_release_artifacts() {
        let t = std::env::temp_dir().join(format!("oj-build-urls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        nested_fixture(&t);
        // canonicalize：/var → /private/var（macOS），保证与 loader 看到的路径同形态
        let src = t.join("src").canonicalize().unwrap();
        let idx = only_js::bridge::resolve_roots(&src, true, None).unwrap();
        run(&module_root_args(&t, None)).await.unwrap();

        // When：dev 侧按同一份索引建路由表（`oj serve --module-root` / `oj openapi` 同路径）
        let table =
            crate::openapi_cmd::discover_routes(&src, &t, true, "/v1/api", Some(&idx)).unwrap();
        let mut dev: Vec<String> = table
            .listing()
            .iter()
            .map(|r| r.pattern.trim_start_matches("/v1/api/").to_string())
            .collect();
        dev.sort();

        // Then：与 release 产物的 pattern 逐条一致（去掉前导 base 后比对）
        let mut rel = routes_patterns(&t.join("dist/m1-0.1.0"));
        rel.extend(routes_patterns(&t.join("dist/m2-0.2.0")));
        rel.sort();
        assert_eq!(dev, rel, "dev 与 release 的 URL 必须同源");
        assert!(dev.iter().any(|p| p == "m1/xxx/yyy"), "{dev:?}");
        assert!(dev.iter().any(|p| p == "sys/user"), "{dev:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD（T10，评审 F2）：src/tasks 存在 → 全部 .ts 转译镜像到 dist/tasks/
    /// （保目录结构、.ts→.js、相对 import 补 .js 后缀；共享库一并镜像）；
    /// 不进 tgz/manifests（任务非版本化模块）。
    #[tokio::test]
    async fn given_src_with_tasks_when_build_then_dist_tasks_transpiled() {
        let t = std::env::temp_dir().join(format!("oj-build-tasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("src/tasks/_shared")).unwrap();
        std::fs::write(
            t.join("src/tasks/task_demo.ts"),
            "import { tick } from \"./_shared/tick\";\nimport \"./_shared/side\";\nconst n: number = 1;\nwhile (true) { await Promise.resolve(tick(n)); }\n",
        )
        .unwrap();
        std::fs::write(t.join("src/tasks/_shared/side.ts"), "export {};\n").unwrap();
        std::fs::write(
            t.join("src/tasks/_shared/tick.ts"),
            "export function tick(n: number): number { return n; }\n",
        )
        .unwrap();
        run(&build_args(&t, None)).await.unwrap();
        let demo = std::fs::read_to_string(t.join("dist/tasks/task_demo.js")).unwrap();
        assert!(demo.contains("let n=1"), "{demo}"); // 类型已剥（转译产物，默认 minify）
        assert!(
            demo.contains("\"./_shared/tick.js\"") && demo.contains("\"./_shared/side.js\""),
            "{demo}"
        ); // 具名 + 副作用 import 均已补 .js 后缀（审查 #7）
        assert!(
            std::fs::read_to_string(t.join("dist/tasks/_shared/tick.js"))
                .unwrap()
                .contains("function tick("),
            "shared lib transpiled"
        );
        // 非版本化：不落锁、不打 tgz。
        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert!(!lock.contains_key("tasks"), "{lock:?}");
        assert!(!t.join("dist/tasks.tgz").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD（T10）：无 src/tasks → 不产生 dist/tasks。
    #[tokio::test]
    async fn given_src_without_tasks_when_build_then_no_tasks_dir() {
        let t = std::env::temp_dir().join(format!("oj-build-notasks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, None)).await.unwrap();
        assert!(!t.join("dist/tasks").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// v0.1.64 嵌套 handler 组：routes.js 为嵌套 verb 落 `handler` 链；
    /// 顶层动词（链 == 方法）不写该字段（旧产物字节形态不变）。
    #[tokio::test]
    async fn build_emits_handler_chain_for_nested_groups() {
        let t = std::env::temp_dir().join(format!("oj-build-nest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/user/item")).unwrap();
        std::fs::write(
            t.join("src/user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/item/api.ts"),
            "function get(){ json.ok({}); }\n\
             const upload = { post: () => { json.ok({}); } };\n\
             upload.route = \"/x/{anchor}/upload\";\n\
             export default { get, upload };\n",
        )
        .unwrap();
        run(&build_args(&t, Some("user"))).await.unwrap();
        let routes = std::fs::read_to_string(t.join("dist/user-0.1.0/routes.js")).unwrap();
        // 嵌套组 verb → handler 链 + 组 .route 的根级 pattern
        assert!(routes.contains("handler: \"upload.post\""), "{routes}");
        assert!(routes.contains("\"x/{anchor}/upload\""), "{routes}");
        // 顶层动词：链即方法，不写 handler（旧产物兼容）
        assert!(!routes.contains("handler: \"get\""), "{routes}");
        assert!(routes.contains("\"user/item\""), "{routes}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 判别红线：非 verb 键挂函数（此前的静默丢弃陷阱）→ 构建期可操作失败。
    #[tokio::test]
    async fn build_rejects_non_verb_function_export() {
        let t = std::env::temp_dir().join(format!("oj-build-nonverb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/user")).unwrap();
        std::fs::write(
            t.join("src/user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/api.ts"),
            "function helper(){ json.ok({}); }\n\
             export default { get(){ json.ok({}); }, helper };\n",
        )
        .unwrap();
        let err = run(&build_args(&t, Some("user"))).await.unwrap_err();
        assert!(err.contains("helper"), "{err}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_converts_underscore_dirs_and_validates_patterns() {
        // v0.1.27：`_name_` 目录段 → routes.js pattern `{name}`，file 保留磁盘路径；
        // 非法 pattern 构建期 fail-fast；`.route` 值内 `_name_` 是字面段（不转换）。
        let t = std::env::temp_dir().join(format!("oj-build-us-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/user/_id_")).unwrap();
        std::fs::write(
            t.join("src/user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/_id_/api.ts"),
            "function get(){ json.ok({id: http.param(\"id\")}); }\nexport default { get };\n",
        )
        .unwrap();
        run(&build_args(&t, None)).await.unwrap();
        let routes = std::fs::read_to_string(t.join("dist/user-0.1.0/routes.js")).unwrap();
        assert!(routes.contains("\"user/{id}\""), "{routes}");
        assert!(routes.contains("\"_id_/api.js\""), "{routes}");

        // `.route = "_id_"`：字面段，构建成功且不转换
        std::fs::write(
            t.join("src/user/_id_/api.ts"),
            "function get(){ json.ok({}); }\nget.route = \"_id_\";\nexport default { get };\n",
        )
        .unwrap();
        run(&build_args(&t, None)).await.unwrap();
        let routes = std::fs::read_to_string(t.join("dist/user-0.1.0/routes.js")).unwrap();
        assert!(routes.contains("\"user/{id}/_id_\""), "{routes}");

        // 非法 pattern（matchit 混合段）→ 构建期 fail-fast
        std::fs::write(
            t.join("src/user/_id_/api.ts"),
            "function get(){ json.ok({}); }\nget.route = \"{id}.json\";\nexport default { get };\n",
        )
        .unwrap();
        let err = run(&build_args(&t, None)).await.unwrap_err();
        assert!(err.contains("invalid route pattern"), "{err}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_module_emits_versioned_artifacts() {
        let t = std::env::temp_dir().join(format!("oj-build-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, Some("user"))).await.unwrap();

        let vd = t.join("dist/user-0.1.0");
        assert_eq!(only_file(&vd.join("item")), "api.js"); // 保留原目录结构原名

        let routes = std::fs::read_to_string(vd.join("routes.js")).unwrap();
        assert!(routes.contains("\"user/item/{id}\""), "{routes}"); // pattern 无 base 含模块段
        assert!(routes.contains("\"item/api.js\""), "{routes}"); // file 含目录段
        assert!(!routes.contains("/v1/api"), "{routes}");

        let item_js = std::fs::read_to_string(vd.join("item/api.js")).unwrap();
        assert!(!item_js.contains(".route"), "{item_js}"); // .route 已剥
        assert!(item_js.contains("\"../_shared/validate.js\""), "{item_js}"); // import 后缀已补
        assert!(!item_js.contains('\n'), "{item_js}"); // 默认 minify：单行

        assert!(vd.join("manifest.yaml").is_file()); // 原样复制
        assert!(vd.join("_shared/validate.js").is_file()); // 非 api 原路径
        assert!(!item_js.contains("sourceMappingURL"), "{item_js}"); // minify 剥内联 sourcemap

        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
        assert!(!lock.contains_key("other")); // 单模块构建不动他人
        assert!(t.join("dist/user-0.1.0.tgz").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD：别名端到端——同一份源码跑 `oj build`，产物内**只剩相对 specifier**，
    /// 本模块别名落同版本目录、跨模块别名按锁钉到目标版本目录。
    #[tokio::test]
    async fn alias_build_materializes_to_versioned_relative_paths() {
        let t = std::env::temp_dir().join(format!("oj-build-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("src/other/_shared")).unwrap();
        std::fs::write(t.join("src/other/_shared/util.ts"), "export const u = 1;\n").unwrap();
        // S008：别名跨模块引用必须声明 deps（这正是本用例要顺带钉住的门禁）
        std::fs::write(
            t.join("src/user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\ndeps:\n  other: \"^0.9.0\"\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/item/api.ts"),
            "import { v } from \"#_shared/validate\";\nimport { u } from \"#/other/_shared/util\";\nfunction get(){ json.ok({v,u}); }\nget.route = \"{id}\";\nexport default { get };\n",
        )
        .unwrap();
        run(&build_args(&t, None)).await.unwrap();

        let item = std::fs::read_to_string(t.join("dist/user-0.1.0/item/api.js")).unwrap();
        assert!(item.contains("\"../_shared/validate.js\""), "{item}");
        assert!(
            item.contains("\"../../other-0.9.0/_shared/util.js\""),
            "{item}"
        );
        assert!(!item.contains('#'), "{item}");
        // 全产物扫描：任何落盘 .js 都不得残留别名
        let mut stack = vec![t.join("dist")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "js") {
                    let js = std::fs::read_to_string(&p).unwrap();
                    assert!(
                        assert_no_aliases(&js, &p.display().to_string()).is_ok(),
                        "{js}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD：跨模块别名未声明 deps → S008 拦住构建（fail build，报错给下一步）。
    #[tokio::test]
    async fn build_rejects_cross_module_alias_without_deps() {
        let t = std::env::temp_dir().join(format!("oj-build-nodeps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("src/other/_shared")).unwrap();
        std::fs::write(t.join("src/other/_shared/util.ts"), "export const u = 1;\n").unwrap();
        std::fs::write(
            t.join("src/user/item/api.ts"),
            "import { u } from \"#/other/_shared/util\";\nfunction get(){ json.ok({u}); }\nexport default { get };\n",
        )
        .unwrap();
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(
            e.contains("S008") && e.contains("other") && e.contains("deps"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD：tasks 池 import 越过池根（跨模块/池外目标）→ 产物里必然不存在，fail build。
    #[tokio::test]
    async fn build_rejects_tasks_import_escaping_pool() {
        let t = std::env::temp_dir().join(format!("oj-build-tesc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("src/tasks")).unwrap();
        std::fs::write(
            t.join("src/tasks/task_esc.ts"),
            "import { v } from \"../../../user/_shared/validate\";\nwhile (true) { await Promise.resolve(v); }\n",
        )
        .unwrap();
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(e.contains("越过任务池根") && e.contains("_shared"), "{e}");
        // 池内互导（含子目录）仍然合法
        std::fs::create_dir_all(t.join("src/tasks/_shared")).unwrap();
        std::fs::write(
            t.join("src/tasks/_shared/tick.ts"),
            "export const tick = 1;\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/tasks/task_esc.ts"),
            "import { tick } from \"./_shared/tick\";\nwhile (true) { await Promise.resolve(tick); }\n",
        )
        .unwrap();
        run(&build_args(&t, None)).await.unwrap();
        let _ = std::fs::remove_dir_all(&t);
    }

    /// BDD：tasks 池是**非版本化**资产（不进锁 / 不打 tgz）→ 别名一律拒绝，fail build。
    #[tokio::test]
    async fn build_rejects_alias_in_tasks_pool() {
        let t = std::env::temp_dir().join(format!("oj-build-talias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("src/tasks")).unwrap();
        std::fs::write(
            t.join("src/tasks/task_alias.ts"),
            "import { v } from \"#_shared/validate\";\nwhile (true) { await Promise.resolve(v); }\n",
        )
        .unwrap();
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(
            e.contains("别名") && e.contains("非版本化") && e.contains("#_shared/validate"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_is_deterministic_and_wipes_on_change() {
        let t = std::env::temp_dir().join(format!("oj-build-wipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, Some("user"))).await.unwrap();
        let snap = |p: &std::path::Path| std::fs::read(p).unwrap();
        let (js1, tgz1) = (
            snap(&t.join("dist/user-0.1.0/item/api.js")),
            snap(&t.join("dist/user-0.1.0.tgz")),
        );
        // 内容未变 → 重建字节一致（转译 + minify 确定性，落点 tgz）
        run(&build_args(&t, Some("user"))).await.unwrap();
        assert_eq!(snap(&t.join("dist/user-0.1.0/item/api.js")), js1);
        assert_eq!(
            snap(&t.join("dist/user-0.1.0.tgz")),
            tgz1,
            "同输入两次构建 tgz 必须字节一致"
        );
        // 内容变更 → 产物更新，目录内仍恰好 1 个 api.js（同版本清场）
        std::fs::write(
            t.join("src/user/item/api.ts"),
            "function get(){ json.ok({v:2}); }\nget.route = \"{id}\";\nexport default { get };\n",
        )
        .unwrap();
        run(&build_args(&t, Some("user"))).await.unwrap();
        let js2 = snap(&t.join("dist/user-0.1.0/item/api.js"));
        assert_ne!(js2, js1);
        assert_eq!(only_file(&t.join("dist/user-0.1.0/item")), "api.js");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn no_minify_keeps_readable_output() {
        let t = std::env::temp_dir().join(format!("oj-build-nomin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        let mut a = build_args(&t, Some("user"));
        a.minify = false;
        run(&a).await.unwrap();
        let js = std::fs::read_to_string(t.join("dist/user-0.1.0/item/api.js")).unwrap();
        assert!(js.contains('\n'), "{js}"); // 未压缩：多行可读
        assert!(js.contains("function get"), "{js}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_rejects_api_import() {
        let t = std::env::temp_dir().join(format!("oj-build-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/user/_shared")).unwrap();
        std::fs::write(
            t.join("src/user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/_shared/x.ts"),
            "import { g } from \"../item/api\";\n",
        )
        .unwrap();
        let e = run(&build_args(&t, Some("user")))
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("不可被"), "{e}"); // 守卫专属文案（"api" 子串近似恒真）
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_all_modules() {
        let t = std::env::temp_dir().join(format!("oj-build-all-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        run(&build_args(&t, None)).await.unwrap();
        assert!(t.join("dist/user-0.1.0/routes.js").is_file());
        assert!(t.join("dist/other-0.9.0/routes.js").is_file());
        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.len(), 2, "{lock:?}");
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 测试辅助：单模块 src（name/version 可注入）。
    fn one_module(t: &std::path::Path, name: &str, version: &str) {
        std::fs::create_dir_all(t.join("src").join(name)).unwrap();
        std::fs::write(
            t.join("src").join(name).join("manifest.yaml"),
            format!("name: {name}\ndesc: d\nversion: {version}\n"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn build_all_rejects_illegal_module_dir() {
        // 全量路径的模块名同样是信任边界输入（I-1）：fail-fast 且锁不被污染
        let t = std::env::temp_dir().join(format!("oj-build-illegal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "bad module", "0.1.0");
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(e.contains("illegal module"), "{e}");
        assert!(!t.join("dist/manifests.yaml").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_rejects_illegal_version() {
        let t = std::env::temp_dir().join(format!("oj-build-ver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "user", "0..1");
        let e = run(&build_args(&t, Some("user")))
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("illegal version") && e.contains("0..1"), "{e}");
        assert!(!t.join("dist/manifests.yaml").is_file());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_all_rejects_vdir_collision() {
        // {m}-{v} 不单射：a v1-x 与 a-1 vx 同落 dist/a-1-x（后者构建清场前者）→ 计划期 Err
        let t = std::env::temp_dir().join(format!("oj-build-vdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "a", "1-x");
        one_module(&t, "a-1", "x");
        let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
        assert!(e.contains("collision") && e.contains("a-1-x"), "{e}");
        assert!(!t.join("dist/a-1-x").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_rejects_collision_with_stale_lock_entry() {
        // R-1：撞名比对含锁内条目——锁 {a-1: x} 陈旧残留时，单建 a v1-x 同落 dist/a-1-x → Err
        let t = std::env::temp_dir().join(format!("oj-build-vdir2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        one_module(&t, "a", "1-x");
        std::fs::create_dir_all(t.join("dist")).unwrap();
        std::fs::write(t.join("dist/manifests.yaml"), "a-1: x\n").unwrap();
        let e = run(&build_args(&t, Some("a")))
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("collision") && e.contains("a-1-x"), "{e}");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn single_build_preserves_other_lock_entries() {
        let t = std::env::temp_dir().join(format!("oj-build-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("dist")).unwrap();
        std::fs::write(t.join("dist/manifests.yaml"), "other: 0.9.0\n").unwrap(); // 预置他模块
        run(&build_args(&t, Some("user"))).await.unwrap();
        let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
        assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
        assert_eq!(lock.get("other").map(String::as_str), Some("0.9.0")); // spec §6：保留
        let _ = std::fs::remove_dir_all(&t);
    }

    /// P0 白名单：模块 seed.sql / schema.yaml / migrations/*.sql 原样进 dist；
    /// fixtures/ 整目录排除（演示数据不随产物发布）。
    #[tokio::test]
    async fn build_copies_module_sql_and_excludes_fixtures() {
        let t = std::env::temp_dir().join(format!("oj-build-sql-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        one_module(&t, "user", "0.1.0");
        // S005：tables 声明与 schema.yaml 一致（build 内嵌检查）。
        std::fs::write(
            t.join("src/user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\ntables: [account]\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/seed.sql"),
            "INSERT OR IGNORE INTO account VALUES (1, 'neo');\n",
        )
        .unwrap();
        std::fs::write(
            t.join("src/user/schema.yaml"),
            "tables:\n  account:\n    pk: id\n    columns:\n      id: { type: integer }\n",
        )
        .unwrap();
        std::fs::create_dir_all(t.join("src/user/migrations")).unwrap();
        std::fs::write(
            t.join("src/user/migrations/0001__init.sql"),
            "CREATE TABLE account (id, name);\n",
        )
        .unwrap();
        std::fs::create_dir_all(t.join("src/user/fixtures")).unwrap();
        std::fs::write(
            t.join("src/user/fixtures/demo.sql"),
            "INSERT INTO account VALUES (9);\n",
        )
        .unwrap();
        run(&build_args(&t, Some("user"))).await.unwrap();
        let vd = t.join("dist/user-0.1.0");
        // 原样 copy（字节一致）
        assert_eq!(
            std::fs::read(vd.join("seed.sql")).unwrap(),
            b"INSERT OR IGNORE INTO account VALUES (1, 'neo');\n"
        );
        assert!(vd.join("schema.yaml").is_file());
        assert!(vd.join("migrations/0001__init.sql").is_file());
        // fixtures 整目录不进产物
        assert!(!vd.join("fixtures").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[tokio::test]
    async fn build_errs_on_corrupt_lock() {
        // 坏锁（非法 YAML）→ Err，不得 unwrap_or_default 当空表静默重置（I-2）
        let t = std::env::temp_dir().join(format!("oj-build-badlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        src_fixture(&t);
        std::fs::create_dir_all(t.join("dist")).unwrap();
        std::fs::write(t.join("dist/manifests.yaml"), "user: [unclosed\n").unwrap();
        let e = run(&build_args(&t, Some("user")))
            .await
            .err()
            .unwrap_or_default();
        assert!(e.contains("manifests.yaml"), "{e}");
        assert!(
            std::fs::read_to_string(t.join("dist/manifests.yaml"))
                .unwrap()
                .contains("unclosed")
        );
        let _ = std::fs::remove_dir_all(&t);
    }
}
