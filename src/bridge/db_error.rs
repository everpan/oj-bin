//! 数据库结构化错误信封（M1 / T3）。
//!
//! 跨 FFI 边界后，db 轴 vtable 的 `Err(RString)` 载荷统一为 JSON 信封
//! `{category, retryable, session_disposition, message, data}`。宿主在 `await_ffi` 出口
//! **单点** `parse_raw`；任何解析失败（含 panic 收敛文本、legacy 纯文本）一律归
//! `{Internal, false, Keep, message: raw 原文}`。未知 category 串 → `Internal`。
//! `data.vendor_code` 原样透传（ORA-xxxxx / 达梦错误码 / pg SQLSTATE），禁止猜。
//!
//! `unsupported` 是 DRY 单构造器，供六门控点与 default-throw 统一产出结构化 unsupported。

use deno_error::{AdditionalProperties, JsErrorBox, JsErrorClass, PropertyValue};
use serde_json::{Map, Value};

/// 错误类别（设计 §5.1 category 集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorCategory {
    Syntax,
    Constraint,
    Timeout,
    Connection,
    Capacity,
    Unsupported,
    Cancelled,
    Internal,
}

impl DbErrorCategory {
    /// 小写串（与信封 `category` 字段、`sql_trace` 画像同源；`parse_raw` 的反向映射）。
    pub fn as_str(self) -> &'static str {
        match self {
            DbErrorCategory::Syntax => "syntax",
            DbErrorCategory::Constraint => "constraint",
            DbErrorCategory::Timeout => "timeout",
            DbErrorCategory::Connection => "connection",
            DbErrorCategory::Capacity => "capacity",
            DbErrorCategory::Unsupported => "unsupported",
            DbErrorCategory::Cancelled => "cancelled",
            DbErrorCategory::Internal => "internal",
        }
    }
}

/// 会话处置（执行者为**插件自身**——映射错误时逐出自己的池；宿主只据此放弃重试 / 丢弃会话）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDisposition {
    Keep,
    Reset,
    Close,
}

impl SessionDisposition {
    /// 小写串（与信封 `session_disposition` 字段同源；JS 面 `e.sessionDisposition` 用它）。
    pub fn as_str(self) -> &'static str {
        match self {
            SessionDisposition::Keep => "keep",
            SessionDisposition::Reset => "reset",
            SessionDisposition::Close => "close",
        }
    }
}

/// 跨 FFI 的结构化数据库错误。
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeError {
    pub category: DbErrorCategory,
    pub retryable: bool,
    pub session_disposition: SessionDisposition,
    pub message: String,
    pub data: Map<String, Value>,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BridgeError {}

/// deno_core 经 `JsErrorClass::get_additional_properties` 把 `code` / `data` /
/// `retryable` / `sessionDisposition` 作为 JS Error 的自身属性附加（deno_error 机制）。
/// `PropertyValue` 仅支持 String/Number，故 `data` 以 JSON 文本透传——JS 侧 `e.data` 为 JSON
/// 串，bootstrap.js 就地 `JSON.parse` 为对象（`e.data.capability`）；`retryable` 以 1/0 数字
/// 下发，`normDbErr` 再归一为布尔（重试决策不必解析 `data`）。
/// `get_message` 保留原始人类可读文本（既有 handler 在 catch 里匹配的文本逐字节不变）。
struct OjDbJsError {
    code: &'static str,
    message: String,
    data: String,
    retryable: bool,
    disposition: &'static str,
}

impl std::fmt::Debug for OjDbJsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OjDbJsError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("data", &self.data)
            .finish()
    }
}

impl std::fmt::Display for OjDbJsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for OjDbJsError {}

impl JsErrorClass for OjDbJsError {
    fn get_class(&self) -> std::borrow::Cow<'static, str> {
        // 必须用 deno_core 已注册的基类名（"Error" == `GENERIC_ERROR`），否则
        // buildCustomError 在 errorMap 里查不到类名 → 返回 undefined → V8 抛 undefined
        // 退化成 `TypeError: invalid_argument`。结构化信息由 e.code / e.data 携带（见下），
        // 不依赖 e.name 判别。
        "Error".into()
    }
    fn get_message(&self) -> std::borrow::Cow<'static, str> {
        self.message.clone().into()
    }
    fn get_additional_properties(&self) -> AdditionalProperties {
        Box::new(
            [
                (
                    std::borrow::Cow::Borrowed("code"),
                    PropertyValue::from(self.code),
                ),
                (
                    std::borrow::Cow::Borrowed("data"),
                    PropertyValue::from(self.data.clone()),
                ),
                (
                    std::borrow::Cow::Borrowed("retryable"),
                    PropertyValue::Number(if self.retryable { 1.0 } else { 0.0 }),
                ),
                (
                    std::borrow::Cow::Borrowed("sessionDisposition"),
                    PropertyValue::from(self.disposition),
                ),
            ]
            .into_iter(),
        )
    }
    fn get_ref(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
}

/// 宿主 op 边界：`BridgeError` → `JsErrorBox`，经 deno_core 把 `code`（`code_of(category)`）、
/// `data`（JSON 文本）、`retryable`（1/0，`normDbErr` 归一为布尔）与 `sessionDisposition`
/// 作为 JS Error 的自身属性透传；`e.message` 保留原始文本。
impl From<BridgeError> for JsErrorBox {
    fn from(e: BridgeError) -> Self {
        let code = code_of(e.category);
        let data = serde_json::to_string(&Value::Object(e.data.clone()))
            .unwrap_or_else(|_| "{}".to_string());
        let disposition = e.session_disposition.as_str();
        JsErrorBox::from_err(OjDbJsError {
            code,
            message: e.message,
            data,
            retryable: e.retryable,
            disposition,
        })
    }
}

/// category → 信封 `code` 固定映射（JS 面 / `_sql` 信封 / sql_trace 同源）。
pub fn code_of(category: DbErrorCategory) -> &'static str {
    match category {
        DbErrorCategory::Syntax => "OJ_DB_SYNTAX",
        DbErrorCategory::Constraint => "OJ_DB_CONSTRAINT",
        DbErrorCategory::Timeout => "OJ_DB_TIMEOUT",
        DbErrorCategory::Connection => "OJ_DB_CONNECTION",
        DbErrorCategory::Capacity => "OJ_DB_CAPACITY",
        DbErrorCategory::Unsupported => "OJ_DB_UNSUPPORTED",
        DbErrorCategory::Cancelled => "OJ_DB_CANCELLED",
        DbErrorCategory::Internal => "OJ_DB_INTERNAL",
    }
}

/// 单点解析 FFI `Err` 载荷为 [`BridgeError`]。
///
/// - 合法信封对象：逐字段映射；未知 category 串 → `Internal`；`data.vendor_code` 透传。
/// - 非信封（解析失败 / 裸文本 / panic 收敛文本）→ `{Internal, false, Keep, message: raw 原文}`。
pub fn parse_raw(raw: &str) -> BridgeError {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(obj)) if obj.get("category").is_some() => {
            let category = match obj.get("category").and_then(Value::as_str) {
                Some("syntax") => DbErrorCategory::Syntax,
                Some("constraint") => DbErrorCategory::Constraint,
                Some("timeout") => DbErrorCategory::Timeout,
                Some("connection") => DbErrorCategory::Connection,
                Some("capacity") => DbErrorCategory::Capacity,
                Some("unsupported") => DbErrorCategory::Unsupported,
                Some("cancelled") => DbErrorCategory::Cancelled,
                Some("internal") => DbErrorCategory::Internal,
                // 未知 category 串 → Internal（禁止猜）。
                _ => DbErrorCategory::Internal,
            };
            let retryable = obj
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let session_disposition = match obj.get("session_disposition").and_then(Value::as_str) {
                Some("reset") => SessionDisposition::Reset,
                Some("close") => SessionDisposition::Close,
                _ => SessionDisposition::Keep,
            };
            let message = obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(raw)
                .to_string();
            let data = obj
                .get("data")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            BridgeError {
                category,
                retryable,
                session_disposition,
                message,
                data,
            }
        }
        // 非信封：legacy 纯文本 / panic 收敛文本 / 合法 JSON 但非信封 → 归 internal，保留原文。
        _ => BridgeError {
            category: DbErrorCategory::Internal,
            retryable: false,
            session_disposition: SessionDisposition::Keep,
            message: raw.to_string(),
            data: Map::new(),
        },
    }
}

/// DRY 单构造器：未知方言 / 未声明能力的结构化 unsupported。
///
/// message 模板（设计 §5.5）：`{capability} is not supported on dialect "{dialect}" yet.
/// See docs/devkit/db-guide.md 方言能力矩阵.`
pub fn unsupported(dialect: &str, capability: &str) -> BridgeError {
    let message = format!(
        "{capability} is not supported on dialect \"{dialect}\" yet. See docs/devkit/db-guide.md 方言能力矩阵."
    );
    let mut data = Map::new();
    data.insert(
        "capability".to_string(),
        Value::String(capability.to_string()),
    );
    data.insert("dialect".to_string(), Value::String(dialect.to_string()));
    BridgeError {
        category: DbErrorCategory::Unsupported,
        retryable: false,
        session_disposition: SessionDisposition::Keep,
        message,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_envelope() {
        // Given 合法信封 JSON
        // When parse_raw → category==Constraint; data["vendor_code"]==1062; message=="dup"
        let raw = r#"{"category":"constraint","retryable":false,"session_disposition":"keep","message":"dup","data":{"vendor_code":1062}}"#;
        let e = parse_raw(raw);
        assert_eq!(e.category, DbErrorCategory::Constraint);
        assert!(!e.retryable);
        assert_eq!(e.session_disposition, SessionDisposition::Keep);
        assert_eq!(e.message, "dup");
        assert_eq!(
            e.data.get("vendor_code").and_then(Value::as_i64),
            Some(1062)
        );
    }

    #[test]
    fn parse_legacy_text() {
        // Given "boom: something broke"（裸文本/panic）→ {Internal, false, Keep}, message 保留原文
        let e = parse_raw("boom: something broke");
        assert_eq!(e.category, DbErrorCategory::Internal);
        assert!(!e.retryable);
        assert_eq!(e.session_disposition, SessionDisposition::Keep);
        assert_eq!(e.message, "boom: something broke");
    }

    #[test]
    fn parse_unknown_category() {
        // Given {"category":"weird",...} → Internal
        let e = parse_raw(r#"{"category":"weird","retryable":true,"message":"x"}"#);
        assert_eq!(e.category, DbErrorCategory::Internal);
    }

    #[test]
    fn parse_vendor_code_passthrough() {
        // data.vendor_code 原样透传（pg SQLSTATE 串亦同）。
        let e = parse_raw(
            r#"{"category":"connection","retryable":false,"message":"m","data":{"vendor_code":"08006"}}"#,
        );
        assert_eq!(e.category, DbErrorCategory::Connection);
        assert_eq!(
            e.data.get("vendor_code").and_then(Value::as_str),
            Some("08006")
        );
    }

    #[test]
    fn unsupported_constructor() {
        // Given unsupported("oracle","query_build")
        // Then code_of()==OJ_DB_UNSUPPORTED; data.capability=="query_build";
        //      message 含 dialect 名与 raw 路径指引
        let e = unsupported("oracle", "query_build");
        assert_eq!(e.category, DbErrorCategory::Unsupported);
        assert_eq!(code_of(e.category), "OJ_DB_UNSUPPORTED");
        assert_eq!(
            e.data.get("capability").and_then(Value::as_str),
            Some("query_build")
        );
        assert_eq!(
            e.data.get("dialect").and_then(Value::as_str),
            Some("oracle")
        );
        assert!(
            e.message.contains("oracle"),
            "message must name dialect: {}",
            e.message
        );
        assert!(
            e.message.contains("docs/devkit/db-guide.md"),
            "message must carry guidance: {}",
            e.message
        );
    }

    #[test]
    fn code_of_mapping() {
        assert_eq!(code_of(DbErrorCategory::Syntax), "OJ_DB_SYNTAX");
        assert_eq!(code_of(DbErrorCategory::Constraint), "OJ_DB_CONSTRAINT");
        assert_eq!(code_of(DbErrorCategory::Timeout), "OJ_DB_TIMEOUT");
        assert_eq!(code_of(DbErrorCategory::Connection), "OJ_DB_CONNECTION");
        assert_eq!(code_of(DbErrorCategory::Capacity), "OJ_DB_CAPACITY");
        assert_eq!(code_of(DbErrorCategory::Unsupported), "OJ_DB_UNSUPPORTED");
        assert_eq!(code_of(DbErrorCategory::Cancelled), "OJ_DB_CANCELLED");
        assert_eq!(code_of(DbErrorCategory::Internal), "OJ_DB_INTERNAL");
    }

    /// T9：BridgeError → JsErrorBox 把 code / data 作为 JS Error 的自身属性透传，
    /// message 保留原文（既有 handler catch 匹配文本逐字节不变）。
    #[test]
    fn bridge_error_maps_to_js_error_with_code_and_data() {
        let be = unsupported("oracle", "query_build");
        let js: JsErrorBox = be.into();
        // e.name 必须是 deno_core 已注册的基类名（"Error"），否则 buildCustomError 无法构造对象。
        assert_eq!(JsErrorClass::get_class(&js), "Error");
        // e.message 保留人类可读原文（含方言名与指引）
        let msg = JsErrorClass::get_message(&js);
        assert!(msg.contains("oracle"), "msg must keep dialect name: {msg}");
        assert!(
            msg.contains("query_build"),
            "msg must keep capability: {msg}"
        );
        // e.code / e.data / e.retryable / e.sessionDisposition 来自 additional_properties
        let mut code = None;
        let mut data_json = None;
        let mut retryable = None;
        let mut disposition = None;
        for (k, v) in JsErrorClass::get_additional_properties(&js) {
            match k.as_ref() {
                "code" => {
                    if let PropertyValue::String(s) = v {
                        code = Some(s.to_string());
                    }
                }
                "data" => {
                    if let PropertyValue::String(s) = v {
                        data_json = Some(s.to_string());
                    }
                }
                // PropertyValue 只有 String/Number：布尔以 1/0 下发，JS 侧 normDbErr 归一。
                "retryable" => {
                    if let PropertyValue::Number(n) = v {
                        retryable = Some(n != 0.0);
                    }
                }
                "sessionDisposition" => {
                    if let PropertyValue::String(s) = v {
                        disposition = Some(s.to_string());
                    }
                }
                _ => {}
            }
        }
        assert_eq!(code.as_deref(), Some("OJ_DB_UNSUPPORTED"));
        assert_eq!(
            retryable,
            Some(false),
            "e.retryable 必须可读（免解析 data）"
        );
        assert_eq!(disposition.as_deref(), Some("keep"));
        let data: Value =
            serde_json::from_str(&data_json.expect("e.data must be present")).unwrap();
        assert_eq!(data["capability"], "query_build");
        assert_eq!(data["dialect"], "oracle");
    }
}
