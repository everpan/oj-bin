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
#[path = "build_cmd_tests.rs"]
mod tests;
