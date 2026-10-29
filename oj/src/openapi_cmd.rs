//! `oj openapi`：从路由表生成 OpenAPI 3.1，并支持 `--check` 漂移校验（PR-6 第一步）。
//!
//! 设计：
//! - 路由发现双模复用既有路径——dev 走 `serve::build_table`（V8 内省 .route），
//!   release 走 `dist/manifests.yaml` + 各模块 `routes.js`（与 serve 装配同构，见
//!   `app.rs` 的 release 分支）。
//! - 请求体 / 响应 schema / 入参校验由 `.schema` 契约补出（v0.1.44+），spec 生成
//!   本体在 `serve::openapi`（MCP 双线路 R1 迁入，字节级不变）——本模块保留路由
//!   发现与 emit/drift-check。
//! - 漂移校验对生成物与已提交 `openapi.json` 做**键序规范化**比较，不一致即非零退出
//!   （CI 门禁），并打印重生成命令。

use only_js::bridge::{Bridge, Extras, InMemoryKV, LoaderShared, ModuleIndex, SchemaRegistry};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::args::OpenApiArgs;

/// spec 生成本体（自本模块迁入 serve，R1）；本模块仅保留路由发现与落盘/校验。
pub(crate) use serve::openapi::generate;

/// 双模路由发现：dev(ts=true) 走 V8 内省；release(ts=false) 走 dist 直载。
/// `index`（`--module-root` 下有值）：dev 侧路由的模块段与模块内路径由索引给出，
/// 与 `serve` 装配（`app.rs`）同源——`oj openapi --check` 比的就是这两条路径的产物。
///
/// `project_root`：bridge loader 的裸包解析行走上限（Node node_modules 算法），
/// 必须锚 **config 所在目录**（`app.rs` dev 分支同语义）——module-root 下 `dir` 是
/// 源码树根（src/），锚它会把行走截断在 src（工程根 node_modules 在其外），预生成
/// `web/__client/api.ts`（import `@oj-module/runtime/...`）内省即 fail-fast（2026-10-23 实测）。
pub fn discover_routes(
    dir: &Path,
    project_root: &Path,
    ts: bool,
    base: &str,
    index: Option<&ModuleIndex>,
) -> Result<serve::routes::RouteTable, String> {
    if ts {
        // dev：与 serve 装配同构（app.rs 的 dev 分支）——自建最小 Bridge + bridge_introspector。
        // v0.1.85：dev 侧的 ESM 别名（`#/m/x`）与 serve 装配同源——须把模块索引带进
        // loader，否则内省时按目录名猜模块，跨模块 import 直接失败（与 app.rs dev 分支一致）。
        let mi: Option<Arc<ModuleIndex>> = index.map(|i| Arc::new(i.clone()));
        let make = {
            // project_root 须 canonicalize：referrer 来自 file URL（已 realpath），词法
            // 前缀不一致会让 `module_root_of` 误判「未找到模块根」（macOS /tmp 实测）。
            let root = only_js::bridge::strip_verbatim(
                &project_root
                    .canonicalize()
                    .unwrap_or_else(|_| project_root.to_path_buf()),
            );
            let ts_load = ts;
            let mi = mi.clone();
            move || {
                Bridge::with_dbs_and_loader(
                    HashMap::new(),
                    Arc::new(InMemoryKV::new()),
                    SchemaRegistry::new(),
                    false,
                    Some(Arc::new(LoaderShared {
                        project_root: root.clone(),
                        ts: ts_load,
                    })),
                    Extras {
                        module_index: mi.clone(),
                        ..Default::default()
                    },
                )
            }
        };
        let (t, failures) = serve::routes::RouteTable::build(
            base,
            dir,
            ts,
            index,
            serve::routes::bridge_introspector(make),
        );
        if !failures.is_empty() {
            // 与 release 分支同语义：内省失败 fail-fast——静默继续会让 spec 缺路由，
            // `--check` 反而误报漂移。
            return Err(format!("dev routes: {}", failures.join("; ")));
        }
        return Ok(t);
    }
    // release：与 app.rs 的 release 分支同构，但收敛到本命令的纯发现职责。
    let lock = crate::manifest::load_lock(&dir.join("manifests.yaml")).map_err(|e| {
        format!(
            "release mode: {}: {e}",
            dir.join("manifests.yaml").display()
        )
    })?;
    if lock.is_empty() {
        return Err(format!(
            "release mode: {} missing or empty — run `oj build` first",
            dir.join("manifests.yaml").display()
        ));
    }
    let make = {
        // 同 dev 分支：canonicalize 对齐 referrer 的 realpath 形态。
        let root = only_js::bridge::strip_verbatim(
            &dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
        );
        let ts_load = ts;
        move || {
            Bridge::with_dbs_and_loader(
                HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(Arc::new(LoaderShared {
                    project_root: root.clone(),
                    ts: ts_load,
                })),
                Extras::default(),
            )
        }
    };
    let reader = serve::routes::bridge_default_reader(make);
    let mut entries = Vec::new();
    let b = base.trim_matches('/');
    for (module, version) in &lock {
        crate::manifest::validate_module(module).map_err(|e| format!("manifests.yaml: {e}"))?;
        crate::manifest::validate_version(version).map_err(|e| format!("manifests.yaml: {e}"))?;
        let mdir = dir.join(format!("{module}-{version}"));
        let mf = mdir.join("manifest.yaml");
        if !mf.is_file() {
            return Err(format!(
                "release mode: {} missing — run `oj build {module}`",
                mf.display()
            ));
        }
        let m = crate::manifest::parse_one(&mf)?;
        if m.name != *module {
            return Err(format!(
                "manifest name {:?} != module {module:?} (in {})",
                m.name,
                mf.display()
            ));
        }
        let rjs = mdir.join("routes.js");
        let v = reader(&rjs).map_err(|e| format!("load {}: {e}", rjs.display()))?;
        for e in serve::routes::entries_from_value(&v) {
            entries.push(serve::routes::RouteEntry {
                method: e.method,
                pattern: format!("/{b}/{}", e.pattern.trim_matches('/')),
                file: format!("{module}-{version}/{}", e.file),
                handler: e.handler,
                schema: e.schema,
            });
        }
    }
    let (table, failures) = serve::routes::RouteTable::from_entries(dir, &entries);
    if !failures.is_empty() {
        return Err(format!("release routes: {}", failures.join("; ")));
    }
    Ok(table)
}

/// 命令入口：生成或漂移校验。返回进程退出码（0 成功 / 1 漂移或错误）。
pub async fn run(a: &OpenApiArgs) -> Result<i32, String> {
    let loaded = crate::serve_cmd::load_app_config(
        a.config.as_deref(),
        a.site.as_deref(),
        a.dir.as_deref(),
        a.base.as_deref(),
        true,
        a.module_root.as_deref(),
    )?;
    let crate::serve_cmd::AppConfig {
        dir,
        ts,
        base,
        module_index,
        config_dir,
        ..
    } = loaded;

    let table = discover_routes(&dir, &config_dir, ts, &base, module_index.as_deref())?;
    let spec = generate(&table, &base)?;
    let explicit_out = a.out.is_some();
    let out_path: PathBuf = a
        .out
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("openapi.json"));
    // regen 提示须可原样复跑：module-root 模式给 `--module-root`（与 `-d` 互斥，给 `-d`
    // 会 clap 报错或静默退回平铺语义）。
    let source_arg = match &a.module_root {
        Some(mr) => format!("--module-root {mr}"),
        None => format!("-d {}", dir.display()),
    };
    let mut regen = match &a.config {
        Some(c) => format!("oj openapi -c {c} {source_arg}"),
        None => format!("oj openapi {source_arg}"),
    };
    if let Some(b) = &a.base {
        regen.push_str(&format!(" --base {b}"));
    }
    if explicit_out {
        regen.push_str(&format!(" -o {}", out_path.display()));
    }
    emit_or_check(&spec, &out_path, explicit_out, a.check, Some(&regen))
}

/// 生成物落盘（explicit_out=true 写文件 / false 打 stdout）或漂移校验。
/// 返回进程退出码（0 成功 / 1 漂移或错误）。`regen_hint`：漂移时打印的重生成命令
/// （None = 不打印，单测场景）。
fn emit_or_check(
    spec: &Value,
    out_path: &Path,
    explicit_out: bool,
    check: bool,
    regen_hint: Option<&str>,
) -> Result<i32, String> {
    if check {
        let committed = std::fs::read_to_string(out_path).map_err(|e| {
            format!(
                "oj openapi --check: cannot read {}: {e} (run `oj openapi` first to generate it)",
                out_path.display()
            )
        })?;
        let committed_val: Value = serde_json::from_str(&committed).map_err(|e| {
            format!(
                "oj openapi --check: {} is not valid JSON: {e}",
                out_path.display()
            )
        })?;
        let g = serde_json::to_string_pretty(&canonical(spec)).unwrap();
        let c = serde_json::to_string_pretty(&canonical(&committed_val)).unwrap();
        if g != c {
            eprintln!(
                "oj openapi --check: SPEC DRIFT DETECTED in {}",
                out_path.display()
            );
            if let Some(h) = regen_hint {
                eprintln!("  regenerate with: {h}");
            }
            for line in diff_lines(&g, &c).into_iter().take(60) {
                eprintln!("  {line}");
            }
            return Ok(1);
        }
        eprintln!("oj openapi --check: OK (no drift) — {}", out_path.display());
        Ok(0)
    } else {
        let text = serde_json::to_string_pretty(spec).map_err(|e| format!("serialize: {e}"))?;
        if explicit_out {
            std::fs::write(out_path, format!("{text}\n"))
                .map_err(|e| format!("write {}: {e}", out_path.display()))?;
            eprintln!("oj openapi: wrote {}", out_path.display());
        } else {
            println!("{text}");
        }
        Ok(0)
    }
}

/// 递归规范化：对象键升序，使键序无关的比较稳定。
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut m = serde_json::Map::new();
            for k in keys {
                m.insert(k.clone(), canonical(&map[k]));
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// 极简行级差（CI 排障用）：仅取前若干增/删行。
fn diff_lines(generated: &str, committed: &str) -> Vec<String> {
    let g: Vec<&str> = generated.lines().collect();
    let c: Vec<&str> = committed.lines().collect();
    let cset: std::collections::HashSet<&str> = c.iter().copied().collect();
    let gset: std::collections::HashSet<&str> = g.iter().copied().collect();
    let mut out = Vec::new();
    for l in &g {
        if !cset.contains(l) {
            out.push(format!("+ {l}"));
        }
    }
    for l in &c {
        if !gset.contains(l) {
            out.push(format!("- {l}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serve::routes::RouteTable;

    /// 每调用一个**独立**临时目录。用例并行跑时不能共用同一路径——构造函数的
    /// `remove_dir_all` 会把并行用例的 `create_dir_all` 打断（曾让
    /// `check_detects_no_drift_and_drift` 偶发 panic）。
    fn unique_tmp(prefix: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    /// 合成路由表（免 V8）：把若干 api 文件写盘，用假内省闭包喂声明。
    fn synthetic_table() -> (std::path::PathBuf, RouteTable) {
        let dir = unique_tmp("oj-oa-syn");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("user/account")).unwrap();
        std::fs::create_dir_all(dir.join("catch")).unwrap();
        std::fs::write(
            dir.join("user/account/api.ts"),
            "function get() {}\nexport default { get };\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("catch/api.ts"),
            "function get() {}\nexport default { get };\n",
        )
        .unwrap();
        let root = dir.clone();
        let rc = root.clone();
        let (t, _fail) = RouteTable::build("/v1/api", &root, true, None, move |f: &Path| {
            let rel = f
                .strip_prefix(&rc)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let decls = if rel == "user/account/api.ts" {
                vec![
                    ("get".to_string(), None, None),
                    ("post".to_string(), Some("/custom".to_string()), None),
                ]
            } else if rel == "catch/api.ts" {
                vec![("get".to_string(), Some("/catch/{*path}".to_string()), None)]
            } else {
                vec![]
            };
            Ok(decls)
        });
        (dir, t)
    }

    #[test]
    fn check_detects_no_drift_and_drift() {
        let (_d, table) = synthetic_table();
        let spec = generate(&table, "/v1/api").unwrap();

        // 无漂移：同一生成物比较 → 等价。
        let a = serde_json::to_string_pretty(&canonical(&spec)).unwrap();
        let b = serde_json::to_string_pretty(&canonical(&spec)).unwrap();
        assert_eq!(a, b, "同一生成物不应有漂移");

        // 有漂移：键序不同也算等价（规范化），但内容不同必须被检出。
        let mut mutated = spec.clone();
        mutated["paths"]["/v1/api/user/account"]["get"]["summary"] =
            serde_json::Value::String("TAMPERED".into());
        let c = serde_json::to_string_pretty(&canonical(&mutated)).unwrap();
        assert_ne!(
            a, c,
            "summary 被篡改必须被漂移校验检出（这正是 CI 门禁要抓的）"
        );
    }

    #[test]
    fn check_roundtrip_writes_then_detects_tamper() {
        // 真·`--check` 文件往返：generate → 落盘 → 同生成物校验应无漂移（0）；
        // 篡改 committed 文件后应检出漂移（1）。直接打 emit_or_check，绕开 config 解析。
        let (_d, table) = synthetic_table();
        let spec = generate(&table, "/v1/api").unwrap();
        let dir = unique_tmp("oj-oa-rt");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("openapi.json");

        // 落盘。
        let code = emit_or_check(&spec, &file, true, false, None).unwrap();
        assert_eq!(code, 0);
        assert!(file.is_file(), "生成物应已落盘");

        // 同生成物校验：无漂移。
        let code = emit_or_check(&spec, &file, true, true, None).unwrap();
        assert_eq!(code, 0, "同一生成物不应有漂移");

        // 篡改 committed 文件后校验：必须检出漂移。
        let mut committed: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        committed["info"]["title"] = serde_json::Value::String("TAMPERED".into());
        std::fs::write(&file, serde_json::to_string_pretty(&committed).unwrap()).unwrap();
        let code = emit_or_check(&spec, &file, true, true, None).unwrap();
        assert_eq!(
            code, 1,
            "篡改 committed openapi.json 必须被 --check 检出（CI 门禁要抓的）"
        );
    }

    #[test]
    fn dev_introspect_failure_fails_discovery() {
        // dev 分支内省失败必须 fail-fast（曾仅 eprintln 继续 → spec 静默缺路由，
        // `--check` 反而误报漂移）；语义对齐 release 分支的 failures → Err。
        let dir = unique_tmp("oj-oa-devfail");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("user/broken")).unwrap();
        std::fs::write(dir.join("user/broken/api.ts"), "this is not ?? valid ts").unwrap();
        let err = match discover_routes(&dir, &dir, true, "/v1/api", None) {
            Ok(_) => panic!("dev 内省失败须报错退出（fail-fast）"),
            Err(e) => e,
        };
        assert!(
            err.contains("dev routes:"),
            "dev 内省失败须报错退出（fail-fast）：{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_module_root_bare_import_resolves_from_project_root_node_modules() {
        // 回归（2026-10-23 框架演练实测）：module-root 下 loader 的 project_root 曾锚
        // dir=src，裸包 node_modules 行走被截断在 src → 模板预生成 `web/__client/api.ts`
        // （import `@oj-module/runtime/...`）内省 fail-fast，整条 `oj openapi --module-root`
        // 链不可用。修复后锚 config_dir（app.rs dev 分支同语义）：工程根 node_modules
        // 可达，内省通过，且零动词的 __client 文件不混入路由表。
        let proj = unique_tmp("oj-oa-mrroot");
        let _ = std::fs::remove_dir_all(&proj);
        std::fs::create_dir_all(proj.join("src/m1/api/ping")).unwrap();
        std::fs::create_dir_all(proj.join("src/m1/web/__client")).unwrap();
        std::fs::create_dir_all(proj.join("node_modules/@fake/pkg")).unwrap();
        std::fs::write(
            proj.join("node_modules/@fake/pkg/package.json"),
            r#"{"name":"@fake/pkg","version":"1.0.0","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            proj.join("node_modules/@fake/pkg/index.js"),
            "export const x = 1;\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("src/m1/api/manifest.yaml"),
            "name: m1\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("src/m1/api/ping/api.ts"),
            "export default { get() { json.ok({ok:true}); } };\n",
        )
        .unwrap();
        std::fs::write(
            proj.join("src/m1/web/__client/api.ts"),
            "import { x } from \"@fake/pkg\";\nexport const c = x;\n",
        )
        .unwrap();
        let src = proj.join("src").canonicalize().unwrap();
        let idx = only_js::bridge::resolve_roots(&src, true, None).unwrap();
        let table = discover_routes(&src, &proj, true, "/api", Some(&idx))
            .expect("project_root 锚工程根：__client 内省须通过");
        let mut patterns: Vec<String> = table.listing().iter().map(|r| r.pattern.clone()).collect();
        patterns.sort();
        assert_eq!(patterns, vec!["/api/m1/ping"]);
        let _ = std::fs::remove_dir_all(&proj);
    }
}
