# `oj tauri` 子命令设计（桌面端无端口集成，Mode C）

- 日期：2026-09-29
- 状态：v2（已据架构/工程/产品三路评审调整；处置见 §12）
- 关联调研：`docs/tauri-integration.md`；本设计采纳「模式 C：JSON/REST 无端口，WS 混合监听」

## 1. 背景与目标

oj 是嵌入 V8 的低代码后端框架，HTTP 服务本质是一个标准 `axum::Router`
（`server/src/lib.rs:174` `app()`；`oj::app::App` 暴露 `from_config` 与 `dispatch(req)`
经 `router.oneshot` 在进程内完成路由 + 前置管线 + 鉴权 + 证书门禁，`oj/src/app.rs:1195` /
`server/src/lib.rs:354` `handle`）。

目标：新增 `oj tauri` 子命令，把 oj 后端以**桌面应用**形态交付。让前端（Tauri webview）
调用 oj handler 时**本地不起面向业务的 TCP 端口**——经 Tauri IPC / 自定义协议在进程内
`App::dispatch` 处理。

**价值重述（评审修正）**：`127.0.0.1` 本就不暴露局域网，故「无端口」的安全收益有限；
真正的价值是**单二进制桌面分发** + **前后端同进程、共享内存、统一生命周期**，以及避免
`oj serve` 的端口/生命周期管理。无端口是这一形态的伴随结果，而非首要卖点。

## 2. 范围与边界（YAGNI）

- **做**：`oj tauri` 脚手架生成 + 开发/构建承载；桌面运行时以「IPC/自定义协议桥接」跑通
  oj 的 `/v1/api/*` 与 `/blob/*`（无端口）；sqlite 本地库开箱可用；WS 走本机最小监听。
- **不做**：不重写 oj 运行时；不替换 Tauri 前端构建；不内嵌 mysql/postgres/redis/s3
  等需插件的后端（桌面默认 sqlite，可选随包分发插件 cdylib）。
- **迁移注意**：依赖 HTTP 语义（multipart 上传、证书 GET 门禁、WS）的 handler 在桌面形态下
  行为一致（同一 `handle`/`pipeline`），无业务代码迁移清单之外的漂移。

## 3. 总体架构（采纳：生成器 + 伴生运行时 crate）

oj 主 CLI **不链 Tauri**，保持轻 CLI 定位；Tauri 依赖隔离在独立 crate `oj-tauri`。

```
oj/ (主 CLI)
  └─ args.rs: Commands::Tauri { #[command(subcommand)] cmd: TauriCmd }
       TauriCmd::New { name, frontend }   生成 Tauri 工程骨架 + 写 oj-tauri 依赖
       TauriCmd::Dev                      调起 `cargo tauri dev`（生成工程内）
       TauriCmd::Build                    调起 `cargo tauri build`
  └─ tauri_cmd.rs: 仅生成代码 / 透传 cargo tauri，不跑 webview

oj-tauri/ (伴生 crate，依赖 oj + tauri)
  └─ main.rs:
       - setup 中 `oj::app::App::from_config(cfg, ...).await?` 构造 App（同 oj serve 装配）
       - `tauri::Builder::manage(Arc<App>)` 注入状态
       - 注册 `#[tauri::command] oj_dispatch(method, uri, headers, body) -> OjResp`
           内部 `APP.dispatch(req).await`，Response 转 OjResp{status,headers,body}
       - 注册**自定义协议**（如 `oj://` 或 `http://oj.local`）：协议处理器内部调
         `oj_dispatch`，使前端 `fetch` **零改动**
       - WS：另起最小 axum 监听（见 §7），端口经 invoke 传给前端
       - 窗口关闭 → 置 `App::tasks_flag()` 触发优雅停机
  └─ 不调用 serve_graceful（业务 API 无监听）；仅 App::dispatch + WS 监听
```

## 4. 数据流（零改 fetch）

```
webview 业务代码（原样 fetch）
  → fetch('oj://v1/api/user/account/?id=1')        // 自定义协议伪装同源
  → Tauri 协议处理器 → oj_dispatch(...)
  → Rust: Request::builder().method(uri).headers(body)
        → APP.dispatch(req).await                   // 完整 router + pipeline + auth + cert 门禁
        → OjResp{status, headers, body_bytes}
  → 协议处理器回写 Response → fetch 解析 envelope {code,msg,data}
```

`App::dispatch` 已覆盖静态站点兜底、`/blob/*` GET（含 Range）、证书 GET 门禁、auth/tenant
前置管线（`server/src/lib.rs:354` `handle`）。**业务代码无需把 `fetch` 换成 `ojFetch`**——
自定义协议在传输层完成 IPC 桥接（评审采纳产品意见，避免破坏性改造）。

## 5. 认证模型（关键）

`dispatch` 走完整前置管线，oj-auth Bearer 守卫照常生效。桌面端推荐落地：

- **Tauri 后端作可信代理**：`oj tauri` 脚手架期用 oj-auth 的密钥预签发/生成「本地服务账号」
  keypair 并随包分发证书；`oj_dispatch` 在构造 Request 时自动注入
  `Authorization: Bearer <local-jwt>`。前端无需管理令牌；webview 已在进程内、视为可信。
  保留 oj 既有鉴权语义，零改造 handler。

**硬性约束（评审修正，删除 `--no-cert`）**：`from_config` 强制要求
`server.public_key_path` + `server.certificate_path` 必配，否则 fail-fast，且**代码明言无任何
配置/旗标可跳过**（`oj/src/app.rs:951`）。故桌面端**必须随包分发证书**——脚手架期生成
自签名 keypair 写入 config 指向的资源路径，不做「跳过证书」后门（不破坏证书强制校验不变量）。
证书续期：桌面场景比服务端更痛，列为 §11 开放问题（随包证书 + 版本升级时重签）。

## 6. 配置 / 模块 / 插件打包

- `config.yaml` + `dist`（release，不转译、按锁聚合）作为 Tauri 资源随包；`oj tauri build`
  先 `oj build`（src→dist）再随包。
- 本地库：`db.default` 指向用户目录下的 sqlite 文件（内置，无需插件）。
- 插件 cdylib（可选）：**显式**设 `OJ_PLUGINS_DIR` 或 config `plugins_dir` 指向
  `tauri::Manager::resolve_resource` 解压目录（不可依赖 `<exe>/plugins` 默认，资源在
  `Resources` 目录）；`oj tauri build` 把 `bin/plugins/<host-triple>/*` 拷入资源并写入该变量。

> 实现注记（已据 Tauri 2.12 实测）：旧文档写的 `Manager::resolve_resource` 在 Tauri 2.x 已改为
> `app.path().resource_dir() -> Result<PathBuf>`（release 返 `.app/Contents/Resources`，dev 返
> `target/debug`，**dev 下无 config**，故 dev/release 必须分叉：dev 用 `CARGO_MANIFEST_DIR`、
> release 用 `resource_dir()`）。`sample-desktop` 的 `src/lib.rs` 已按此实现并通过验证。
- `App::from_config` 的 `config_dir` 指向资源解压目录（Tauri `resolveResource`）。
- 插件须与 `ABI_VERSION`（`oj-plugin-ffi`）严格相等且同 `host-triple` 构建，否则加载即
  fail-fast；`oj tauri build` 显式校验 cert + plugins 存在再打包。

## 7. WebSocket（评审定稿：混合，非 spike）

`App::dispatch` 经 `router.oneshot` 派发——**`oneshot` 下 WS 升级握手虽返回 101，但永不跑
帧循环**（`app.rs:6-8` 明示；`on_upgrade` 闭包依赖真实 socket，oneshot 无 TCP 连接故不触发）。
故**无端口直连 WS 在架构上不可能**，原「待 spike 验证」为误判，直接定方案：

- **JSON / REST / 静态 / blob：无端口**，走 `App::dispatch`（已被 `oj test` 的
  `op_client_dispatch`→`App::dispatch` 实证，可用，无需 spike）。
- **WS：复用 `App::ws_router()` + 本机最小 axum 监听**（127.0.0.1 随机端口；`client.ws`
  已有同构实现，`app.rs:1301` 注释），前端经 `invoke('oj_ws_port')` 拿到端口后直连
  `ws://127.0.0.1:<port>/...`。该监听仅服务 WS 路由，不暴露业务 REST（REST 仍走 dispatch），
  且不暴露局域网（bind 127.0.0.1）。

## 8. 生命周期与并发

- `App` 在 Tauri `setup` 构造一次，`Arc` 注入 `manage`。**`App: Send + Sync + 'static` 已由
  `impl ClientTransport` 编译证明**（`app.rs:49-55`，并被 `oj test` 注入 OpState），`manage`
  可直接 `Arc<App>`，无需额外验证（评审修正，原「需编译验证」删除）。
- `Bridge` 为 `!Send`、跑 current_thread 池，oj 已内部封装；`dispatch` 从 Tauri 的
  multi_thread 调用安全（actor 自行 spawn 到其池）。oj serve 本就在 multi_thread 跑，故
  runtime 共存风险低；集成时以 `manage(Arc::new(app))` 编过即为最终确认。
- 停机：窗口关闭 → `App::tasks_flag().store(true)` → 在途 `dispatch` 自然结束；WS 监听
  `abort` 对应 JoinHandle。

## 9. 实施分阶段（评审重排：先 demo 验证价值）

0. **Sample 桌面 demo（先行验收物）**：在 `sample/`（或 `sample-desktop/`）手写一个最小
   Tauri 工程，直接依赖 `oj` + `tauri`，跑通「webview fetch → 自定义协议 → App::dispatch →
   信封」与「sqlite 本地库」。先证明桌面形态有价值，再投完整子命令。
1. **`oj tauri new` 脚手架**：生成 `TauriCmd::New` 工程骨架（含 `oj-tauri` 依赖、自签名
   证书、config 模板），`dev`/`build` 透传 `cargo tauri`。
2. **`oj-tauri` 运行时**：`from_config` + `oj_dispatch` 桥接（JSON/REST 先行）。
3. **零改 fetch**：自定义协议处理器包装 `oj_dispatch`（取代 `ojFetch` 方案）。
4. **`oj tauri build` 串联**：`oj build` + 资源/证书/插件随包 + cert/plugins 存在性 fail-fast。
5. **WS 混合监听**：`App::ws_router()` + 127.0.0.1 随机端口 + `oj_ws_port` invoke。

## 10. 验收标准

- Sample 桌面 demo：`webview` 内 `fetch('oj://v1/api/...')` 调 oj handler 返回正确信封，
  **业务 REST 无监听端口**（`lsof` 仅见 WS 的 127.0.0.1 随机端口）。
- 证书/鉴权：经 `oj_dispatch` 自动注入 Bearer，handler 内 `auth` 模块正常取到身份；无
  `--no-cert` 后门。
- 静态站点 / `/blob/*` GET 经 `dispatch` 可达；`/blob` 大文件响应体**设内存上限**（见 §11）。
- WS：经混合监听可订阅广播。
- `oj tauri new demo && cd demo && cargo tauri dev` 端到端跑通。

## 11. 开放问题

- Q1（已决）：证书强制随包（脚手架生成自签名 keypair），不做 `--no-cert`。
- Q2：前端桥默认用 Tauri 自定义协议（零改 `fetch`）还是仍提供 `@oj/tauri-fetch` npm 包？
  倾向前者。
- Q3：多窗口下 `App` 状态共享与长任务隔离如何约定？
- Q4：WS 随机端口经 `invoke` 传给前端后，如何防本机其他进程连入（bind 127.0.0.1 + 一次性
  token？）。
- Q5：`/blob` 大文件经 `dispatch` 全量读入 `OjResp.body` 有 OOM 风险，是否设上限或改前端
  直连 WS/blob 专用监听？
- Q6：桌面证书过期/续期如何随 oj 版本升级接力（无 `oj-cert renew` 静默通道）。

## 12. 三路评审处置（架构 / 工程 / 产品）

| # | 来源 | 意见 | 处置 |
|---|------|------|------|
| 1 | 工程【严重】 | `oneshot` WS 不可行（app.rs:6-8） | **采纳**：§7 直接定混合监听，删 WS spike |
| 2 | 工程/架构【认可】 | `App` Send+Sync 已编译实证 | **采纳**：§8 删「需编译验证」 |
| 3 | 架构【严重】 | `--no-cert` 违反证书硬门禁 | **采纳**：§5 删 `--no-cert`，强制随包证书 |
| 4 | 产品【严重】 | `ojFetch` 破坏性改造 | **采纳**：§3/§4 改自定义协议零改 `fetch` |
| 5 | 产品【严重】 | 无端口安全收益被高估、缺场景证据 | **采纳并重述**：§1 把价值改为单二进制分发+同进程；保留无端口为伴随结果 |
| 6 | 产品【严重】 | DX 割裂、迁移阻力大 | **部分采纳**：§2 补迁移说明；未做完整迁移清单（YAGNI，demo 阶段补） |
| 7 | 产品【建议】 | 先 demo 后子命令 | **采纳**：§9 重排 Phase 0 sample demo 先行 |
| 8 | 架构【建议】 | 插件发现须显式设 `OJ_PLUGINS_DIR`→resolveResource | **采纳**：§6 显式化 |
| 9 | 架构【建议】 | WS 混合监听复用 `App::ws_router()`+本地 bind | **采纳**：§7 |
| 10 | 工程【建议】 | `/blob` 大文件响应体驻内存 | **采纳为开放问题**：§11 Q5 |
| 11 | 架构【建议】 | 可先仅做 `new` 脚手架 | **采纳**：§9 Phase 1 仅 `new` |
| 12 | 工程【认可】 | `dispatch` 已被 oj test 实证，spike(a) 冗余 | **采纳**：删 REST spike |

## 13. `oj tauri` 子命令生成器（详细设计）

> §3 / §9 已定调（生成器 + 伴生 `oj-tauri` crate，Phase 1 仅做 `new` 脚手架）。本节补齐**生成器落地的具体机制**，
> 供实现阶段照做。

### 13.1 CLI 形态

`oj` 主 CLI 新增 `Commands::Tauri`，其下挂子子命令（`oj/src/args.rs`）：

```bash
oj tauri new   <name> [--frontend <react|vanilla|none>] [--out <dir>]
oj tauri dev   [--config <path>] [--manifest <dir>]
oj tauri build [--config <path>] [--manifest <dir>]
```

- **`new`**：一次性生成 Tauri 工程骨架，并写死对 `oj-tauri` 的依赖（见 §13.3）。不跑 webview，
  纯代码生成。
- **`dev` / `build`**：仅**委托** `cargo tauri dev` / `cargo tauri build`（以生成工程为工作目录，
  见 §13.5）。`oj` 主 CLI 不链 Tauri，`tauri_cmd.rs` 只是「拼命令 + 切目录 + 透传」。

`new` 的参数：
- `<name>`：工程名（同时作为 `productName` / Cargo 包名 / Tauri identifier 的基底）。
- `--frontend`：前端模板（`react` / `vanilla` / `none`）。默认 `vanilla`（零构建，对齐
  sample-desktop 的 `public/` 静态资源玩法）。`react` 时把 `beforeDevCommand` / `frontendDist`
  指向相应脚手架（如 Vite）。
- `--out`：输出目录，默认当前目录下的 `<name>`。

### 13.2 `oj tauri new` 生成的工程骨架

与现有 `sample-desktop` **同构**（sample-desktop 就是本生成器的手写验证物，生成器落地后
应能被其取代 / 对齐）。产物树：

```
<name>/
├── Cargo.toml            独立 [workspace]；依赖 oj + oj-tauri + tauri
├── tauri.conf.json       withGlobalTauri: true；frontendDist 指向 public/；bundle.active 默认 true
├── config.yaml           oj 配置模板：base / 证书路径 / db.default=sqlite://<app>.sqlite
├── certs/                脚手架期 oj-cert 生成的自签名 keypair（.gitignore）
├── build.rs              tauri-build
├── icons/                默认图标（生成器内置一个最小 PNG，避免编译期缺图标失败）
├── public/               [--frontend vanilla] 前端静态资源（index.html + main.js 调 oj_dispatch）
├── src/
│   ├── main.rs           fn main() → oj_tauri::run()
│   └── lib.rs            薄壳：仅 `pub fn run()` 调 oj_tauri::run_with(config_dir, dir, base)
└── oj/
    └── src/              [业务模块]：如 sample/api.ts（或用户自带），含 manifest.yaml
```

> 注意：业务 `api.ts` **不随生成器内嵌**，由用户在 `oj/src/` 下放自己的模块；`oj tauri build`
> 期会先 `oj build`（src→dist）再随包，见 §13.5。

### 13.2.1 生成内容模板样例（预览）

> 分层要点（Tauri 机制约束）：`tauri::generate_context!()` 展开时读取的是**展开宏的那个 crate** 的
> `tauri.conf.json`，故 `.run(generate_context!())`、`.manage()`、`.generate_handler![]` 这些 Tauri 胶水
> **必须落在生成的 app crate**，不能塞进 `oj-tauri`。`oj-tauri` 只提供可复用的 oj 桥接原语
>（`build_app` / `dispatch` / 自定义协议处理器），不放 Tauri 运行胶水。

下面给出 `oj tauri new demo`（`<name>=demo`）会生成的各文件**近似内容**（节选、含注释，实现时照抄骨架）。

**① `Cargo.toml`（生成 app crate）**
```toml
[package]
name = "demo"
version = "0.1.0"
edition = "2024"

[workspace]                 # 独立 workspace，避免被 oj 根 workspace 吸进去

[[bin]]
name = "demo"
path = "src/main.rs"

[lib]
name = "demo_lib"
crate-type = ["staticlib", "cdylib", "rlib"]

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = "2"                                                  # 仅用于生成 crate 内的胶水（generate_context / State）
oj-tauri = { version = "0.1.31" }   # 由 new 按 oj 自身版本回填；持有 oj 桥接原语
oj = { path = "../../oj" }            # 业务代码要用 oj 类型（如 App）时显式加；否则可省略

[profile.dev]
debug = false              # 继承本仓库禁用 debug 产物的约定
incremental = false
```

**② `tauri.conf.json`**
```jsonc
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "demo",
  "version": "0.1.0",
  "identifier": "com.oj.desktop.demo",
  "build": {
    "beforeDevCommand": "../bin/oj serve -c src-tauri/config.yaml --app-path public",
    "devUrl": "http://localhost:5173",
    "frontendDist": "../public"
  },
  "app": {
    "withGlobalTauri": true,                 // 前端用 window.__TAURI__.core.invoke，无需 npm 包
    "windows": [{ "title": "demo", "width": 900, "height": 640 }],
    "security": { "csp": null }
  },
  "bundle": { "active": true, "targets": "app" }
}
```

**③ `config.yaml`（oj 配置模板）**
```yaml
server:
  host: 127.0.0.1              # beforeDevCommand 的 `oj serve` 静态服务器绑定地址
  port: 5173                   #   与 tauri.conf.json 的 devUrl 端口一致；仅 oj serve 读取
  base: /v1/api                # 即 api_prefix（旧键名 base 为 alias，代码里写 api_prefix）
  public_key_path: certs/public.pem
  certificate_path: certs/cert.jws
  console_log: true
db:
  default: "sqlite://demo.sqlite"   # sqlite 内置，无需插件
```
> dev 静态服务器用 `oj serve`（不再依赖 `python3 -m http.server`）；`--app-path ../public`
> 由 `beforeDevCommand` 传入，故 config 不写 `app_path`。`host/port` 仅 `oj serve` 读取，
> 生产 `.app` 走 `App::dispatch` 不监听端口，配置对其无副作用。

**④ `src/main.rs`**
```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() { demo_lib::run(); }
```

**⑤ `src/lib.rs`（生成的 app crate：Tauri 胶水 + 调 oj-tauri 原语）**
```rust
use std::path::PathBuf;
pub fn run() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));   // = 生成工程根
    // dev（debug_assertions 开）= 用 oj/src + ts=true 热重载；release = 用 oj/dist + ts=false 预构建
    let dir = if cfg!(debug_assertions) {
        manifest.join("oj/src")
    } else {
        manifest.join("oj/dist")
    };
    let base = "/v1/api".to_string();
    app_glue::start(manifest, dir, base);   // 见下：构造 App、注册命令、run(generate_context!())
}

mod app_glue {
    use oj_tauri::{build_app, dispatch, OjResp};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct AppState { app: Arc<oj::app::App> }

    #[tauri::command]
    async fn oj_dispatch(
        state: tauri::State<'_, AppState>,
        method: String, uri: String,
        headers: HashMap<String, String>, body: Option<String>,
    ) -> Result<OjResp, String> {
        dispatch(&state.app, method, uri, headers, body).await   // 复用 oj-tauri 桥接
    }

    pub fn start(config_dir: PathBuf, dir: PathBuf, base: String) {
        let app = tauri::async_runtime::block_on(async {
            build_app(&config_dir, dir, base).await            // 复用 oj-tauri：from_config 同 oj serve
        }).expect("build oj App");
        tauri::Builder::default()
            .manage(AppState { app: Arc::new(app) })
            .invoke_handler(tauri::generate_handler![oj_dispatch /* , oj_ws_port (Phase 5) */])
            // .register_uri_scheme_protocol("oj", oj_tauri::protocol_handler)  // Phase 3 零改 fetch
            .run(tauri::generate_context!())                   // ⚠️ 必须在生成 crate 内展开
            .expect("run tauri app");
    }
}
```

**⑥ `oj-tauri/src/lib.rs`（伴生运行时 crate：oj 桥接原语，不放 Tauri 运行胶水）**
```rust
use oj::app::App;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct OjResp { pub status: u16, pub headers: HashMap<String, String>, pub body: String }

/// 等价于 oj serve 的装配：读 config → from_config（含证书强制门禁 / sqlite / 路由表）。
pub async fn build_app(config_dir: &Path, dir: PathBuf, base: String)
    -> Result<Arc<App>, String> {
    let cfg = only_js::config::load_from(config_dir, Some("config.yaml"))?;
    let base = cfg.server.api_prefix.clone();      // 以 config 为准
    let ts = cfg!(debug_assertions);               // dev 用 src，release 用 dist
    let app = App::from_config(cfg, config_dir, dir, base, ts, false, None).await?;
    Ok(Arc::new(app))
}

/// oj_dispatch 的核心：把 JS 参数拼成 axum Request → App::dispatch → 拆 OjResp。
pub async fn dispatch(app: &Arc<App>, method: String, uri: String,
                     headers: HashMap<String, String>, body: Option<String>)
                     -> Result<OjResp, String> {
    let mut builder = axum::http::Request::builder().method(method.as_str()).uri(uri.as_str());
    for (k, v) in &headers { builder = builder.header(k.as_str(), v.as_str()); }
    let req = builder.body(axum::body::Body::from(body.unwrap_or_default()))
        .map_err(|e| format!("build request: {e}"))?;
    let resp = app.dispatch(req).await;
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await
        .map_err(|e| format!("read body: {e}"))?;
    let mut hm = HashMap::new();
    for (k, v) in parts.headers.iter() {
        hm.insert(k.as_str().to_string(), v.to_str().unwrap_or("").to_string());
    }
    Ok(OjResp { status: parts.status.as_u16(), headers: hm,
                body: String::from_utf8_lossy(&bytes).into_owned() })
}

// Phase 3（零改 fetch）：自定义协议处理器，对每条请求调 dispatch 并回写 Response。
// pub fn protocol_handler(/* req, responder */) { /* 内部调 dispatch，把 OjResp 写回 */ }

// Phase 5（WS）：复用 App::ws_router() 起 127.0.0.1 随机端口；oj_ws_port 命令把端口透给前端。
```

**⑦ `public/index.html`**
```html
<!doctype html><html lang="zh"><head><meta charset="utf-8"/><title>demo</title></head>
<body>
  <h2>oj × Tauri — 无端口桌面后端</h2>
  <button id="call">调用 GET /v1/api/sample/</button>
  <pre id="out">点击按钮…</pre>
  <script src="./main.js"></script>
</body></html>
```

**⑧ `public/main.js`（vanilla，IPC 方式 —— 已用 sample-desktop 验证）**
```js
const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke || tauri?.invoke;   // ⚠️ v2：invoke 在 .core 下
document.getElementById("call").addEventListener("click", async () => {
  const resp = await invoke("oj_dispatch",
    { method: "GET", uri: "/v1/api/sample/", headers: {}, body: null });
  document.getElementById("out").textContent = "HTTP " + resp.status + "\n\n" + resp.body;
});
```
```js
// —— Phase 3 零改 fetch 变体：自定义协议 oj:// 伪装同源，业务代码直接 fetch，无需 invoke ——
// const r = await fetch("oj://v1/api/sample/");
// const env = await r.json();   // 直接拿到 {code,msg,data}
```

**⑨ `oj/src/sample/api.ts` + `oj/src/sample/manifest.yaml`（用户自带业务；生成器不放，给示例）**
```ts
// oj/src/sample/api.ts  —— 路由 /v1/api/sample/
export default {
  get() { return json.ok({ hello: "oj tauri", now: Date.now() }); },
};
```
```yaml
# oj/src/sample/manifest.yaml  —— 模块根必须有 manifest
name: "sample"
desc: "示例业务模块"
version: "0.1.0"
```

**⑩ `icons/icon.png`** —— 生成器内置一个最小有效 PNG（Tauri 编译期 `generate_context!()` 要读，缺了编译不过；也可后续 `oj tauri build` 前替换为正式图标）。

> 以上模板与现有 `sample-desktop/` 工程**同构**（sample-desktop 即本生成器的手写验证物）。实现阶段先让
> `new` 产出的工程 == sample-desktop 的自动化版本，再增量补 Phase 3（零改 fetch）/ Phase 5（WS）。

### 13.3 依赖注入与版本

- `Cargo.toml` 写 `oj-tauri = { version = "x.y.z" }`（与当前 `oj` 同版本线，由 `oj tauri new`
  按 `oj` 自身的 `CARGO_PKG_VERSION` 回填，避免漂移）。
- `oj-tauri` 是独立 crate（在 workspace 或 crates.io 发布），依赖 `oj` + `tauri`，**持有全部
  Tauri 运行时逻辑**（见 §13.4），使 `oj` 主 CLI 不膨胀。
- `oj` 主 CLI 仅在 `tauri_cmd.rs` 里拼命令，**不**加 `tauri` 依赖。

### 13.4 `oj-tauri` 运行时 crate 内部设计

`oj_tauri::run_with(config_dir, dir, base)` 在 Tauri `setup` 里干这些（与 sample-desktop 的
`lib.rs` 一致，但作为可复用库）：

1. **构造 App**：`oj::app::App::from_config(cfg, config_dir, dir, base, ts, false, None).await`。
   - `dev`（ts=true）：`dir` 指向 `oj/src`（按需转译 TS，热重载）。
   - `build`（ts=false）：`dir` 指向 `oj/dist`（预构建 JS，不转译，按锁聚合）。
2. **注入状态**：`let app = Arc::new(app); tauri::Builder::manage(app.clone())`。`App: Send+Sync+'static`
   （`impl ClientTransport` 已编译实证），`manage(Arc<App>)` 直接可用。
3. **IPC 桥 `oj_dispatch`**（JSON/REST 先行）：`#[tauri::command]` 把 `{method,uri,headers,body}`
   拼成 `axum::http::Request` → `APP.dispatch(req).await` → 拆 `OjResp{status,headers,body}`。
   - 这是 sample-desktop 已验证的路径（`examples/verify_dispatch` 等同路径回归）。
4. **零改 fetch 自定义协议**（Phase 3）：注册自定义 URI scheme（如 `oj://` 或 `http://oj.local`），
   协议处理器内部调 `oj_dispatch`，使前端 `fetch('oj://v1/api/...')` **原样可用**（§4、§12 #4）。
   - Tauri v2 自定义协议机制：`tauri::Builder::register_uri_scheme_protocol`（或等价的 v2 资产/
     IPC 机制）；处理器对每个请求构造 `Request` → `dispatch` → 回写 `Response`。
5. **WS 混合监听**（Phase 5）：复用 `App::ws_router()` 起本机 `127.0.0.1` 随机端口 axum；
   前端经 `invoke('oj_ws_port')` 拿端口后直连 `ws://127.0.0.1:<port>/...`（§7）。REST 仍走
   `dispatch`，该监听仅服务 WS，不暴露业务 REST、不暴露局域网。
6. **证书强制**：`from_config` 已 fail-fast（`oj/src/app.rs:951`）；脚手架期把自签名 keypair
   写进 `certs/`，`config.yaml` 指向之，**无 `--no-cert` 后门**（§5、§12 #3）。
7. **生命周期**：窗口关闭 → `App::tasks_flag().store(true)`；WS 监听 `abort` JoinHandle。
   **不调用 `serve_graceful`**（业务 API 无监听）。

### 13.5 `dev` / `build` 委托机制

- `oj tauri dev|build` = `std::process::Command::new("cargo").arg("tauri").arg("dev"|"build")`
  以**生成工程目录**为 `current_dir` 执行；`config` / `manifest` 等参数透传。
- `new` 生成的工程自带 `tauri.conf.json`，故委托后 Tauri 按常规流程工作；`oj` 主 CLI 不碰
  Tauri 内部。
- `oj tauri build` **额外前置**：先 `oj build -d <oj/src> -o <oj/dist>`（src→dist，不转译、按锁
  聚合），再把 `dist` / `config.yaml` / `certs/` / 可选插件 cdylib 作为资源随包，`cargo tauri build`
  期经 `tauri::Manager::resolve_resource` 解压到 `config_dir`（§6）。

### 13.6 `oj tauri build` 串联的 fail-fast 校验

打包前显式校验，缺一样就报错退出（不让用户带着坏配置出包）：
- `config.yaml` 存在且可解析；
- `certs/public.pem` + `certs/cert.jws` 存在（证书强制，§5）；
- 若配了插件，`OJ_PLUGINS_DIR` / `plugins_dir` 指向的 `bin/plugins/<host-triple>/*` 存在且与
  `ABI_VERSION` 严格相等、`host-triple` 一致（§6）。

### 13.7 与现有 sample-desktop 的关系

- `sample-desktop/` 是 **Phase 0 手写验证物**：直接依赖 `oj` + `tauri`（不经 `oj-tauri` 库），
  证明「无端口 + JSON/REST + sqlite」跑得通。
- 生成器落地后，`oj tauri new demo` 的产物应与 `sample-desktop` **同构**（同一套 `oj-tauri`
  运行时逻辑）。sample-desktop 可保留作「手写对照 / 回归基准」，或在实际子命令可用后退居示例。
- 生成器实现阶段建议先让 `new` 产出的工程 `= sample-desktop` 的自动化版本，再增量加自定义协议
  （Phase 3）、WS（Phase 5）。

### 13.8 实施顺序（细化 §9）

0. ✅ **sample 手写 demo**（已完成：`sample-desktop/`，DEV-GUIDE 见 `sample-desktop/DEV-GUIDE.md`）。
1. `oj tauri new` 脚手架：产出 §13.2 骨架 + 自动 `oj-cert` 证书 + 回填 `oj-tauri` 版本。
2. `oj-tauri` 运行时库：`from_config` + `oj_dispatch`（JSON/REST，复用 sample 验证过的路径）。
3. 零改 fetch：自定义协议处理器包装 `oj_dispatch`。
4. `oj tauri build` 串联：`oj build` + 资源/证书/插件随包 + fail-fast 校验。
5. WS 混合监听：`App::ws_router()` + 127.0.0.1 随机端口 + `oj_ws_port` invoke。

### 13.9 开放问题（更新 §11）

- 新增 **Q7**：`oj-tauri` 作为独立 crate 发布（workspace 内 `path` 依赖 vs crates.io）；
  `oj tauri new` 回填版本策略（`oj` 自身版本 vs 独立发版）。
- 新增 **Q8**：自定义协议在 Tauri v2 的具体机制（`register_uri_scheme_protocol` 弃用路径 vs
  新资产/IPC 协议）；需 spike 确认 v2 下零改 `fetch` 的最优注册方式。
- 保留 §11 的 Q2–Q6。
