//! 方言能力闭集解析器（M1 / T2）。
//!
//! 插件在工厂级 `describe()` 返回 `{dialect, capabilities}` JSON；本模块将其收敛为
//! 闭集结构体 [`DialectCaps`] / [`DialectInfo`]。互斥组（placeholder / id）缺项、多项或
//! 出现未知串一律 `Err` 并点名，杜绝「typo 插件把裸 `?` 发给 Oracle 运行期才炸」。
//!
//! 能力串常量集中在本文件（[`CAP_PLACEHOLDER_Q`] 等 8 个），是宿主与插件共用的唯一来源。

use serde_json::Value;

/// 占位符风格（互斥组，3 选 1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderStyle {
    /// `?` 位置占位符（mysql / sqlite）。
    Q,
    /// `$1` 序号占位符（postgres）。
    Dollar,
    /// `:name` 命名占位符（oracle / dameng，宿主漏斗对 colon 透传，改写归插件内部）。
    Colon,
}

/// 自增 ID 取回策略（互斥组，3 选 1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdStrategy {
    /// `RETURNING` 单语句取回（postgres）。
    Returning,
    /// `LAST_INSERT_ID` 两步取回（mysql）。
    TwoStep,
    /// 无自增 ID（如分析型 / 外部赋值）。
    None,
}

/// 收敛后的方言能力闭集（装配期一次解析，运行期纯查表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialectCaps {
    pub placeholder: PlaceholderStyle,
    pub id: IdStrategy,
    pub stream: bool,
    pub cancel: bool,
}

/// 方言完整描述符（装配期冻结进 `StableState.dbs`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialectInfo {
    pub name: String,
    pub caps: DialectCaps,
}

impl DialectInfo {
    /// 启动横幅 / `plugins()` op 的紧凑能力串（如 `placeholder=$ id=returning stream`）。
    pub fn summary(&self) -> String {
        let ph = match self.caps.placeholder {
            PlaceholderStyle::Q => "?",
            PlaceholderStyle::Dollar => "$",
            PlaceholderStyle::Colon => ":",
        };
        let id = match self.caps.id {
            IdStrategy::Returning => "returning",
            IdStrategy::TwoStep => "two_step",
            IdStrategy::None => "none",
        };
        let mut s = format!("placeholder={ph} id={id}");
        if self.caps.stream {
            s.push_str(" stream");
        }
        if self.caps.cancel {
            s.push_str(" cancel");
        }
        s
    }

    /// 对外 wire 形态（`plugins()` / `GET {base}/plugins` 的 `caps` 列）：与插件
    /// `describe()` **同一套词汇**（`{"dialect":..,"capabilities":["placeholder_dollar",..]}`）。
    ///
    /// 不 derive `Serialize`：那会把 Rust 枚举名（`"Dollar"`/`"Returning"`）冻结成公开契约，
    /// 与能力串常量形成第二套词汇，下游无法与 describe 对账。序列化走 [`Self::to_wire`]。
    pub fn to_wire(&self) -> Value {
        let mut caps: Vec<&str> = vec![match self.caps.placeholder {
            PlaceholderStyle::Q => CAP_PLACEHOLDER_Q,
            PlaceholderStyle::Dollar => CAP_PLACEHOLDER_DOLLAR,
            PlaceholderStyle::Colon => CAP_PLACEHOLDER_COLON,
        }];
        caps.push(match self.caps.id {
            IdStrategy::Returning => CAP_ID_RETURNING,
            IdStrategy::TwoStep => CAP_ID_TWO_STEP,
            IdStrategy::None => CAP_ID_NONE,
        });
        if self.caps.stream {
            caps.push(CAP_STREAM);
        }
        if self.caps.cancel {
            caps.push(CAP_CANCEL);
        }
        serde_json::json!({ "dialect": self.name, "capabilities": caps })
    }
}

/// 唯一序列化口径 = describe 词汇（见 [`DialectInfo::to_wire`]）。
impl serde::Serialize for DialectInfo {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_wire().serialize(s)
    }
}

/// 能力集解析错误（fail-loud，点名违规串 / 组名）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapsError {
    /// 未知能力串（typo / 未识别），携带违规串原文。
    Unknown(String),
    /// describe JSON 本身不可解析（与「能力串 typo」分开，便于运维定位是插件坏了还是写错了）。
    BadJson(String),
    /// 互斥组缺项（如缺 placeholder 组），携带组名。
    MissingGroup(&'static str),
    /// 互斥组多项（如同时含 placeholder_q 与 placeholder_dollar），携带组名。
    DuplicateGroup(&'static str),
}

impl std::fmt::Display for CapsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapsError::Unknown(s) => write!(f, "unknown capability \"{s}\""),
            CapsError::BadJson(s) => write!(f, "describe json invalid: {s}"),
            CapsError::MissingGroup(g) => write!(f, "missing capability group \"{g}\""),
            CapsError::DuplicateGroup(g) => write!(f, "duplicate capability in group \"{g}\""),
        }
    }
}

impl std::error::Error for CapsError {}

// 能力串常量——宿主与插件共用的唯一来源（精确值见设计 §5.1）。
pub const CAP_PLACEHOLDER_Q: &str = "placeholder_q";
pub const CAP_PLACEHOLDER_DOLLAR: &str = "placeholder_dollar";
pub const CAP_PLACEHOLDER_COLON: &str = "placeholder_colon";
pub const CAP_ID_RETURNING: &str = "id_returning";
pub const CAP_ID_TWO_STEP: &str = "id_two_step";
pub const CAP_ID_NONE: &str = "id_none";
pub const CAP_STREAM: &str = "stream";
pub const CAP_CANCEL: &str = "cancel";

/// 解析 `describe()` 返回的 JSON 为闭集 [`DialectInfo`]。
///
/// 输入形态：`{"dialect":"...","capabilities":["..."]}`。
/// - `dialect` 缺失或非字符串 → `MissingGroup("dialect")`。
/// - 未知能力串 → `Unknown`（携带原文，优先于组完整性检查）。
/// - placeholder / id 互斥组：恰好一枚；0 → `MissingGroup`，>1 → `DuplicateGroup`。
/// - `stream` / `cancel`：出现即 `true`，缺省 `false`；重复出现 → `DuplicateGroup`。
pub fn caps_from_json(describe_json: &str) -> Result<DialectInfo, CapsError> {
    let v: Value =
        serde_json::from_str(describe_json).map_err(|e| CapsError::BadJson(e.to_string()))?;

    let obj = v
        .as_object()
        .ok_or_else(|| CapsError::BadJson("describe json is not an object".to_string()))?;

    let name = obj
        .get("dialect")
        .and_then(|d| d.as_str())
        .ok_or(CapsError::MissingGroup("dialect"))?
        .to_string();
    // 空名不是合法方言：会让 unsupported 文案出现 `dialect ""`，也让「一插件=一方言」的
    // 装配约束失去可辨识度。
    if name.trim().is_empty() {
        return Err(CapsError::MissingGroup("dialect"));
    }

    // `capabilities` 存在但不是数组 = 插件 bug，必须点名（否则会被当成空表，
    // 最终以「缺 placeholder 组」报错，排障指向完全错误的方向）。缺省视为空表。
    let caps_arr = match obj.get("capabilities") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(other) => {
            return Err(CapsError::BadJson(format!(
                "describe.capabilities is not an array: {other}"
            )));
        }
    };

    // 互斥组：同时累计「命中计数」与「解析值」；计数 >1 即重复组，解析值用于取回具体项。
    let mut ph_style: Option<PlaceholderStyle> = None;
    let mut ph_count: u32 = 0;
    let mut id_strategy: Option<IdStrategy> = None;
    let mut id_count: u32 = 0;
    let mut stream_count: u32 = 0;
    let mut cancel_count: u32 = 0;
    let mut unknown: Option<String> = None;

    for cap in &caps_arr {
        let s = match cap.as_str() {
            Some(s) => s,
            None => {
                unknown = unknown.or(Some(cap.to_string()));
                continue;
            }
        };
        match s {
            CAP_PLACEHOLDER_Q => {
                ph_style = Some(PlaceholderStyle::Q);
                ph_count += 1;
            }
            CAP_PLACEHOLDER_DOLLAR => {
                ph_style = Some(PlaceholderStyle::Dollar);
                ph_count += 1;
            }
            CAP_PLACEHOLDER_COLON => {
                ph_style = Some(PlaceholderStyle::Colon);
                ph_count += 1;
            }
            CAP_ID_RETURNING => {
                id_strategy = Some(IdStrategy::Returning);
                id_count += 1;
            }
            CAP_ID_TWO_STEP => {
                id_strategy = Some(IdStrategy::TwoStep);
                id_count += 1;
            }
            CAP_ID_NONE => {
                id_strategy = Some(IdStrategy::None);
                id_count += 1;
            }
            CAP_STREAM => stream_count += 1,
            CAP_CANCEL => cancel_count += 1,
            _ => {
                unknown = unknown.or_else(|| Some(s.to_string()));
            }
        }
    }

    // 未知串优先点名（typo 必须 fail-loud，不能静默容忍）。
    if let Some(u) = unknown {
        return Err(CapsError::Unknown(u));
    }
    let placeholder = match (ph_style, ph_count) {
        (Some(s), 1) => s,
        (None, _) => return Err(CapsError::MissingGroup("placeholder")),
        (Some(_), _) => return Err(CapsError::DuplicateGroup("placeholder")),
    };
    let id = match (id_strategy, id_count) {
        (Some(s), 1) => s,
        (None, _) => return Err(CapsError::MissingGroup("id")),
        (Some(_), _) => return Err(CapsError::DuplicateGroup("id")),
    };
    if stream_count > 1 {
        return Err(CapsError::DuplicateGroup("stream"));
    }
    if cancel_count > 1 {
        return Err(CapsError::DuplicateGroup("cancel"));
    }

    Ok(DialectInfo {
        name,
        caps: DialectCaps {
            placeholder,
            id,
            stream: stream_count == 1,
            cancel: cancel_count == 1,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(name: &str, caps: &[&str]) -> String {
        let caps_json = caps
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"dialect\":\"{name}\",\"capabilities\":[{caps_json}]}}")
    }

    #[test]
    fn parse_ok_full() {
        // Given 能力集 ["placeholder_q","id_two_step","stream","cancel"]
        // When caps_from_json
        // Then Ok(info); name=="x"; placeholder==Q; id==TwoStep; stream&&cancel
        let info = caps_from_json(&describe(
            "x",
            &["placeholder_q", "id_two_step", "stream", "cancel"],
        ))
        .expect("should parse");
        assert_eq!(info.name, "x");
        assert_eq!(info.caps.placeholder, PlaceholderStyle::Q);
        assert_eq!(info.caps.id, IdStrategy::TwoStep);
        assert!(info.caps.stream);
        assert!(info.caps.cancel);
    }

    #[test]
    fn parse_ok_minimal() {
        // 仅占位 + id 组，stream/cancel 缺省 false。
        let info =
            caps_from_json(&describe("pg", &["placeholder_dollar", "id_returning"])).unwrap();
        assert_eq!(info.caps.placeholder, PlaceholderStyle::Dollar);
        assert_eq!(info.caps.id, IdStrategy::Returning);
        assert!(!info.caps.stream);
        assert!(!info.caps.cancel);
    }

    #[test]
    fn parse_ok_colon_none() {
        let info = caps_from_json(&describe(
            "oracle",
            &["placeholder_colon", "id_none", "stream"],
        ))
        .unwrap();
        assert_eq!(info.caps.placeholder, PlaceholderStyle::Colon);
        assert_eq!(info.caps.id, IdStrategy::None);
        assert!(info.caps.stream);
        assert!(!info.caps.cancel);
    }

    #[test]
    fn duplicate_placeholder_group() {
        // Given ["placeholder_q","placeholder_dollar","id_none"] → Err(DuplicateGroup("placeholder"))
        let err = caps_from_json(&describe(
            "x",
            &["placeholder_q", "placeholder_dollar", "id_none"],
        ))
        .unwrap_err();
        assert_eq!(err, CapsError::DuplicateGroup("placeholder"));
    }

    #[test]
    fn missing_placeholder_group() {
        // Given ["id_none"] → Err(MissingGroup("placeholder"))
        let err = caps_from_json(&describe("x", &["id_none"])).unwrap_err();
        assert_eq!(err, CapsError::MissingGroup("placeholder"));
    }

    #[test]
    fn unknown_capability_typo() {
        // Given ["placeholder_q","id_none","strem"]（typo）→ Err(Unknown("strem")) 且 Display 含 "strem"
        let err =
            caps_from_json(&describe("x", &["placeholder_q", "id_none", "strem"])).unwrap_err();
        assert_eq!(err, CapsError::Unknown("strem".to_string()));
        assert!(
            err.to_string().contains("strem"),
            "display must name the bad string: {err}"
        );
    }

    #[test]
    fn missing_dialect() {
        // Given dialect 缺失 → Err(MissingGroup("dialect"))
        let json = "{\"capabilities\":[\"placeholder_q\",\"id_none\"]}";
        let err = caps_from_json(json).unwrap_err();
        assert_eq!(err, CapsError::MissingGroup("dialect"));
    }

    #[test]
    fn duplicate_stream_group() {
        let err = caps_from_json(&describe(
            "x",
            &["placeholder_q", "id_none", "stream", "stream"],
        ))
        .unwrap_err();
        assert_eq!(err, CapsError::DuplicateGroup("stream"));
    }

    #[test]
    fn duplicate_id_group() {
        let err = caps_from_json(&describe(
            "x",
            &["placeholder_q", "id_returning", "id_two_step"],
        ))
        .unwrap_err();
        assert_eq!(err, CapsError::DuplicateGroup("id"));
    }

    #[test]
    fn invalid_json() {
        // 非 JSON 与「JSON 合法但形状不对」分开报：前者是插件 bug（BadJson），
        // 后者是能力位拼错（Unknown）——装配期排障要能一眼区分。
        let err = caps_from_json("not json").unwrap_err();
        assert!(matches!(err, CapsError::BadJson(_)), "{err:?}");
        let err = caps_from_json(r#"{"dialect":"x","capabilities":"stream"}"#).unwrap_err();
        assert!(matches!(err, CapsError::BadJson(_)), "{err:?}");
    }
}
