use std::net::SocketAddr;
use std::sync::Arc;

use only_js::bridge::{
    Bridge, InMemoryAccessor, InMemoryKV, RequestInfo, SchemaRegistry,
};

/// 演示：构造 Bridge（含 schema 白名单 + runtime 池 + 可选 inspector），跑业务 JS、
/// 读捕获响应。
///
/// `inspect_addr` 为 `Some` 时启用 DevTools inspector 并在该地址起 WS 服务。
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_demo(None).await
}

/// 演示逻辑（抽取为独立 async fn 便于测试覆盖；`main` 仅以 `None` 调用）。
pub async fn run_demo(inspect_addr: Option<SocketAddr>) -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt().try_init();

    let db = Arc::new(InMemoryAccessor::new());
    db.seed([serde_json::json!({"id": 1, "name": "ever", "age": 18})]);
    let kv = Arc::new(InMemoryKV::new());

    // schema 注册表：动态标识符白名单（SQL 注入根治点）。
    let registry = SchemaRegistry::new().table("user", Some("id"), &["id", "name", "age"]);

    let inspect = inspect_addr.is_some();
    let b = Bridge::with_opts(db, kv, registry, inspect);

    let mut insp_handle = None;
    if let Some(addr) = inspect_addr {
        // 后台 WS 服务存活到 demo 运行结束；随后中止以释放 inspector 引用，
        // 避免 pooled runtime 析构时仍持有 JsRuntimeInspector（deno_core 断言）。
        insp_handle = Some(only_js::bridge::start_inspector(&b, addr));
    }

    let cap = b
        .run_with(
            r#"
            redis.set("last_query", "user")
              .then(() => db.table("user")
                .select(["id","name"])
                .where({field:"age",op:"gte",value:18})
                .orderBy([{field:"id",dir:"desc"}])
                .limit(10).all())
              .then((rows) => {
                json.header("X-Handler", "demo");
                json.ok({ users: rows, req: { method: http.method, query: http.query } });
              })
              .catch((e) => json.fail(500, String(e)));
            "#,
            RequestInfo {
                method: "GET".into(),
                query: [("id".into(), "1".into())].into_iter().collect(),
                ..Default::default()
            },
        )
        .await?;

    println!("status={} headers={:?}", cap.status, cap.headers);
    println!("{}", String::from_utf8_lossy(&cap.body));

    // 收尾：中止 inspector 后台任务并等待其退出，先于 Bridge 析构释放 inspector 引用。
    if let Some(h) = insp_handle.take() {
        h.abort();
        let _ = h.await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn demo_runs_without_inspector() {
        run_demo(None).await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn demo_runs_with_inspector() {
        // 随机端口（:0）避免并行冲突；后台 WS 服务随测试进程结束回收。
        // start_inspector 内部用 spawn_local，必须在 LocalSet 上下文内运行。
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        tokio::task::LocalSet::new()
            .run_until(async move { run_demo(Some(addr)).await.unwrap() })
            .await;
    }
}

