//! oj × Tauri 桌面集成（模式 C：无端口，App::dispatch 直调）。
//!
//! - `setup` 中 `oj::app::App::from_config` 构造 App（与 oj serve 同一装配），
//!   存入 Tauri 状态；不调用 `serve_graceful`（业务 API 无 TCP 监听）。
//! - **资源自包含**：dev 用 `CARGO_MANIFEST_DIR`（src-tauri）；release 用 Tauri 随包打进
//!   `.app/Contents/Resources` 的副本（config.yaml / certs / src），经 `app.path().resource_dir()`
//!   取，不再依赖构建机路径，可拷到别的机器直接跑。
//! - **sqlite 安全/读写**：release 下把 db 重定向到每用户私有可写目录
//!   `app.path().app_data_dir()`（macOS = `~/Library/Application Support/com.oj.desktop.demo`），
//!   目录 700 不对外可读、可写，绝不落在只读 bundle 内。
//! - `#[tauri::command] oj_dispatch` 把 Tauri IPC 请求转成 `axum::http::Request`，
//!   经 `App::dispatch` 在进程内路由 + 前置管线 + 鉴权 + 证书门禁，返回信封。
//! - 前端用 `window.__TAURI__.invoke('oj_dispatch', ...)`（或 Tauri 自定义协议，
//!   见设计文档 §3/§4 的零改 fetch 方案），业务代码无需感知传输差异。
//! - **dev（debug）构建自动弹出 WebKit DevTools**（real devtool），便于桌面端调试前端。

use oj::app::{App, ResourceProfiles};
use std::collections::HashMap;
use std::path::PathBuf;
use tauri::Manager;

/// Tauri 托管状态：持有 oj 的 App。App: Send+Sync+'static（impl ClientTransport 已编译实证）。
pub struct AppState {
    pub app: App,
}

/// IPC 响应：状态码 + 响应头 + 响应体（UTF-8 字符串；blob 二进制为后续项，见设计 Q5）。
#[derive(serde::Serialize)]
pub struct OjResp {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

#[tauri::command]
async fn oj_dispatch(
    state: tauri::State<'_, AppState>,
    method: String,
    uri: String,
    headers: HashMap<String, String>,
    body: Option<String>,
) -> Result<OjResp, String> {
    let mut builder = axum::http::Request::builder()
        .method(method.as_str())
        .uri(uri.as_str());
    for (k, v) in &headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let req = builder
        .body(axum::body::Body::from(body.unwrap_or_default()))
        .map_err(|e| format!("build request: {e}"))?;

    // 完整 router + 前置管线 + auth/tenant + 证书 GET 门禁（server/src/lib.rs handle）。
    let resp = state.app.dispatch(req).await;
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| format!("read body: {e}"))?;

    let mut hm = HashMap::new();
    for (k, v) in parts.headers.iter() {
        hm.insert(k.as_str().to_string(), v.to_str().unwrap_or("").to_string());
    }
    Ok(OjResp {
        status: parts.status.as_u16(),
        headers: hm,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// 任意显示型错误 → `Box<dyn Error>`，供 `setup` 闭包统一返回。
fn boxerr(e: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // dev（debug）用 CARGO_MANIFEST_DIR；release 用打包进 Resources 的资源目录。
            // 注意 `app.path().resource_dir()` 在 dev 返回 `target/debug`（无 config），故必须分叉。
            let (config_dir, api_dir) = if cfg!(debug_assertions) {
                let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                (manifest.clone(), manifest.join("src"))
            } else {
                let res = app.path().resource_dir().map_err(boxerr)?; // .app/Contents/Resources
                (res.clone(), res.join("src"))
            };

            // 读 oj 配置（release 下为只读 bundle 内副本）。
            let mut cfg =
                only_js::config::load_from(&config_dir, Some("config.yaml")).map_err(boxerr)?;

            // release：sqlite 重定向到每用户私有可写目录，不进只读 bundle。
            // 目录 700 不对外可读 —— 满足 macOS 打包安全与读写要求。
            if !cfg!(debug_assertions) {
                let data = app.path().app_data_dir().map_err(boxerr)?;
                std::fs::create_dir_all(&data).ok();
                cfg.db.insert(
                    "default".into(),
                    format!("sqlite://{}/app.sqlite", data.display()),
                );
            }

            let base = cfg.server.api_prefix.clone();
            let ts = true; // dev 与 release 均运行时转译 TS（免 oj build 预构建步骤）

            // App::from_config 为 async，Tauri 自带 tokio runtime，block_on 构造一次。
            // 注意：此处的 `oj_app` 不能与闭包入参的 Tauri `app` 重名，否则 `app.manage`
            // 会误调到 oj 的 App（无该方法）。
            let oj_app = tauri::async_runtime::block_on(async {
                App::from_config(cfg, &config_dir, api_dir, base, ts, false, &ResourceProfiles::default()).await
            })
            .map_err(boxerr)?;

            app.manage(AppState { app: oj_app });

            // dev（debug）自动弹出 WebKit DevTools（real devtool）。
            #[cfg(debug_assertions)]
            if let Some(w) = app.get_webview_window("main") {
                w.open_devtools();
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![oj_dispatch])
        .run(tauri::generate_context!())
        .expect("run tauri app");
}
