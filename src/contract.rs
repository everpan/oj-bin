//! InputContract：handler 入参契约（v0.1.44，PR-6 4b）。
//!
//! handler 导出函数可挂 `.schema`（与 `.route` 同构），分通道描述入参与出参：
//! `params`（路径段）/ `query` / `body`（入参三通道，与 `RequestInfo` 同名字段对应），
//! 以及 v0.1.70 起的 **`response`**（出参声明）。
//! 同一份声明既作运行期校验（入参违反 → 400），也喂 `oj openapi` 出
//! `parameters` / `requestBody` / `responses`。
//!
//! ## `response` 通道为何不参与运行期校验
//! 响应由 JS 产生，校验失败时请求已执行完——无处可去，且会把「少返回一个字段」
//! 变成 500 级故障。故 `response` 只**声明与审计**（装配期关键字白名单 /
//! 类型字面值 / pattern 可编译，一律 fail-fast），是**文档与生成器的真源**
//! （`oj openapi` 的 `responses.200`、`oj gen client` 的响应类型），运行期不拦。
//!
//! ## 为何自研最小校验器而非引入 JSON Schema crate
//! 需要的是 **OpenAPI 方言**：路径/查询参数在 HTTP 里只有字符串形态，数值必须显式强转；
//! 通用 draft 引擎不会替我们做这件事，还得在外面再包一层。更重要的是安全主体是
//! 「**未知关键字一律注册期 fail-fast**」——这条白名单不论换不换引擎都得自己写。
//! 参见 `docs/superpowers/plans/2026-10-02-v0.1.44-request-schema-validation-design.md`。
//!
//! ## 安全红线
//! - 白名单外关键字（`$ref` / `oneOf` / `format` …）→ 构造期 Err，**绝不静默跳过**
//!   （静默放行 = 「声明了但没生效」，是本项目最痛的缺陷类）。
//! - `params` / `query` 只允许扁平原始类型：`RequestInfo` 里两者是 `HashMap<String, String>`，
//!   声明 array/object 是永远无法满足的**死契约**，构造期拒绝。
//! - `pattern` 用 Rust `regex`：**不等于 ECMA-262**；非法（含 lookahead）→ 构造期 Err。

use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;

/// 受支持关键字白名单（见模块文档红线）。
pub const SUPPORTED_KEYWORDS: &[&str] = &[
    "type",
    "required",
    "properties",
    "items",
    "additionalProperties",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "pattern",
    "enum",
    "nullable",
    "minItems",
    "maxItems",
    "default",
];

/// `.schema` 允许的通道（顶层键白名单）。
///
/// 顶层出现白名单外的键 → 装配期 fail-fast：入参三通道拼错（如 `param` / `bodys`）
/// 会让整份契约静默不生效，这正是本项目最重的缺陷类，宁可启动失败。
pub const SCHEMA_CHANNELS: &[&str] = &["params", "query", "body", "response"];

/// params / query 允许的类型（`HashMap<String, String>` 只能表达这些）。
const SCALAR_TYPES: &[&str] = &["string", "number", "integer", "boolean", "null"];
const ALL_TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];

/// `(api 文件绝对路径, js 方法名)` —— 与 `Lookup::Hit.file` / `run_module` 的入参
/// 以及 openapi.json 的 `x-oj-file` 同源，三者天然对齐。
type Key = (std::path::PathBuf, String);

/// 全量入参契约注册表（装配期冻结，运行期只读）。
///
/// 键里的路径与 `RouteTable` 内的 `files` 是**同一个** PathBuf（构造同源），
/// 故字符串相等即命中，不涉及 canonicalize / symlink 归一化。
#[derive(Debug, Default)]
pub struct InputContractRegistry {
    map: HashMap<Key, InputContract>,
    /// 预编译正则（键 = pattern 原文），避免每次请求重编译。
    patterns: HashMap<String, Regex>,
}

impl InputContractRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一处契约。非法契约 → Err（调用方须**硬失败**，不得退化为「忽略」）。
    pub fn insert(
        &mut self,
        file: std::path::PathBuf,
        method: &str,
        contract: InputContract,
    ) -> Result<(), String> {
        let key = (file, method.to_string());
        let mut pats = Vec::new();
        // 重放审计以收集 pattern（同一亦是`.schema` 的注册期 fail-fast 门）。
        let c = InputContract::try_new(
            contract.params().cloned(),
            contract.query().cloned(),
            contract.body().cloned(),
            &mut pats,
        )
        .map_err(|e| format!("{} {}: {e}", key.0.display(), method))?;
        for p in pats {
            self.patterns
                .entry(p.clone())
                .or_insert_with(|| Regex::new(&p).expect("pattern audited"));
        }
        self.map.insert(key, c);
        Ok(())
    }

    pub fn get(&self, file: &std::path::Path, method: &str) -> Option<&InputContract> {
        self.map.get(&(file.to_path_buf(), method.to_string()))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 按一次真实请求校验：`RequestInfo` → 契约。**在 JS 之前**调用。
    ///
    /// body 语义（核心）：仅当该路由**声明了 body 契约**才解析 body；
    /// - 空 body → None（由契约自身判 required/type，通常得 400「expected object」）；
    /// - 非空但**非法 JSON** → Err（`body: invalid JSON`）——不可解析就是不合规，
    ///   不得退化为「跳过校验」放行；
    /// - 未声明 body 契约 → **完全不碰 body**（既省一次解析，也不改变既有行为）。
    pub fn check_request(
        &self,
        file: &std::path::Path,
        method: &str,
        req: &crate::bridge::RequestInfo,
    ) -> Result<(), String> {
        let Some(c) = self.get(file, method) else {
            return Ok(());
        };
        let body = match c.body() {
            Some(_) if !req.body.is_empty() => Some(
                serde_json::from_slice(&req.body)
                    .map_err(|e| format!("body: invalid JSON: {e}"))?,
            ),
            _ => None,
        };
        c.check(&req.params, &req.query, body.as_ref(), &self.patterns)
    }

    /// 校验一次请求的入参；无契约登记的 (file, method) → Ok（不误拦未声明的路由）。
    pub fn check(
        &self,
        file: &std::path::Path,
        method: &str,
        params: &HashMap<String, String>,
        query: &HashMap<String, String>,
        body: Option<&Value>,
    ) -> Result<(), String> {
        match self.get(file, method) {
            Some(c) => c.check(params, query, body, &self.patterns),
            None => Ok(()),
        }
    }
}

/// 一处方法（一个导出函数）的入参契约。字段私有 —— 唯一构造途径是审计过的 `try_new`。
#[derive(Debug, Clone, Default)]
pub struct InputContract {
    params: Option<Value>,
    query: Option<Value>,
    body: Option<Value>,
}

impl InputContract {
    /// 构造：逐通道审计。**任一通道不合法 → Err**（违反者拿不到实例）。
    /// `pats` 收集需预编译的 pattern 原文（调用方 union 进全局正则表）。
    pub fn try_new(
        params: Option<Value>,
        query: Option<Value>,
        body: Option<Value>,
        pats: &mut Vec<String>,
    ) -> Result<Self, String> {
        audit_scalar_channel(&params, "params", pats)?;
        audit_scalar_channel(&query, "query", pats)?;
        audit_channel_opt(&body, "body", pats)?;
        Ok(Self {
            params,
            query,
            body,
        })
    }

    pub fn params(&self) -> Option<&Value> {
        self.params.as_ref()
    }
    pub fn query(&self) -> Option<&Value> {
        self.query.as_ref()
    }
    pub fn body(&self) -> Option<&Value> {
        self.body.as_ref()
    }
    pub fn is_empty(&self) -> bool {
        self.params.is_none() && self.query.is_none() && self.body.is_none()
    }

    /// 校验一次请求的入参。全部通过 Ok(())，任一通道违反 → Err(带字段路径的中文说明)。
    ///
    /// - `params` / `query`：来源是字符串 map，按声明类型**强转**（见 `coerce_scalars`）；
    /// - `body`：已是真 JSON（`None` = 无 body 或不可解析，交由调用方判定），**不做**字符串强转。
    pub fn check(
        &self,
        params: &HashMap<String, String>,
        query: &HashMap<String, String>,
        body: Option<&Value>,
        pats: &HashMap<String, Regex>,
    ) -> Result<(), String> {
        if let Some(s) = &self.params {
            let v = coerce_scalars(s, params, "params")?;
            check_value(s, &v, "params", pats)?;
        }
        if let Some(s) = &self.query {
            let v = coerce_scalars(s, query, "query")?;
            check_value(s, &v, "query", pats)?;
        }
        if let Some(s) = &self.body {
            let v = body.unwrap_or(&Value::Null);
            check_value(s, v, "body", pats)?;
        }
        Ok(())
    }
}

// ---- 构造期审计 ----

/// params / query 通道：顶层必须是 object，且**属性类型只能是可尺度标量**（禁 array/object）。
fn audit_scalar_channel(
    schema: &Option<Value>,
    chan: &str,
    pats: &mut Vec<String>,
) -> Result<(), String> {
    let Some(s) = schema else { return Ok(()) };
    let obj = s
        .as_object()
        .ok_or_else(|| format!(".schema.{chan}: must be an object"))?;
    if let Some(t) = obj.get("type") {
        let want = declared_types(t).map_err(|e| format!(".schema.{chan}: {e}"))?;
        if want.len() != 1 || want[0] != "object" {
            return Err(format!(
                ".schema.{chan}: type must be \"object\", got {want:?}"
            ));
        }
    }
    if let Some(props) = obj.get("properties") {
        let Some(map) = props.as_object() else {
            return Err(format!(".schema.{chan}.properties: must be an object"));
        };
        for (name, sub) in map {
            let p = format!(".schema.{chan}.properties.{name}");
            let types = sub
                .as_object()
                .and_then(|o| o.get("type"))
                .map(declared_types)
                .transpose()
                .map_err(|e| format!("{p}: {e}"))?
                .unwrap_or_default();
            for t in types {
                if !SCALAR_TYPES.contains(&t.as_str()) {
                    return Err(format!(
                        "{p}: type \"{t}\" is unusable in `{chan}` — {chan} values arrive as \
                         HashMap<String,String>, so only scalar types can ever be satisfied \
                         (declaring array/object here creates a contract that can never validate)"
                    ));
                }
            }
            audit_keywords(sub, &p, pats)?;
            // 标量通道不得嵌套：再出现 properties/items 即拒绝。
            if sub.get("properties").is_some() || sub.get("items").is_some() {
                return Err(format!(
                    "{p}: nested properties/items are not allowed in `{chan}` (flat scalars only)"
                ));
            }
        }
    }
    audit_keywords(s, &format!(".schema.{chan}"), pats)
}

fn audit_channel_opt(
    schema: &Option<Value>,
    chan: &str,
    pats: &mut Vec<String>,
) -> Result<(), String> {
    let Some(s) = schema else { return Ok(()) };
    audit_keywords(s, &format!(".schema.{chan}"), pats)
}

/// 一处 handler 的 `.schema` **全量审计**（v0.1.70 唯一入口）。
///
/// 三条路径——serve 装配（`oj/src/app.rs`）/ `oj build` 落盘 `routes.js` /
/// `oj openapi` 生成——**必须同口径**：只在装配期审计会让非法声明先落进产物，
/// 而 openapi.json 现在是下游 `__client` 生成的唯一输入，等 serve 才红就太晚了。
///
/// 审计项：①顶层通道白名单 ②`params`/`query` 仅扁平标量 ③各通道关键字白名单
/// ④类型字面值 ⑤`pattern` 可编译（Rust regex）。**任一不合法 → Err。**
///
/// 与 `server.schema_validation` 开关**无关**：开关管「是否执行校验」，不管
/// 「声明是否合法」——逃生门是给「规则误杀合法流量」准备的，不是给代码写错准备的。
/// 收集到的 pattern 在此丢弃（可编译性已在审计内验证）；需要预编译的调用方自行
/// 走 `InputContract::try_new` / `InputContractRegistry::insert`。
pub fn audit_schema(schema: &Value) -> Result<(), String> {
    let mut pats = Vec::new();
    audit_channels(schema)?;
    audit_scalar_channel(&schema.get("params").cloned(), "params", &mut pats)?;
    audit_scalar_channel(&schema.get("query").cloned(), "query", &mut pats)?;
    audit_channel_opt(&schema.get("body").cloned(), "body", &mut pats)?;
    if let Some(r) = schema.get("response") {
        audit_response(r)?;
    }
    Ok(())
}

/// `.schema` 顶层通道白名单审计（v0.1.70）。
///
/// 拼错通道名（`param` / `bodys` / `responce` …）会让整份契约**静默不生效**——
/// 运行期守卫只认 `params`/`query`/`body`，拼错的那个等于裸奔。故 fail-fast。
fn audit_channels(schema: &Value) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Err(".schema: must be an object".to_string());
    };
    for k in obj.keys() {
        if !SCHEMA_CHANNELS.contains(&k.as_str()) {
            return Err(format!(
                ".schema: unsupported channel `{k}` — allowed channels are {}; \
                 an unknown channel would be silently ignored at runtime, so it FAILS FAST \
                 here instead (a declaration that does nothing is a lie)",
                SCHEMA_CHANNELS.join(", ")
            ));
        }
    }
    Ok(())
}

/// `response` 通道审计（v0.1.70）：关键字白名单 + 类型字面值 + pattern 可编译，
/// 一律 fail-fast。**不套** params/query 的「仅扁平标量」限制——响应是 JSON 值，
/// object / array / 嵌套都是合法形态（与 `body` 同语义）。
///
/// 运行期**不**校验响应（见模块文档），这里只保证声明本身合法，好让它安全地成为
/// `oj openapi` 的 `responses` 与下游 `__client` 响应类型的真源。
fn audit_response(schema: &Value) -> Result<(), String> {
    audit_keywords(schema, ".schema.response", &mut Vec::new())
}

/// 递归审计：白名单关键字 + 类型字面值合法 + pattern 可编译。
fn audit_keywords(v: &Value, path: &str, pats: &mut Vec<String>) -> Result<(), String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("{path}: schema must be an object"))?;
    for k in obj.keys() {
        if !SUPPORTED_KEYWORDS.contains(&k.as_str()) {
            return Err(format!(
                "{path}: unsupported keyword `{k}` — supported subset is {}; \
                 unsupported keywords FAIL FAST instead of being silently skipped \
                 (silent skip would make your declaration a lie)",
                SUPPORTED_KEYWORDS.join(", ")
            ));
        }
    }
    if let Some(t) = obj.get("type") {
        declared_types(t).map_err(|e| format!("{path}: {e}"))?;
    }
    if let Some(p) = obj.get("pattern") {
        let Some(s) = p.as_str() else {
            return Err(format!("{path}: pattern must be a string"));
        };
        Regex::new(s).map_err(|e| {
            format!(
                "{path}: pattern {s:?} is not a valid Rust regex: {e} \
                 (Rust regex != ECMA-262; lookahead/backreference are unsupported)"
            )
        })?;
        pats.push(s.to_string());
    }
    if let Some(e) = obj.get("enum")
        && (!e.is_array() || e.as_array().unwrap().is_empty())
    {
        return Err(format!("{path}: enum must be a non-empty array"));
    }
    for key in ["required", "minLength", "maxLength", "minItems", "maxItems"] {
        // required 已白名单内，此处只做形态校验（数字类不许是字符串）。
        if key == "required" {
            if let Some(r) = obj.get("required") {
                let Some(arr) = r.as_array() else {
                    return Err(format!("{path}: required must be an array of strings"));
                };
                if arr.iter().any(|x| !x.is_string()) {
                    return Err(format!("{path}: required entries must be strings"));
                }
            }
        } else if let Some(n) = obj.get(key)
            && !n.is_number()
        {
            return Err(format!("{path}: {key} must be a number"));
        }
    }
    for key in ["minimum", "maximum"] {
        if let Some(n) = obj.get(key)
            && !n.is_number()
        {
            return Err(format!("{path}: {key} must be a number"));
        }
    }
    if let Some(props) = obj.get("properties") {
        let Some(map) = props.as_object() else {
            return Err(format!("{path}: properties must be an object"));
        };
        for (name, sub) in map {
            // 原型链污染防线：openapi.json 是下游生成 TS/zod 的输入，属性名会直接
            // 变成生成源码里的键。这三个键生成的字面量对象会改到原型上
            // （与 `legal_handler_chain` 对 handler 链段的口径一致）。
            for bad in ["__proto__", "constructor", "prototype"] {
                if name == bad {
                    return Err(format!(
                        "{path}.properties.{name}: `{bad}` is not allowed as a property name \
                         (prototype-pollution guard: these names become keys in generated client code)"
                    ));
                }
            }
            audit_keywords(sub, &format!("{path}.properties.{name}"), pats)?;
        }
    }
    if let Some(items) = obj.get("items") {
        audit_keywords(items, &format!("{path}.items"), pats)?;
    }
    Ok(())
}

/// `type` 可为单个字符串或类型数组；返回值均为规范化的小写类型名。
fn declared_types(t: &Value) -> Result<Vec<String>, String> {
    let norm = |s: &str| {
        if ALL_TYPES.contains(&s) {
            Ok(s.to_string())
        } else {
            Err(format!(
                "unknown type {s:?} (allowed: {})",
                ALL_TYPES.join(", ")
            ))
        }
    };
    match t {
        Value::String(s) => norm(s).map(|v| vec![v]),
        Value::Array(a) => {
            if a.is_empty() {
                return Err("type array must not be empty".into());
            }
            a.iter()
                .map(|x| {
                    x.as_str()
                        .ok_or_else(|| "type array entries must be strings".to_string())
                        .and_then(norm)
                })
                .collect()
        }
        _ => Err("type must be a string or an array of strings".into()),
    }
}

// ---- 运行期校验 ----

/// 把字符串来源的 map 按声明类型强转为 Value::Object（params / query 专用）。
fn coerce_scalars(
    schema: &Value,
    src: &HashMap<String, String>,
    chan: &str,
) -> Result<Value, String> {
    let mut out = serde_json::Map::new();
    let props = schema.get("properties").and_then(|p| p.as_object());
    for (k, raw) in src {
        let target = props
            .and_then(|p| p.get(k))
            .and_then(|s| s.get("type"))
            .map(declared_types)
            .transpose()
            .map_err(|e| format!("{chan}.{k}: {e}"))?
            .unwrap_or_else(|| vec!["string".to_string()]);
        if let Some(v) = coerce_one(raw, &target) {
            out.insert(k.clone(), v);
        } else if target.iter().any(|t| t == "null") {
            out.insert(k.clone(), Value::Null);
        } else {
            return Err(format!(
                "{chan}.{k}: expected {}, cannot parse {raw:?}",
                target.join("|")
            ));
        }
    }
    Ok(Value::Object(out))
}

/// 字符串 → 目标类型的显式强转。无法转换 → None（由调用方报违反）。
fn coerce_one(raw: &str, target: &[String]) -> Option<Value> {
    // 多类型取首个能成功的（number 优先于 string 以便 "12" 转数字）。
    for t in target {
        let v = match t.as_str() {
            "integer" => raw.parse::<i64>().ok().map(Value::from),
            "number" => raw.parse::<f64>().ok().map(Value::from),
            "boolean" => match raw {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            "null" => (raw == "null").then_some(Value::Null),
            "string" => Some(Value::String(raw.to_string())),
            _ => None,
        };
        if let Some(v) = v {
            return Some(v);
        }
    }
    None
}

fn matches_type(v: &Value, ty: &str) -> bool {
    match ty {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        _ => false,
    }
}

fn check_value(
    schema: &Value,
    value: &Value,
    path: &str,
    pats: &HashMap<String, Regex>,
) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };
    let nullable = obj
        .get("nullable")
        .and_then(|n| n.as_bool())
        .unwrap_or(false);
    if nullable && value.is_null() {
        return Ok(());
    }
    if let Some(t) = obj.get("type") {
        let types = declared_types(t).map_err(|e| format!("{path}: {e}"))?;
        if !types.iter().any(|ty| matches_type(value, ty)) {
            return Err(format!(
                "{path}: expected {}, got {}",
                types.join("|"),
                kind_of(value)
            ));
        }
    }
    if let Some(e) = obj.get("enum") {
        let Some(arr) = e.as_array() else {
            return Ok(());
        };
        if !enum_contains(arr, value) {
            return Err(format!(
                "{path}: must be one of {}, got {}",
                serde_json::to_string(arr).unwrap_or_default(),
                value
            ));
        }
    }
    match value {
        Value::Object(m) => {
            if let Some(req) = obj.get("required").and_then(|r| r.as_array()) {
                for r in req {
                    let Some(name) = r.as_str() else { continue };
                    if !m.contains_key(name) {
                        return Err(format!("{path}: missing required field `{name}`"));
                    }
                }
            }
            let props = obj.get("properties").and_then(|p| p.as_object());
            if let Some(props) = props {
                for (k, v) in m {
                    if let Some(sub) = props.get(k) {
                        check_value(sub, v, &format!("{path}.{k}"), pats)?;
                    } else if obj.get("additionalProperties") == Some(&Value::Bool(false)) {
                        return Err(format!("{path}: additional property `{k}` is not allowed"));
                    }
                }
            } else if obj.get("additionalProperties") == Some(&Value::Bool(false)) && !m.is_empty()
            {
                return Err(format!(
                    "{path}: additional properties are not allowed (got {})",
                    m.len()
                ));
            }
        }
        Value::Array(a) => {
            if let Some(items) = obj.get("items") {
                for (i, v) in a.iter().enumerate() {
                    check_value(items, v, &format!("{path}[{i}]"), pats)?;
                }
            }
            if let Some(n) = obj.get("minItems").and_then(|n| n.as_u64())
                && (a.len() as u64) < n
            {
                return Err(format!(
                    "{path}: expected at least {n} items, got {}",
                    a.len()
                ));
            }
            if let Some(n) = obj.get("maxItems").and_then(|n| n.as_u64())
                && (a.len() as u64) > n
            {
                return Err(format!(
                    "{path}: expected at most {n} items, got {}",
                    a.len()
                ));
            }
        }
        Value::String(s) => {
            if let Some(n) = obj.get("minLength").and_then(|n| n.as_u64())
                && (s.chars().count() as u64) < n
            {
                return Err(format!("{path}: shorter than minLength {n}"));
            }
            if let Some(n) = obj.get("maxLength").and_then(|n| n.as_u64())
                && (s.chars().count() as u64) > n
            {
                return Err(format!("{path}: longer than maxLength {n}"));
            }
            if let Some(Value::String(p)) = obj.get("pattern") {
                let Some(re) = pats.get(p) else {
                    return Err(format!(
                        "{path}: pattern {p:?} was not precompiled (internal)"
                    ));
                };
                if !re.is_match(s) {
                    return Err(format!("{path}: does not match pattern {p:?}"));
                }
            }
        }
        Value::Number(_) => {
            if let Some(n) = obj.get("minimum").and_then(|n| n.as_f64())
                && value.as_f64().unwrap_or(f64::MAX) < n
            {
                return Err(format!("{path}: must be >= {n}"));
            }
            if let Some(n) = obj.get("maximum").and_then(|n| n.as_f64())
                && value.as_f64().unwrap_or(f64::MIN) > n
            {
                return Err(format!("{path}: must be <= {n}"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 枚举成员判定：数字按**数值**比较，而非 serde_json 的严格变体相等。
///
/// 根因：query/param 经强转得到的数值，`number` 声明会变成 `Float(1.0)`，
/// 而契约里 `enum:[1,2]` 的字面量被解析为 `PosInt(1)`；serde_json 的
/// `Float(1.0) != PosInt(1)`，于是合法值 `1` 被 `must be one of` 误杀
/// （`got 1.0`）。按语义数值比较即可让 `1` / `1.0` 正确命中成员。
fn enum_contains(arr: &[Value], value: &Value) -> bool {
    arr.iter().any(|x| values_eq(x, value))
}

fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => numeric_eq(x, y),
        _ => a == b,
    }
}

fn numeric_eq(a: &serde_json::Number, b: &serde_json::Number) -> bool {
    // 两边都是整数时优先精确比较（as_i64 对非整数返回 None）。
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return x == y;
    }
    if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64()) {
        return x == y;
    }
    // 任一为浮点：按 f64 比较，使 1 == 1.0 成立，而 1.5 仍不命中 1。
    a.as_f64() == b.as_f64()
}

#[cfg(test)]
#[path = "contract_registry_tests.rs"]
mod registry_tests;

#[cfg(test)]
#[path = "contract_tests.rs"]
mod tests;
