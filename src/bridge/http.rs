//! http 请求上下文绑定（移植自 Go http.go），只读，懒加载（每次访问从 ReqState 取最新）。

use deno_core::{OpState, op2};
use serde_json::{Value, json};

use super::ReqState;

/// 一次 HTTP 请求的上下文（由 server 层填充）。
#[derive(Default, Clone)]
pub struct RequestInfo {
    pub method: String,
    pub params: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// 租户 id（tenant.enable 时由 handle() 从 header 提取注入；否则 None）。
    pub tenant_id: Option<String>,
    /// 已验签用户（auth 启用且非匿名路径：{id, roles, claims}；否则 None）。
    pub user: Option<Value>,
}

use std::collections::HashMap;

/// http 全局对象的快照（bootstrap 每次访问调用，保证 per-request 最新）。
#[op2]
#[serde]
pub fn op_http_info(state: &mut OpState) -> serde_json::Value {
    let s = state.borrow::<ReqState>();
    json!({
        "method": s.req.method,
        "params": s.req.params,
        "query": s.req.query,
        "headers": s.req.headers,
        "body": export_bytes(&s.req.body),
        "tenantId": s.req.tenant_id,
        "user": s.req.user,
    })
}

/// 移植 Go 的 exportBytes：空为 null，能解析为 JSON 则解析，否则按 UTF-8 字符串。
fn export_bytes(b: &[u8]) -> Value {
    if b.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(b)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(b).into_owned()))
}
