//! 错误归类 + 凭据脱敏表驱动测试（design spec §5.8）。

use crate::errmap::{classify, redact};

/// SQLState class 表镜像 oj-db-mysql（plugins/oj-db-mysql/src/lib.rs `sqlstate_class`）：
/// (sql_state, category, retryable, session_disposition)。
#[test]
fn sqlstate_class_table() {
    let cases: &[(&str, &str, bool, &str)] = &[
        ("08006", "connection", true, "close"),
        ("08S01", "connection", true, "close"),
        ("23000", "constraint", false, "keep"),
        ("23505", "constraint", false, "keep"),
        ("40001", "timeout", true, "keep"),
        ("40P01", "timeout", true, "keep"),
        ("42601", "syntax", false, "keep"),
        ("53300", "capacity", true, "keep"),
        ("57014", "cancelled", false, "keep"),
        ("0A000", "unsupported", false, "keep"), // SQLFeatureNotSupportedException 标准类
    ];
    for (state, cat, retryable, disp) in cases {
        assert_eq!(
            classify(Some(state), None),
            (*cat, *retryable, *disp),
            "sql_state={state}"
        );
    }
}

/// §5.8 硬性兜底：SQLState 为 None（达梦/金仓部分错误只给厂商码）——
/// 拿不到标准 SQLState 时绝不硬套 MySQL 表后静默误判 retryable。
#[test]
fn null_sqlstate_never_silent_retryable() {
    // DM 厂商码形态（无标准 SQLState）：粗分类不可得 → internal + 不 retryable。
    for code in [
        Some(-6001),
        Some(100),
        Some(0),
        Some(i32::MIN),
        Some(i32::MAX),
    ] {
        let (cat, retryable, disp) = classify(None, code);
        assert_eq!(
            (cat, retryable, disp),
            ("internal", false, "keep"),
            "code={code:?}"
        );
    }
}

/// 非标准 SQLState（厂商自造码 / 空串 / 长度不足）同样落 internal。
#[test]
fn nonstandard_sqlstate_falls_back_internal() {
    for state in ["", "DM", "D0", "XY123"] {
        let (cat, retryable, _) = classify(Some(state), Some(12345));
        assert_eq!((cat, retryable), ("internal", false), "state={state}");
    }
}

/// 凭据脱敏：`://[^@/\s]*@` → `://***@`（spec §5.8，镜像宿主 secret::redact 语义）。
#[test]
fn redact_url_credentials() {
    let cases: &[(&str, &str)] = &[
        (
            "jdbc:dm://sysdba:Secret123@192.168.67.64:5237",
            "jdbc:dm://***@192.168.67.64:5237",
        ),
        (
            "mysql://root:hunter2@127.0.0.1:3306/app",
            "mysql://***@127.0.0.1:3306/app",
        ),
        ("redis://:pwd@h:6379/1", "redis://***@h:6379/1"),
        (
            "oracle://scott:tiger@//db:1521/X",
            "oracle://***@//db:1521/X",
        ),
        // 无凭据 / 路径先于 @ → 不含凭据段，原样返回。
        ("jdbc:h2:mem:test", "jdbc:h2:mem:test"),
        ("sqlite://db.sqlite", "sqlite://db.sqlite"),
        ("com.foo@bar/baz", "com.foo@bar/baz"),
        ("a://b/c@d", "a://b/c@d"), // 路径先于 @ → 非凭据段
        ("", ""),
    ];
    for (input, want) in cases {
        assert_eq!(redact(input), *want, "input={input}");
    }
}

/// 回归（review finding）：密码含 `/` 必须脱敏——旧实现见 `/` 即停，
/// `jdbc:mysql://user:p/ass@host:3306/db` 的密码 `p/ass` 会原样泄漏进日志/HTTP 响应。
#[test]
fn redact_password_containing_slash() {
    let cases: &[(&str, &str)] = &[
        (
            "jdbc:mysql://user:p/ass@host:3306/db",
            "jdbc:mysql://***@host:3306/db",
        ),
        // 密码同时含 @ 与 /：取最后一个 @ 作为终止符，整段密码进 ***。
        (
            "jdbc:mysql://user:p@ss/word@dbhost:3306/x",
            "jdbc:mysql://***@dbhost:3306/x",
        ),
        // 无 user:password（无 `:`）的路径先于 @ → 不误脱敏（保留既有语义）。
        ("a://b/c@d", "a://b/c@d"),
        // 无凭据 / 非 URL 原样返回。
        (
            "jdbc:mysql://host:3306/db?useSSL=false",
            "jdbc:mysql://host:3306/db?useSSL=false",
        ),
        (
            "connection refused: password=hunter2",
            "connection refused: password=hunter2",
        ),
    ];
    for (input, want) in cases {
        assert_eq!(redact(input), *want, "input={input}");
    }
}

/// 消息中间夹带连接串也脱敏（驱动报错消息回显完整 JDBC URL）。
#[test]
fn redact_embedded_in_message() {
    let msg = "Connection refused: jdbc:dm://u:p@host:5237 (IoException)";
    assert_eq!(
        redact(msg),
        "Connection refused: jdbc:dm://***@host:5237 (IoException)"
    );
}

/// 同一行多个连接串全部脱敏（只脱第一个是安全控件覆盖缺口）。
#[test]
fn redact_multiple_urls_in_line() {
    let msg = "primary jdbc:dm://u1:p1@h1:5237 failed, fallback jdbc:dm://u2:p2@h2:5237";
    assert_eq!(
        redact(msg),
        "primary jdbc:dm://***@h1:5237 failed, fallback jdbc:dm://***@h2:5237"
    );
    // 第二个 URL 的凭据不得残留。
    assert!(!redact(msg).contains("u2:p2") && !redact(msg).contains("u1:p1"));
}
