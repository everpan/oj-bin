//! DSN 解析（design spec §5.2）。connect 收到的 DSN 三种形态：
//! 1. 裸 JDBC URL（凭据在 URL 内）——原样透出（`Bare`），插件不做解析；
//! 2. `jdbc:{...}` JSON 信封——剥掉 `jdbc:` 前缀后见 `{` 按 JSON 解析（`Envelope`）；
//! 3. 整串 `ENC[...]` 密封值——原样透出（`Bare`）；解密在宿主配置装载期
//!    （`decrypt_tree` 递归，早于 connect），插件零支持（spec §5.2 ENC 密封范式）。

use serde_json::Value;

/// JSON 信封的解析结果。`driver_class` 从 `props` 提取（spec §5.2 逃生舱：
/// 缺 ServiceLoader 条目的老商用 jar 显式 Class.forName），不随 props 上送。
#[derive(Debug)]
pub struct Envelope {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub props: serde_json::Map<String, Value>,
    pub driver_class: Option<String>,
}

#[derive(Debug)]
pub enum ParsedDsn {
    Bare(String),
    Envelope(Envelope),
}

pub fn parse_dsn(dsn: &str) -> Result<ParsedDsn, String> {
    // 形态 3：整串 ENC 密封值原样透出。解密在宿主配置装载期完成（早于 connect），
    // 能到这里 = JS 运行时 db.client.register 等动态路径传入的整串密封值——按契约
    // 插件不做解密（信封内嵌 ENC 字段宿主也不解，见 spec §5.2）。
    if dsn.starts_with("ENC[") {
        return Ok(ParsedDsn::Bare(dsn.to_string()));
    }
    let Some(rest) = dsn.strip_prefix("jdbc:") else {
        return Ok(ParsedDsn::Bare(dsn.to_string()));
    };
    if !rest.starts_with('{') && !rest.starts_with('[') {
        // 形态 1：裸 JDBC URL（含凭据），原样透出——JDBC 驱动自己解析。
        return Ok(ParsedDsn::Bare(dsn.to_string()));
    }
    // 形态 2：JSON 信封。
    let v: Value = serde_json::from_str(rest).map_err(|e| format!("jdbc: envelope parse: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "jdbc: envelope must be a JSON object".to_string())?;
    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "jdbc: envelope missing \"url\"".to_string())?
        .to_string();
    let user = obj.get("user").and_then(Value::as_str).map(str::to_string);
    let password = parse_password(obj.get("password"))?;
    let mut props = match obj.get("props") {
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err("jdbc: envelope props must be an object".to_string()),
        None => serde_json::Map::new(),
    };
    let driver_class = match props.remove("driver_class") {
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err("jdbc: envelope props.driver_class must be a string".to_string()),
        None => None,
    };
    Ok(ParsedDsn::Envelope(Envelope {
        url,
        user,
        password,
        props,
        driver_class,
    }))
}

/// password 支持明文串或 `{"$env":"VAR"}` 环境变量引用（spec §5.2）。
/// 未设置 → Err 指明变量名（部署机可诊断）；空串视为已设置（值就是空密码）。
fn parse_password(v: Option<&Value>) -> Result<Option<String>, String> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Object(m)) => {
            let (key, val) = m.iter().next().ok_or_else(|| {
                "jdbc: envelope password $env object must not be empty".to_string()
            })?;
            if key != "$env" || m.len() != 1 {
                return Err(
                    "jdbc: envelope password must be a string or {\"$env\":\"NAME\"}".to_string(),
                );
            }
            let name = val
                .as_str()
                .ok_or_else(|| "jdbc: envelope password $env value must be a string".to_string())?;
            std::env::var(name)
                .map(Some)
                .map_err(|_| format!("env var {name} not set"))
        }
        Some(_) => {
            Err("jdbc: envelope password must be a string or {\"$env\":\"NAME\"}".to_string())
        }
    }
}

/// vendor 识别表（spec §5.5）：`jdbc:<vendor>:...` 的子协议段 → 方言族。
/// 仅识别，不改变运行时行为（`dialect()` 恒 "jdbc"）；结果用于诊断日志/错误文案。
/// 未知 vendor 原样透出；`&'static` 签名下以 Box::leak 承载（每连接一次，量级可忽略）。
pub fn vendor_of(url: &str) -> &'static str {
    let rest = url.strip_prefix("jdbc:").unwrap_or(url);
    let seg = rest.split(':').next().unwrap_or(rest);
    match seg {
        "postgresql" | "postgres" => "postgres",
        "mysql" => "mysql",
        "oracle" => "oracle",
        "dm" => "dameng",
        "kingbase8" => "kingbase",
        "sqlserver" | "microsoft" => "mssql",
        // 七方言扩展（REQ-A4，v0.1.144）：仅诊断臂；heavydb 收编旧前缀
        // mapd/omnisci（上游更名史：MapD → OmniSci → HeavyAI）。
        "snowflake" => "snowflake",
        "databricks" => "databricks",
        "ch" | "clickhouse" => "clickhouse",
        "trino" => "trino",
        "kyuubi" => "kyuubi",
        "heavyai" | "mapd" | "omnisci" => "heavydb",
        "duckdb" => "duckdb",
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}
