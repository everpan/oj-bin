//! `oj openapi`：从路由表生成 OpenAPI 3.1，并支持 `--check` 漂移校验（PR-6 第一步）。
//!
//! 设计：
//! - 路由发现双模复用既有路径——dev 走 `serve::build_table`（V8 内省 .route），
//!   release 走 `dist/manifests.yaml` + 各模块 `routes.js`（与 serve 装配同构，见
//!   `app.rs` 的 release 分支）。
//! - 当前仅能收集体量信息（路径 / 方法 / 源文件 / 派生 module），请求体 / 响应 schema /
//!   入参校验留待 PR-6 后续步；故生成物含 `responses` 占位与 `x-oj-file` 溯源标记。
//! - 漂移校验对生成物与已提交 `openapi.json` 做**键序规范化**比较，不一致即非零退出
//!   （CI 门禁），并打印重生成命令。

use only_js::bridge::{Bridge, Extras, InMemoryKV, LoaderShared, SchemaRegistry};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::args::OpenApiArgs;

/// 双模路由发现：dev(ts=true) 走 V8 内省；release(ts=false) 走 dist 直载。
pub fn discover_routes(
    dir: &Path,
    ts: bool,
    base: &str,
) -> Result<serve::routes::RouteTable, String> {
    if ts {
        // dev：与 serve 装配同构（app.rs 的 dev 分支）——自建最小 Bridge + bridge_introspector。
        let make = {
            let root = dir.to_path_buf();
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
        let (t, failures) = serve::routes::RouteTable::build(
            base,
            dir,
            ts,
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
        let root = dir.to_path_buf();
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

/// 路由表 → OpenAPI 3.1（serde_json::Value；手工构造，避免引入额外 crate）。
///
/// v0.1.44：handler 的 `.schema`（经 `RouteRow.schema` 落到此处）补出
/// `parameters`（path / query）与 `requestBody`。返回 Result —— 契约与路由
/// pattern 不一致时**生成期 fail-fast**（见 `contract_params` 的交叉校验）。
pub fn generate(table: &serve::routes::RouteTable, base: &str) -> Result<Value, String> {
    let b = base.trim_matches('/');
    let mut paths: serde_json::Map<String, Value> = serde_json::Map::new();
    for row in table.listing() {
        // row.pattern 形如 "/v1/api/user/{id}"（含 base 与前导 /）。
        let raw = row.pattern.trim_start_matches('/');
        let oa_path = to_oa_path(raw);
        let method = row.method.to_lowercase();
        // 溯源标记统一用正斜杠（Windows 原生分隔符是反斜杠，会让跨平台比对/展示不一致）。
        let file = table
            .file_path(row.file)
            .to_string_lossy()
            .replace('\\', "/");
        let sub = raw
            .strip_prefix(&format!("{b}/"))
            .unwrap_or(raw)
            .to_string();
        let operation_id = operation_id(&sub, &method);
        let summary = format!("{} {}", method.to_uppercase(), raw);

        let mut op = serde_json::Map::new();
        op.insert("operationId".to_string(), Value::String(operation_id));
        op.insert("summary".to_string(), Value::String(summary));
        op.insert("x-oj-file".to_string(), Value::String(file));

        let sc = row.schema.clone();
        let mut parameters: Vec<Value> = Vec::new();
        // 声明了 params 契约 → 契约为真源；否则回落「由 pattern 推导」（type: string）。
        match sc.as_ref().and_then(|s| s.get("params")) {
            Some(p) => parameters.extend(contract_params(
                p,
                "path",
                &pattern_param_names(&oa_path),
                &sub,
            )?),
            None => parameters.extend(path_params(&oa_path)),
        }
        if let Some(q) = sc.as_ref().and_then(|s| s.get("query")) {
            parameters.extend(contract_params(q, "query", &[], &sub)?);
        }
        if !parameters.is_empty() {
            op.insert("parameters".to_string(), Value::Array(parameters));
        }
        if let Some(body) = sc.as_ref().and_then(|s| s.get("body")) {
            op.insert(
                "requestBody".to_string(),
                json!({
                    "required": true,
                    "content": { "application/json": { "schema": body.clone() } },
                }),
            );
        }
        // 占位响应：响应 schema 契约留待后续步，此处仅保 OpenAPI 3.1 基本结构。
        op.insert(
            "responses".to_string(),
            json!({ "200": { "description": "OK" } }),
        );

        let path_obj = paths.entry(oa_path.clone()).or_insert_with(|| json!({}));
        path_obj
            .as_object_mut()
            .unwrap()
            .insert(method, Value::Object(op));
    }
    Ok(json!({
        "openapi": "3.1.0",
        "info": { "title": "oj API", "version": "0.1.0" },
        "paths": Value::Object(paths),
    }))
}

/// 命令入口：生成或漂移校验。返回进程退出码（0 成功 / 1 漂移或错误）。
pub async fn run(a: &OpenApiArgs) -> Result<i32, String> {
    let (_cfg, _top, _config_dir, dir, ts, base) = crate::serve_cmd::load_app_config(
        a.config.as_deref(),
        a.site.as_deref(),
        a.dir.as_deref(),
        a.base.as_deref(),
    )
    .map_err(|e| format!("oj openapi: {e}"))?;

    let table = discover_routes(&dir, ts, &base)?;
    let spec = generate(&table, &base)?;
    let explicit_out = a.out.is_some();
    let out_path: PathBuf = a
        .out
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("openapi.json"));
    let mut regen = match &a.config {
        Some(c) => format!("oj openapi -c {c} -d {}", dir.display()),
        None => format!("oj openapi -d {}", dir.display()),
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

// ---- 路径 / 标识符辅助 ----

/// oj pattern → OpenAPI path：catch-all `{*x}` 收敛为 `{x}`（matchit/glob 写法非 OpenAPI 合法）。
fn to_oa_path(raw: &str) -> String {
    let seg: Vec<String> = raw.split('/').map(|s| s.replacen("{*", "{", 1)).collect();
    format!("/{}", seg.join("/"))
}

/// 由契约生成 OpenAPI `parameters`（location = "path" | "query"）。
///
/// **路径参数交叉校验**（安全相关）：契约声明的名字必须与路由 pattern 的真实参数
/// **双向一致** —— 声明了 pattern 没有的、或 pattern 有却没声明的，都 fail-fast。
/// 否则「契约」与「URL」会变成两份真源，而运行期校验只认前者，漏掉的参数等于
/// 不受任何约束。
fn contract_params(
    schema: &Value,
    location: &str,
    pattern_names: &[String],
    sub: &str,
) -> Result<Vec<Value>, String> {
    let props = schema.get("properties").and_then(|p| p.as_object());
    let names: Vec<String> = match props {
        Some(m) => m.keys().cloned().collect(),
        None => Vec::new(),
    };
    let required: Vec<String> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if location == "path" {
        for n in &names {
            if !pattern_names.contains(n) {
                return Err(format!(
                    "`{sub}`: .schema.params declares `{n}`, but it is not a path param of the route pattern — contract and route must agree (two sources of truth otherwise)"
                ));
            }
        }
        for n in pattern_names {
            if !names.contains(n) {
                return Err(format!(
                    "`{sub}`: route pattern has path param `{n}` but .schema.params does not declare it — it would pass through unconstrained"
                ));
            }
        }
    }

    let mut out = Vec::new();
    for n in names {
        let mut sc = props
            .and_then(|m| m.get(&n))
            .cloned()
            .unwrap_or_else(|| json!({ "type": "string" }));
        // OpenAPI 的 `required` 是 parameter 级字段，schema 级出现即冗余。
        if let Some(o) = sc.as_object_mut() {
            o.remove("required");
        }
        out.push(json!({
            "name": n,
            "in": location,
            // 路径参数恒必填（URL 段不存在即不匹配）。
            "required": location == "path" || required.contains(&n),
            "schema": sc,
        }));
    }
    Ok(out)
}

/// 路由 pattern 里的路径参数名（OpenAPI 形态 `{name}`）。
fn pattern_param_names(oa_path: &str) -> Vec<String> {
    oa_path
        .split('/')
        .filter_map(|seg| {
            seg.strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .map(|s| s.to_string())
        })
        .collect()
}

/// 抽取路径参数（OpenAPI `in: path` 必填）。
fn path_params(oa_path: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for seg in oa_path.split('/') {
        if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            out.push(json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" },
            }));
        }
    }
    out
}

/// 生成稳定且合法的 operationId（`^[a-zA-Z0-9._~-]+$`）：非允许字符统一转 `_`。
fn operation_id(sub: &str, method: &str) -> String {
    let mut s: String = sub
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    s.push('_');
    s.push_str(method);
    s
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
    use serde_json::json;
    use serve::routes::RouteTable;
    use std::path::PathBuf;

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
        let (t, _fail) = RouteTable::build("/v1/api", &root, true, move |f: &Path| {
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
    fn generate_produces_valid_openapi_3_1_with_params_and_catchall() {
        let (_d, table) = synthetic_table();
        let spec = generate(&table, "/v1/api").unwrap();
        assert_eq!(spec["openapi"], "3.1.0");
        let paths = spec["paths"].as_object().expect("paths object");

        // dev 派生：/v1/api/user/account（get）+ /v1/api/custom（post，因 .route="/custom" 视为 base 根）
        let acct = &paths["/v1/api/user/account"];
        assert_eq!(acct["get"]["operationId"], "user_account_get");
        assert_eq!(acct["get"]["summary"], "GET v1/api/user/account");
        assert!(
            acct["get"].get("parameters").is_none(),
            "无路径参数不应有 parameters"
        );
        let custom = &paths["/v1/api/custom"];
        assert_eq!(custom["post"]["operationId"], "custom_post");
        assert_eq!(custom["post"]["summary"], "POST v1/api/custom");

        // catch-all：{*path} 必须收敛为 {path}，且抽出路径参数。
        let catch = &paths["/v1/api/catch/{path}"];
        assert_eq!(catch["get"]["operationId"], "catch___path__get");
        let params = catch["get"]["parameters"]
            .as_array()
            .expect("catch has params");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0]["name"], "path");
        assert_eq!(params[0]["in"], "path");
        assert_eq!(params[0]["required"], true);

        // 溯源标记 + 占位响应。
        assert!(
            acct["get"]["x-oj-file"]
                .as_str()
                .unwrap()
                .contains("user/account/api.ts")
        );
        assert!(
            acct["get"]["responses"]["200"]["description"]
                .as_str()
                .is_some()
        );
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

    /// 防回归：并行用例各自建表时**必须拿到不同临时目录**——曾共用 `oj-oa-syn-<pid>`，
    /// 并行跑时构造函数的 `remove_dir_all` 会打断另一用例的 `create_dir_all`（偶发 panic）。
    #[test]
    fn synthetic_tables_use_distinct_temp_dirs() {
        let (a, _) = synthetic_table();
        let (b, _) = synthetic_table();
        assert_ne!(a, b, "两次建表不能共用临时目录（并行会互踩）");
        assert!(a.join("user/account/api.ts").is_file());
        assert!(b.join("user/account/api.ts").is_file());
    }

    /// 由带契约的 RouteEntry 建表：契约经 `from_entries` 落进 RouteRow.schema。
    fn table_with_contract(
        method: &str,
        pattern: &str,
        file: &str,
        schema: Value,
    ) -> (PathBuf, RouteTable) {
        let dir = unique_tmp("oj-oa-ct");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entries = vec![serve::routes::RouteEntry {
            method: method.to_string(),
            pattern: pattern.to_string(),
            file: file.to_string(),
            schema: Some(schema),
        }];
        let (t, fail) = RouteTable::from_entries(&dir, &entries);
        assert!(fail.is_empty(), "{fail:?}");
        (dir, t)
    }

    #[test]
    fn contract_emits_parameters_and_request_body() {
        let (_d, t) = table_with_contract(
            "post",
            "/v1/api/user/{id}",
            "user-0.1.0/api.js",
            json!({
                "params": {
                    "type": "object",
                    "properties": { "id": { "type": "integer", "minimum": 1 } }
                },
                "query": {
                    "type": "object",
                    "required": ["page"],
                    "properties": { "page": { "type": "integer" }, "kw": { "type": "string" } }
                },
                "body": {
                    "type": "object",
                    "required": ["name"],
                    "properties": { "name": { "type": "string" } }
                }
            }),
        );
        let spec = generate(&t, "/v1/api").unwrap();
        let op = &spec["paths"]["/v1/api/user/{id}"]["post"];

        // params → in:path，且**类型来自契约**（不再是硬编码 string）
        let ps = op["parameters"].as_array().unwrap();
        let path_p: Vec<_> = ps.iter().filter(|p| p["in"] == "path").collect();
        assert_eq!(path_p.len(), 1);
        assert_eq!(path_p[0]["name"], "id");
        assert_eq!(path_p[0]["required"], true);
        assert_eq!(path_p[0]["schema"]["type"], "integer");
        assert_eq!(path_p[0]["schema"]["minimum"], 1);

        // query → in:query，required 只标声明的
        let q: Vec<_> = ps.iter().filter(|p| p["in"] == "query").collect();
        assert_eq!(q.len(), 2);
        let page = q.iter().find(|p| p["name"] == "page").unwrap();
        assert_eq!(page["required"], true);
        let kw = q.iter().find(|p| p["name"] == "kw").unwrap();
        assert_eq!(kw["required"], false);

        // body → requestBody
        assert_eq!(
            op["requestBody"]["content"]["application/json"]["schema"]["required"][0],
            "name"
        );
    }

    #[test]
    fn contract_params_must_agree_with_route_pattern() {
        // 击穿：契约与 pattern 不一致必须 fail-fast（否则两份真源，未声明者不受约束）。
        let (_d, t) = table_with_contract(
            "get",
            "/v1/api/user/{id}",
            "user-0.1.0/api.js",
            json!({ "params": { "type": "object", "properties": { "id": {"type":"integer"}, "ghost": {"type":"string"} } } }),
        );
        let err = generate(&t, "/v1/api").unwrap_err();
        assert!(
            err.contains("ghost"),
            "契约声明了 pattern 没有的参数必须报错: {err}"
        );

        let (_d2, t2) = table_with_contract(
            "get",
            "/v1/api/user/{id}",
            "user-0.1.0/api.js",
            json!({ "params": { "type": "object", "properties": {} } }),
        );
        let err2 = generate(&t2, "/v1/api").unwrap_err();
        assert!(
            err2.contains("id"),
            "pattern 有参数而契约未声明必须报错: {err2}"
        );
    }

    #[test]
    fn without_contract_path_params_fall_back_to_pattern() {
        // 未声明契约：保持 v0.1.43 行为（pattern 推导 + type: string）。
        let dir = unique_tmp("oj-oa-nb");
        let (t, fail) = RouteTable::from_entries(
            &dir,
            &[serve::routes::RouteEntry {
                method: "get".to_string(),
                pattern: "/v1/api/user/{id}".to_string(),
                file: "user-0.1.0/api.js".to_string(),
                schema: None,
            }],
        );
        assert!(fail.is_empty());
        let spec = generate(&t, "/v1/api").unwrap();
        let op = &spec["paths"]["/v1/api/user/{id}"]["get"];
        assert_eq!(op["parameters"][0]["schema"]["type"], "string");
        assert!(
            op.get("requestBody").is_none(),
            "无 body 契约不得生成 requestBody"
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
        let err = match discover_routes(&dir, true, "/v1/api") {
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
    fn release_entries_prefix_base_and_build_table() {
        // 直接喂 from_entries，验证 release 形态 pattern 含 base 前缀（与 app.rs 同构）。
        let entries = vec![
            serve::routes::RouteEntry {
                schema: None,
                method: "get".to_string(),
                pattern: "/v1/api/user/{id}".to_string(),
                file: "user-0.1.0/_id_/api.js".to_string(),
            },
            serve::routes::RouteEntry {
                method: "post".to_string(),
                pattern: "/v1/api/admin/role".to_string(),
                file: "admin-0.1.0/role/api.js".to_string(),
                schema: None,
            },
        ];
        let dir = unique_tmp("oj-oa-rel");
        let (t, fail) = RouteTable::from_entries(&dir, &entries);
        assert!(fail.is_empty(), "release entries 不应有 failures: {fail:?}");
        let spec = generate(&t, "/v1/api").unwrap();
        let paths = spec["paths"].as_object().unwrap();
        assert!(
            paths.contains_key("/v1/api/user/{id}"),
            "release pattern 必须含 base 前缀"
        );
        assert!(paths.contains_key("/v1/api/admin/role"));
        assert_eq!(
            paths["/v1/api/user/{id}"]["get"]["operationId"],
            "user__id__get"
        );
        assert_eq!(
            paths["/v1/api/admin/role"]["post"]["operationId"],
            "admin_role_post"
        );
    }
}
