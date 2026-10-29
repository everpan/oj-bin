//! `POST {base}/mcp`——MCP streamable HTTP（stateless 子集，spec 2026-10-30
//! R3/R5/R6/R9）。协议面约 5 个方法手写实现（取舍 1：不加 rmcp）。
//!
//! 合规要点（R5，经 2025-06-18 spec 逐条核验）：
//! - stateless：不签发 Mcp-Session-Id（缺席即声明）；GET/DELETE 由 axum `post()`
//!   天然 405；响应一律 `application/json`（client 给了两种 Accept，选 JSON 合规）。
//! - 通知（任何无 `id` 的消息）→ 202 空 body，不回 JSON-RPC。
//! - batch 数组 → -32600（MCP 2025-06-18 已移除 batch）；解析失败 → -32700（id:null）。
//! - `initialize` 必回 `protocolVersion`（协商）+ `capabilities:{tools:{}}` +
//!   `serverInfo`——无 capabilities 声明严格客户端不发 tools/list。
//!
//! 安全（R9）：endpoint 级 admit（字面 `/mcp`）先于一切——匿名目录不可用；
//! Content-Type/Accept/body 上限三重门；tools/call 经 `run_capture`（admit 收 tool
//! 自身 path_no_base，per-route anonymous_paths 粒度不摊平）。

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use crate::mcp::{McpTool, spec_to_tools};
use crate::routes::{self, Lookup, RouteTable};
use crate::{AppState, RuntimeMount, admit, run_capture};
use only_js::bridge::Capture;

/// 支持的 protocolVersion 集（协商 = client 版本在集内则回显，否则回最新）。
const SUPPORTED_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const DEFAULT_PROTOCOL: &str = "2025-06-18";

/// encodeURIComponent 等价集：NON_ALPHANUMERIC 减回 JS 不动的 9 个标点。
const EC_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn enc(v: &str) -> String {
    utf8_percent_encode(v, EC_SET).to_string()
}

/// protocolVersion 协商（R5）。
fn negotiate_protocol(client: Option<&str>) -> &'static str {
    match client {
        Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => SUPPORTED_PROTOCOLS
            .iter()
            .find(|s| **s == v)
            .copied()
            .unwrap(),
        _ => DEFAULT_PROTOCOL,
    }
}

/// Accept 须含 application/json 或 text/event-stream（`*/*` 不含两者 → 拒，
/// 406；浏览器 form-CSRF 发不出这两种 Accept，配合 Content-Type 门形成双保险）。
fn accept_ok(headers: &HeaderMap) -> bool {
    let Some(a) = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let a = a.to_ascii_lowercase();
    a.contains("application/json") || a.contains("text/event-stream")
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
}

fn json_rpc_response(v: &Value) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        v.to_string(),
    )
        .into_response()
}

fn rpc_err(id: Value, code: i64, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// 按请求路径定位 api 挂载（`{prefix}/{suffix}` → (prefix, table)）。
fn mount_for_endpoint<'a>(
    st: &'a AppState,
    path: &str,
    suffix: &str,
) -> Option<(&'a str, &'a RouteTable)> {
    st.mounts.iter().find_map(|m| match m {
        RuntimeMount::Api { prefix, table, .. } => {
            let p = prefix.trim_end_matches('/');
            (path == format!("{p}/{suffix}")).then_some((p, table))
        }
        _ => None,
    })
}

/// tool 派生（单一来源）：generate → spec_to_tools → modules 白名单（fail-closed）。
fn tools_for(st: &AppState, prefix: &str, table: &RouteTable) -> Result<Vec<McpTool>, String> {
    let spec = crate::openapi::generate(table, prefix)?;
    let tools = spec_to_tools(&spec)?;
    Ok(apply_modules_filter(
        tools,
        st.pipeline.mcp.as_ref().and_then(|m| m.modules.as_ref()),
    ))
}

/// modules 白名单（R9 fail-closed）：Some 时仅保留 tags 与名单**有交集**的 tool，
/// 无 tags 的路由排除；名单匹配不到任何模块 → 空列表（不得回落全集）。
fn apply_modules_filter(tools: Vec<McpTool>, modules: Option<&Vec<String>>) -> Vec<McpTool> {
    let Some(mods) = modules else {
        return tools;
    };
    tools
        .into_iter()
        .filter(|t| {
            t.tool
                .get("tags")
                .and_then(|v| v.as_array())
                .is_some_and(|tags| {
                    tags.iter()
                        .any(|tg| tg.as_str().is_some_and(|s| mods.iter().any(|m| m == s)))
                })
        })
        .collect()
}

/// 由 table 找回 raw pattern（oa_path 已把 `{*x}` 收敛为 `{x}`，丢失 catch-all
/// 信息；填充编码策略必须按 raw pattern 段判定）。
fn raw_pattern_for(table: &RouteTable, oa_path: &str, method: &str) -> Option<String> {
    table
        .listing()
        .iter()
        .find(|r| {
            let oa_of_row = r
                .pattern
                .split('/')
                .map(|s| s.replacen("{*", "{", 1))
                .collect::<Vec<_>>()
                .join("/");
            let m = {
                let m = r.method.to_lowercase();
                if m == "del" { "delete".to_string() } else { m }
            };
            oa_of_row == oa_path && m == method
        })
        .map(|r| r.pattern.clone())
}

/// path 模板填充：单段参数 encodeURIComponent（值含 `/` 将正确 miss），
/// catch-all `{*name}` 原样插入（其中的 `/` 是真实分隔符）。
fn fill_path(raw_pattern: &str, args: Option<&Map<String, Value>>) -> Option<String> {
    let mut out = String::new();
    for seg in raw_pattern.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        if let Some(name) = seg.strip_prefix("{*").and_then(|s| s.strip_suffix('}')) {
            let v = args?.get(name)?;
            out.push_str(
                &v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            );
        } else if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            let v = args?.get(name)?;
            out.push_str(&enc(&v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())));
        } else {
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    Some(out)
}

/// query 值一律字符串化（RequestInfo.query 是 HashMap<String,String>，数组/多值
/// 塌缩——参数约定的明示语义）。
fn query_of(args: Option<&Map<String, Value>>) -> HashMap<String, String> {
    args.map(|m| {
        m.iter()
            .filter_map(|(k, v)| match v {
                Value::Null => None,
                Value::String(s) => Some((k.clone(), s.clone())),
                Value::Bool(b) => Some((k.clone(), b.to_string())),
                other => Some((k.clone(), other.to_string())),
            })
            .collect()
    })
    .unwrap_or_default()
}

fn is_envelope(v: &Value) -> bool {
    v.get("code").is_some_and(|c| c.is_number())
        && v.get("msg").is_some_and(|m| m.is_string())
        && v.get("data").is_some()
}

fn ok_result(data: &Value) -> Value {
    json!({
        "content": [{ "type": "text",
                      "text": serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string()) }],
        "structuredContent": data,
    })
}

fn is_error(msg: &str) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": msg }] })
}

/// Capture → CallToolResult（R3：content 必填 + structuredContent；
/// 业务 code!=0 → isError；R6：stream → isError 不缓冲）。
fn capture_to_result(cap: &Capture) -> Value {
    if cap.stream.is_some() {
        return is_error("streaming endpoint, not supported via MCP");
    }
    if !(200..300).contains(&cap.status) {
        return is_error(&String::from_utf8_lossy(&cap.body));
    }
    match serde_json::from_slice::<Value>(&cap.body) {
        Ok(v) if is_envelope(&v) => {
            if v["code"].as_i64() == Some(0) {
                ok_result(&v["data"])
            } else {
                is_error(v["msg"].as_str().unwrap_or("business error"))
            }
        }
        _ => json!({
            "content": [{ "type": "text", "text": String::from_utf8_lossy(&cap.body).into_owned() }]
        }),
    }
}

fn initialize_result(params: &Value, st: &AppState) -> Value {
    json!({
        "protocolVersion": negotiate_protocol(params.get("protocolVersion").and_then(|v| v.as_str())),
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "oj",
            "version": st
                .pipeline
                .mcp
                .as_ref()
                .map(|m| m.server_version.as_str())
                .unwrap_or(""),
        },
    })
}

enum Inbound {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// 无 id 的通知/响应消息：回 202 空 body，不进方法分派。
    Notification,
}

/// 端点公共门禁 + 组帧（R9 B1 endpoint admit / Content-Type / Accept / body 上限 /
/// batch 拒绝 / 解析失败 -32700）。Ok(Notification) = 调用方回 202。
// Response 是端点级拒绝的自然载体（与 handle 返回类型一致；admit 用 Box 是
// 历史签名）。result_large_err 属吹毛求疵：此处每请求只构造一次。
#[allow(clippy::result_large_err)]
async fn read_inbound(
    st: &AppState,
    headers: &HeaderMap,
    body: axum::body::Body,
    endpoint: &str,
) -> Result<Inbound, Response> {
    // R9 B1：endpoint 级 admit（字面端点路径）——拒绝 → 401 传输语义，不进 JSON-RPC 层。
    if let Err(resp) = admit(st, headers, "POST", Some(endpoint)) {
        return Err(*resp);
    }
    if !is_json_content_type(headers) {
        return Err(crate::fail_response(
            415,
            "Content-Type must be application/json",
        ));
    }
    if !accept_ok(headers) {
        return Err(crate::fail_response(
            406,
            "Accept must include application/json or text/event-stream",
        ));
    }
    let limit = st.pipeline.max_upload as usize;
    let bytes = match axum::body::to_bytes(body, limit.saturating_mul(2)).await {
        Ok(b) if b.len() <= limit => b,
        _ => return Err(crate::fail_response(413, "request too large")),
    };
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Err(json_rpc_response(&rpc_err(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
            )));
        }
    };
    // batch 数组 → -32600（单一错误对象，不逐元素处理）。
    if v.is_array() {
        return Err(json_rpc_response(&rpc_err(
            Value::Null,
            -32600,
            "batch requests are not supported",
        )));
    }
    let Some(obj) = v.as_object() else {
        return Err(json_rpc_response(&rpc_err(
            Value::Null,
            -32600,
            "invalid request",
        )));
    };
    let Some(id) = obj.get("id") else {
        return Ok(Inbound::Notification);
    };
    Ok(Inbound::Request {
        id: id.clone(),
        method: obj
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        params: obj.get("params").cloned().unwrap_or(Value::Null),
    })
}

pub async fn handle(
    State(st): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let inbound = match read_inbound(&st, &headers, body, "/mcp").await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let Inbound::Request { id, method, params } = inbound else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method.as_str() {
        "initialize" => Ok(initialize_result(&params, &st)),
        "ping" => Ok(json!({})),
        "tools/list" | "tools/call" => {
            let Some((prefix, table)) = mount_for_endpoint(&st, uri.path(), "mcp") else {
                return json_rpc_response(&rpc_err(id.clone(), -32603, "no api mount for /mcp"));
            };
            match method.as_str() {
                "tools/list" => tools_for(&st, prefix, table)
                    .map(|tools| json!({ "tools": tools.into_iter().map(|t| t.tool).collect::<Vec<_>>() }))
                    .map_err(|e| ("-32603", e)),
                _ => call_tool(&st, &headers, prefix, table, &params).await,
            }
        }
        _ => Err(("-32601", format!("method not found: {method}"))),
    };
    match result {
        Ok(r) => json_rpc_response(&rpc_result(&id, r)),
        Err((code, msg)) => {
            let code: i64 = code.parse().unwrap_or(-32603);
            json_rpc_response(&rpc_err(id.clone(), code, &msg))
        }
    }
}

/// `POST {base}/rpc`——裸 JSON-RPC 2.0（spec 2026-10-30）：method = **原始 operationId**
/// （非净化 tool 名）。与 MCP 的语义差异：业务信封 `code!=0` → JSON-RPC error
/// （裸 RPC 无 isError 概念，error 对象是正确载体，code 取业务 code）。
pub async fn rpc_handle(
    State(st): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    let inbound = match read_inbound(&st, &headers, body, "/rpc").await {
        Ok(i) => i,
        Err(resp) => return resp,
    };
    let Inbound::Request { id, method, params } = inbound else {
        return StatusCode::ACCEPTED.into_response();
    };
    let Some((prefix, table)) = mount_for_endpoint(&st, uri.path(), "rpc") else {
        // 按 /rpc 后缀定位本挂载。
        return json_rpc_response(&rpc_err(id.clone(), -32603, "no api mount for /rpc"));
    };
    let result = async {
        let tools = tools_for(&st, prefix, table).map_err(|e| (-32603, e))?;
        let Some(tool) = tools.iter().find(|t| t.dispatch.operation_id == method) else {
            return Err((-32601, format!("unknown method: {method}")));
        };
        let args = params.as_object();
        let cap = run_tool(&st, &headers, prefix, table, tool, args)
            .await
            .map_err(|(_, m)| (-32603, m))?;
        capture_to_rpc_result(&cap)
    }
    .await;
    match result {
        Ok(r) => json_rpc_response(&rpc_result(&id, r)),
        Err((code, msg)) => json_rpc_response(&rpc_err(id.clone(), code, &msg)),
    }
}

/// Capture → 裸 JSON-RPC result（/rpc 变体：业务错误是 error 对象而非 isError）。
fn capture_to_rpc_result(cap: &Capture) -> Result<Value, (i64, String)> {
    if cap.stream.is_some() {
        return Err((
            -32000,
            "streaming endpoint, not supported via RPC".to_string(),
        ));
    }
    if !(200..300).contains(&cap.status) {
        return Err((
            i64::from(cap.status),
            String::from_utf8_lossy(&cap.body).into_owned(),
        ));
    }
    match serde_json::from_slice::<Value>(&cap.body) {
        Ok(v) if is_envelope(&v) => {
            if v["code"].as_i64() == Some(0) {
                Ok(v["data"].clone())
            } else {
                Err((
                    v["code"].as_i64().unwrap_or(-32000),
                    v["msg"].as_str().unwrap_or("business error").to_string(),
                ))
            }
        }
        Ok(other) => Ok(other),
        Err(_) => Ok(Value::String(
            String::from_utf8_lossy(&cap.body).into_owned(),
        )),
    }
}

/// tools/call（MCP 变体）：tool 名 → dispatch 映射 → 执行 → Capture 映射（R3/R6）。
async fn call_tool(
    st: &AppState,
    headers: &HeaderMap,
    prefix: &str,
    table: &RouteTable,
    params: &Value,
) -> Result<Value, (&'static str, String)> {
    let Some(name) = params.get("name").and_then(|v| v.as_str()) else {
        return Err(("-32602", "params.name must be a string".to_string()));
    };
    let args = params.get("arguments").and_then(|v| v.as_object());
    let tools = tools_for(st, prefix, table).map_err(|e| ("-32603", e))?;
    let Some(tool) = tools.iter().find(|t| t.name == name) else {
        return Err(("-32601", format!("unknown tool: {name}")));
    };
    let cap = run_tool(st, headers, prefix, table, tool, args).await?;
    Ok(capture_to_result(&cap))
}

/// 公共执行段（MCP 与 /rpc 共用，零语义漂移）：path 填充 → normalize → lookup →
/// admit 取 tool 自身路径（R2 粒度）→ run_capture。
async fn run_tool(
    st: &AppState,
    headers: &HeaderMap,
    prefix: &str,
    table: &RouteTable,
    tool: &McpTool,
    args: Option<&Map<String, Value>>,
) -> Result<Capture, (&'static str, String)> {
    let raw = raw_pattern_for(table, &tool.dispatch.oa_path, &tool.dispatch.method).ok_or((
        "-32603",
        "route row vanished between list and call".to_string(),
    ))?;
    let path_args = args.and_then(|a| a.get("path")).and_then(|v| v.as_object());
    let Some(concrete) = fill_path(&raw, path_args) else {
        return Err((
            "-32602",
            format!("missing/invalid path argument for `{}`", tool.name),
        ));
    };
    // 参数致路径非法（如值解码为 "." / ".."）→ 坏参数协议错（-32602），与缺参同侧。
    let Some(norm) = routes::normalize(&concrete) else {
        return Err(("-32602", format!("invalid path argument: {concrete}")));
    };
    let verb = tool.dispatch.method.to_uppercase();
    let (file, params_map, handler) = match table.lookup(&norm, &verb) {
        Lookup::Hit {
            file,
            params,
            handler,
        } => (file, params, handler),
        Lookup::Conflict(m) => return Err(("-32603", format!("route conflict: {m}"))),
        Lookup::MethodNotAllowed => {
            return Err(("-32603", format!("method {verb} not allowed on {norm}")));
        }
        Lookup::NotFound => return Err(("-32603", format!("no route: {verb} {norm}"))),
    };
    // admit 的 path_no_base = tool 自身路径剥挂载前缀（per-route 粒度不摊平，R2）。
    let path_no_base = norm.strip_prefix(prefix).map(str::to_string);
    let query_args = args
        .and_then(|a| a.get("query"))
        .and_then(|v| v.as_object());
    let body_bytes = args
        .and_then(|a| a.get("body"))
        .map(|b| serde_json::to_vec(b).unwrap_or_default())
        .unwrap_or_default();
    run_capture(
        st,
        headers,
        &verb,
        query_of(query_args),
        path_no_base.as_deref(),
        &norm,
        file,
        params_map,
        handler,
        body_bytes,
        Vec::new(),
    )
    .await
    // 准入/超时/执行失败 → 合成 Capture 统一出口：MCP 侧 isError（R3，模型可读），
    // /rpc 侧 error 对象（capture_to_rpc_result 的 status 分支）。两侧语义不漂移。
    .or_else(|e| Ok(fail_capture(e.status, &e.msg)))
}

/// 合成失败 Capture（RunFail → 信封形态，content-type 标注供结果映射）。
fn fail_capture(status: u16, msg: &str) -> Capture {
    Capture {
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        body: serde_json::to_vec(&json!({ "code": status, "msg": msg, "data": null }))
            .unwrap_or_default(),
        stream: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- R5：协议协商与组帧 ----

    // Given client 报 2024-11-05 / 2099-01-01 / 缺省
    // Then 分别回显 2024-11-05 / 回 2025-06-18 / 回 2025-06-18
    #[test]
    fn initialize_negotiates_protocol_version() {
        assert_eq!(negotiate_protocol(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate_protocol(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate_protocol(Some("2099-01-01")), DEFAULT_PROTOCOL);
        assert_eq!(negotiate_protocol(None), DEFAULT_PROTOCOL);
    }

    // Given Accept 头各种形态
    // Then 含 application/json 或 text/event-stream 才放行；*/* 拒
    #[test]
    fn accept_gate_is_strict() {
        let mk = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(axum::http::header::ACCEPT, v.parse().unwrap());
            h
        };
        assert!(accept_ok(&mk("application/json, text/event-stream")));
        assert!(accept_ok(&mk("text/event-stream")));
        assert!(!accept_ok(&mk("*/*")));
        assert!(!accept_ok(&mk("text/html")));
        let mut no_accept = HeaderMap::new();
        assert!(!accept_ok(&no_accept));
        no_accept.insert(axum::http::header::ACCEPT, "".parse().unwrap());
        assert!(!accept_ok(&no_accept));
    }

    // ---- R9：白名单 fail-closed ----

    // Given modules=["不存在"] 与 tags=["user"] 的 tool
    // Then 空列表（不得回落全集）
    #[test]
    fn modules_whitelist_is_fail_closed() {
        let mk_tool = |tags: Value| McpTool {
            name: "t".into(),
            tool: json!({ "name": "t", "tags": tags }),
            dispatch: crate::mcp::ToolDispatch {
                operation_id: "t".into(),
                method: "get".into(),
                oa_path: "/t".into(),
            },
        };
        let tools = vec![mk_tool(json!(["user"])), mk_tool(Value::Null)];
        let out = apply_modules_filter(tools, Some(&vec!["不存在".to_string()]));
        assert!(out.is_empty(), "白名单无命中必须为空列表");
        // 有交集保留；无 tags 排除。
        let tools = vec![
            mk_tool(json!(["user"])),
            mk_tool(json!(["admin"])),
            mk_tool(Value::Null),
        ];
        let out = apply_modules_filter(tools, Some(&vec!["user".to_string()]));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tool["tags"][0], "user");
        // None = 不过滤。
        let tools = vec![mk_tool(json!(["user"])), mk_tool(Value::Null)];
        assert_eq!(apply_modules_filter(tools, None).len(), 2);
    }

    // ---- 参数约定：path 填充 ----

    // Given 单段参数值含 "/" 与特殊字符
    // Then encodeURIComponent（%2F 走私将在 lookup 期正确 miss）
    #[test]
    fn fill_path_encodes_single_segment() {
        let mut args = Map::new();
        args.insert("id".into(), json!("a/b?c d"));
        let p = fill_path("/v1/api/user/{id}", Some(&args)).unwrap();
        assert_eq!(p, "/v1/api/user/a%2Fb%3Fc%20d");
    }

    // Given catch-all 参数值含 "/"
    // Then 原样插入（真实分隔符，不编码）
    #[test]
    fn fill_path_keeps_catchall_slashes() {
        let mut args = Map::new();
        args.insert("path".into(), json!("a/b/c"));
        let p = fill_path("/v1/api/catch/{*path}", Some(&args)).unwrap();
        assert_eq!(p, "/v1/api/catch/a/b/c");
    }

    // Given 缺失 path 参数
    // Then None（→ -32602）
    #[test]
    fn fill_path_missing_arg_is_none() {
        assert!(fill_path("/v1/api/user/{id}", Some(&Map::new())).is_none());
        assert!(fill_path("/v1/api/user/{id}", None).is_none());
    }

    // Given 无参数模板（e2e 抓出的回归：曾把缺省 path 对象一律判 -32602）
    // Then None 参数合法，原样返回
    #[test]
    fn fill_path_paramless_needs_no_args() {
        // 尾斜杠由 routes::normalize 收敛，fill_path 不保证保留。
        assert_eq!(
            fill_path("/v1/api/user/account/", None).unwrap(),
            "/v1/api/user/account"
        );
        assert_eq!(
            fill_path("/v1/api/user/account/", Some(&Map::new())).unwrap(),
            "/v1/api/user/account"
        );
    }

    // ---- R3/R6：结果映射 ----

    #[test]
    fn capture_maps_envelope_ok_and_business_error() {
        let mut cap = Capture {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&json!({ "code": 0, "msg": "ok", "data": { "n": 1 } }))
                .unwrap(),
            stream: None,
        };
        let r = capture_to_result(&cap);
        assert_eq!(r["structuredContent"]["n"], 1);
        assert_eq!(r["content"][0]["type"], "text");
        assert!(r.get("isError").is_none());
        // 业务错误 → isError + msg（HTTP 200，非 JSON-RPC error）。
        cap.body = serde_json::to_vec(&json!({ "code": 40401, "msg": "not found", "data": null }))
            .unwrap();
        let r = capture_to_result(&cap);
        assert_eq!(r["isError"], true);
        assert_eq!(r["content"][0]["text"], "not found");
    }

    // Given cap.stream.is_some()（SSE/stream 端点）
    // Then isError 文案 "streaming endpoint"，不碰 body
    #[test]
    fn stream_endpoint_returns_iserror_instead_of_hanging() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(tx); // 类型由 Capture.stream 字段推断（bytes::Bytes），无需直接依赖 bytes crate。
        let cap = Capture {
            status: 200,
            headers: vec![],
            body: vec![],
            stream: Some(rx),
        };
        let r = capture_to_result(&cap);
        assert_eq!(r["isError"], true);
        assert!(
            r["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("streaming endpoint")
        );
    }

    // Given 非信封 JSON（json.raw 端点）
    // Then 原样 text content，无 structuredContent
    #[test]
    fn raw_json_body_passes_through_as_text() {
        let cap = Capture {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&json!({ "anything": true })).unwrap(),
            stream: None,
        };
        let r = capture_to_result(&cap);
        assert!(r.get("structuredContent").is_none());
        assert_eq!(
            r["content"][0]["text"],
            serde_json::to_string(&json!({ "anything": true })).unwrap()
        );
    }

    // ---- query 字符串化 ----

    #[test]
    fn query_values_are_stringified() {
        let mut q = Map::new();
        q.insert("s".into(), json!("x"));
        q.insert("n".into(), json!(1.5));
        q.insert("b".into(), json!(true));
        q.insert("nil".into(), Value::Null);
        let out = query_of(Some(&q));
        assert_eq!(out.get("s").unwrap(), "x");
        assert_eq!(out.get("n").unwrap(), "1.5");
        assert_eq!(out.get("b").unwrap(), "true");
        assert!(!out.contains_key("nil"));
        assert!(query_of(None).is_empty());
    }
}
