//! log 结构化日志绑定（基于 tracing）。
//!
//! JS 用法（msg + 交替键值对）：
//!
//! ```js
//! log.info("user login", "user_id", uid, "ip", ip);
//! ```

use deno_core::op2;

/// log.debug/info/warn/error：level 0-3，fields 为已 stringify 的 JSON 对象字符串（可空）。
/// fast：仅基础类型参数，无 V8 引用 / 无异步。
#[op2(fast)]
pub fn op_log(level: u8, #[string] msg: &str, #[string] fields: String) {
    let fields = if fields.is_empty() {
        "{}"
    } else {
        fields.as_str()
    };
    match level {
        0 => tracing::debug!(target: "js", %msg, fields = %fields),
        1 => tracing::info!(target: "js", %msg, fields = %fields),
        2 => tracing::warn!(target: "js", %msg, fields = %fields),
        _ => tracing::error!(target: "js", %msg, fields = %fields),
    }
}

/// 级别门（perf F7）：logCall 据此在 JS 侧早退，被 tracing 过滤掉的级别
/// （生产 info 级下的 debug）不再白付 fields 构建 + stringify + String 拷贝。
/// 与 op_log 同一 target/level 语义（tracing::enabled 走同一 subscriber 判定）。
#[op2(fast)]
pub fn op_log_enabled(level: u8) -> bool {
    match level {
        0 => tracing::enabled!(target: "js", tracing::Level::DEBUG),
        1 => tracing::enabled!(target: "js", tracing::Level::INFO),
        2 => tracing::enabled!(target: "js", tracing::Level::WARN),
        _ => tracing::enabled!(target: "js", tracing::Level::ERROR),
    }
}

#[cfg(test)]
mod tests {
    use crate::bridge::{Bridge, InMemoryAccessor, InMemoryKV};
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread")]
    async fn all_levels_and_empty_fields() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        );
        let cap = b
            .run(
                r#"
                log.debug("debug-msg");
                log.info("info-msg", "k", 1);
                log.warn("warn-msg");
                log.error("error-msg");
                log.info("no-fields");
                json.ok({ ok: 1 });
                "#,
            )
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
    }
}
