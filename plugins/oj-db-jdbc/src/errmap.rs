//! 错误归类 + 凭据脱敏（design spec §5.8）：JDBC `SQLException` 的
//! sql_state/error_code → 宿主 `{category,retryable,session_disposition}` 信封三元组。
//! SQLState class 表镜像 oj-db-mysql（plugins/oj-db-mysql/src/lib.rs `sqlstate_class`）。

/// (category, retryable, session_disposition)。
pub type Classified = (&'static str, bool, &'static str);

/// SQLState class（前两位）→ 归类。只认标准类；未收录类落 internal（禁止猜）。
fn sqlstate_class(code: Option<&str>) -> Option<Classified> {
    let class = code?.get(..2)?;
    Some(match class {
        "08" => ("connection", true, "close"),
        "0A" => ("unsupported", false, "keep"), // SQLFeatureNotSupportedException 标准类
        "23" => ("constraint", false, "keep"),
        "40" => ("timeout", true, "keep"),
        "42" => ("syntax", false, "keep"),
        "53" => ("capacity", true, "keep"),
        "57" => ("cancelled", false, "keep"),
        _ => return None,
    })
}

/// 归类入口。标准 SQLState 优先；拿不到标准类时——达梦/金仓部分错误只给
/// 厂商码——绝不硬套 MySQL 表后静默误判 retryable：直接 internal（spec §5.8 硬性）。
/// `error_code` 原样留待信封 `data.vendor_code`（透传，禁止猜语义）。
pub fn classify(sql_state: Option<&str>, _error_code: Option<i32>) -> Classified {
    sqlstate_class(sql_state).unwrap_or(("internal", false, "keep"))
}

/// 错误消息凭据脱敏（spec §5.8 硬性）：`://` 与 `@` 之间的凭据段（不得含 `/` 或空白）
/// 换成 `***`，host/库名保留——排障要知道连的是哪个库，但日志/HTTP 响应里不该有密码。
/// 非 URL 形态原样返回（裸密码无法可靠识别，不误伤普通文本）。
pub fn redact(msg: &str) -> String {
    // 全程扫描不提前返回：同一行可能回显多个连接串（如「主库 X 失败回退 Y」），
    // 只脱第一个会让第二个的凭据原样泄漏。
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(rel) = rest.find("://") {
        let seg = rel + 3;
        let bytes = rest.as_bytes();
        let mut j = seg;
        // 凭据段：@ 之前不得出现 / 或空白（出现 = 已是路径，不是凭据）。
        let at = loop {
            match bytes.get(j) {
                Some(b'@') => break Some(j),
                Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\r') | Some(b'\n') | None => {
                    break None;
                }
                _ => j += 1,
            }
        };
        if let Some(at) = at {
            out.push_str(&rest[..rel]);
            out.push_str("://***@");
            rest = &rest[at + 1..];
        } else {
            out.push_str(&rest[..seg]);
            rest = &rest[seg..];
        }
    }
    out.push_str(rest);
    out
}
