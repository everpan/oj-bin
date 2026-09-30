//! 验证 IPC 后端：`oj_dispatch` 命令内部正是调用 `App::dispatch`。
//! 这里绕过 Tauri 前端，直接构造请求并 dispatch，确认返回信封（状态码 + body）。
//! 运行：`cargo run --example verify_dispatch --release`（复用已编译的 oj/only-js）。

use oj::app::{App, ResourceProfiles};
use only_js::config;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // = src-tauri
    let cfg = config::load_from(&manifest, Some("config.yaml")).expect("load config.yaml");
    let base = cfg.server.api_prefix.clone();
    let dir = manifest.join("src"); // api 根：sample/api.ts → /v1/api/sample/

    let app = App::from_config(cfg, &manifest, dir, base, /*ts=*/ true, /*fixtures=*/ false, &oj::app::ResourceProfiles::default())
        .await
        .expect("build oj App");

    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/v1/api/sample/")
        .body(axum::body::Body::from(""))
        .unwrap();

    let resp = app.dispatch(req).await;
    let status = resp.status().as_u16();
    let (_parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();

    println!("IPC-BACKEND status={status}");
    println!("IPC-BACKEND body={body}");
    assert!(status == 200, "expected 200, got {status}");
    assert!(
        body.contains("oj tauri"),
        "expected hello payload, got {body}"
    );
    println!("IPC-BACKEND OK: App::dispatch 返回预期信封");
}
