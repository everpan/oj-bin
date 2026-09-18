//! http 请求上下文绑定，只读，懒加载（每次访问从 ReqState 取最新）。

use std::cell::RefCell;
use std::rc::Rc;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{Value, json};

use super::ReqState;

/// 上传文件（multipart；op_http_info 只出元信息，字节经 op_http_file 按索引取）。
#[derive(Default, Clone)]
pub struct UploadedFile {
    pub field: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// 一次 HTTP 请求的上下文（由 server 层填充）。
#[derive(Default, Clone)]
pub struct RequestInfo {
    pub method: String,
    pub params: HashMap<String, String>,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    /// 帧型标志（v0.1.16）：WS binary 帧为 true——`http.body` 置 null（丢弃 lossy
    /// 文本），原始字节走 `http.bodyBytes()`；HTTP 请求恒 false。
    pub body_binary: bool,
    /// 租户 id（tenant.enable 时由 handle() 从 header 提取注入；否则 None）。
    pub tenant_id: Option<String>,
    /// 匿名请求标志（v0.1.20）：**仅** `tenant.anonymous_paths` 命中、且确实没带租户头
    /// 的 HTTP 请求为 true（server handle 的 `None if exempt` 分支置位）。
    /// `tenant_id=None` 的其他来源（租户未启用 / WS 帧 / 任务桥 / 测试默认 RequestInfo）
    /// **一律 false** —— 它是 `db.asTenant` 的唯一授信判据，不能用 `tenant_id.is_none()`
    /// 代替（那会把 WS/任务/测试路径一并放行）。
    pub anonymous: bool,
    /// 已验签用户（auth 启用且非匿名路径：{id, roles, claims}；否则 None）。
    pub user: Option<Value>,
    /// 上传文件（multipart 解析结果；非 multipart 为空）。
    pub files: Vec<UploadedFile>,
    /// WS 会话的 bus 发送端（bus.subscribe 注册用；HTTP 请求为 None）。
    pub bus_tx: Option<tokio::sync::mpsc::UnboundedSender<super::WsSend>>,
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
        // binary 帧：body 置 null（lossy 文本是损坏数据，不给误用面），字节走 op_http_body_bytes。
        "body": if s.req.body_binary {
            Value::Null
        } else {
            export_bytes(&s.req.body)
        },
        "tenantId": s.req.tenant_id,
        "user": s.req.user,
        "files": s.req.files.iter().map(|f| json!({
            "field": f.field,
            "filename": f.filename,
            "content_type": f.content_type,
            "size": f.bytes.len(),
        })).collect::<Vec<_>>(),
    })
}

/// http.file(i) → 第 i 个上传文件字节（越界 Err "no such file"）。
/// async + #[buffer] 返回（sync buffer-return 在 fast-call 路径卡死；与 blob ops 同款契约）。
#[op2]
#[buffer]
pub async fn op_http_file(
    state: Rc<RefCell<OpState>>,
    #[smi] i: i32,
) -> Result<Vec<u8>, JsErrorBox> {
    let s = state.borrow();
    let r = s.borrow::<ReqState>();
    r.req
        .files
        .get(i as usize)
        .map(|f| f.bytes.clone())
        .ok_or_else(|| JsErrorBox::generic(format!("no such file: {i}")))
}

/// http.bodyBytes()：当前请求/帧的原始字节（WS 文本与二进制帧都可用；HTTP 请求
/// 即原始 body）。async + #[buffer] 返回（sync buffer-return 在 fast-call 路径卡死；
/// 与 op_http_file 同款契约）。
#[op2]
#[buffer]
pub async fn op_http_body_bytes(state: Rc<RefCell<OpState>>) -> Result<Vec<u8>, JsErrorBox> {
    let s = state.borrow();
    let r = s.borrow::<ReqState>();
    Ok(r.req.body.clone())
}

/// exportBytes：空为 null，能解析为 JSON 则解析，否则按 UTF-8 字符串。
fn export_bytes(b: &[u8]) -> Value {
    if b.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(b)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(b).into_owned()))
}
