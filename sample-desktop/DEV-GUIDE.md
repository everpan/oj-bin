# sample-desktop 开发手册（人话版）

> 给刚接手这个桌面工程的新人看的。不讲虚的，只讲"它是什么、怎么跑、怎么改、坑在哪"。
> 配套设计文档：`docs/plans/2026-09-29-tauri-subcommand-design.md`（讲为什么要这么干）。

---

## 0. 这玩意儿到底是啥

一句话：**把 oj 后端塞进一个桌面应用里，前端不暴露任何 TCP 端口，靠 Tauri 的 IPC 把请求送进 oj 在进程内处理。**

- **oj** 是咱们的低代码后端框架（JS/TS 写 handler，跑在 Rust 嵌的 V8 里）。
- 正常用法是 `oj serve` 起一个 HTTP 服务，前端走网络调它。
- 这里换了个玩法：**不起端口**。桌面应用自己就是"服务器"——前端在窗口里，通过 Tauri 的 IPC 机制（本质是一套进程内通信）直接调 oj 的 `App::dispatch`，oj 在内存里把请求路由、跑 handler、返回 `{code,msg,data}` 信封。

对外看，这台机器上**没有任何业务端口在监听**（用 `lsof` 看不到），但桌面应用照样能增删改查。这就是设计文档里说的"模式 C：无端口，App::dispatch 直调"。

**新人记住一句话**：前端 ↔ 后端之间没有网络，只有 Tauri IPC 这一道桥。

---

## 1. 先把环境备齐

### 1.1 必装

| 东西 | 干嘛的 | 装法 |
|------|--------|------|
| Rust 工具链 | 编译 Rust | [rustup](https://rustup.rs) |
| **tauri-cli**（v2） | 打包 / 起桌面应用，**必须用这个**，别裸跑二进制 | `cargo install tauri-cli --version "^2"` |
| 系统 WebView | 窗口里渲染前端页面 | macOS 自带；Linux 需 `webkit2gtk-4.1`；Windows 需 WebView2 |
| `oj` CLI 二进制（`bin/oj`） | **dev 模式下由 `oj serve` 托管前端静态资源**，不再依赖 python3 | 仓库根 `cargo xtask bin` 产出 |
| `node`（可选） | 真要做前端工程化时才用 | 任意版本 |

### 1.2 生成自签名证书（**第一次必须做**）

oj 的设计红线：**证书强制必配**，缺一个就直接启动失败（fail-fast），没有后门。
仓库里已经提供了生成工具 `oj-cert`，在**仓库根目录**跑：

```bash
cd <仓库根>/oj-bin
cargo run -p oj-cert -- gen -o sample-desktop/src-tauri/certs
```

会生成三个文件到 `sample-desktop/src-tauri/certs/`：
- `private.pem` —— 私钥（别提交、别泄露）
- `public.pem` —— 公钥
- `cert.jws` —— 证书（JWS 格式）

`config.yaml` 里写死了这两个路径（`certs/public.pem`、`certs/cert.jws`），所以**路径别乱改**。

### 1.3 一个项目约定（很重要）

这个仓库**只用 release 构建，禁用 debug 构建**（根 `Cargo.toml` 里 `[profile.dev]` 把 `debug` 关了，省磁盘）。
所以你后面看到的命令基本都是 `cargo ... --release` / `cargo tauri build`，别手滑用裸 `cargo build` / `cargo run`。

---

## 2. 目录结构，逐个文件讲

```
sample-desktop/
├── DEV-GUIDE.md          ← 你正在看的这份
├── README.md             快速开始（5 分钟能跑起来那版）
├── public/               前端静态资源（纯 vanilla JS，不要打包工具也能跑）
│   ├── index.html        窗口里显示的页面
│   └── main.js           调 oj 的前端脚本（核心：window.__TAURI__.core.invoke）
└── src-tauri/            Tauri 工程（Rust 这边）
    ├── Cargo.toml        注意：它自己是一个独立 workspace（见下文）
    ├── tauri.conf.json   Tauri 配置（withGlobalTauri、frontendDist、bundle 等）
    ├── config.yaml       oj 的配置（api 前缀、证书路径、sqlite）
    ├── certs/            证书（生成，见 1.2；已被 .gitignore 忽略）
    ├── build.rs          调 tauri-build
    ├── icons/            应用图标（Tauri 编译期要读，缺了编译不过）
    └── src/
        ├── main.rs       入口：`fn main()` 直接调 `oj_desktop_demo_lib::run()`
        ├── lib.rs        ★核心★：构造 oj 的 App、注册 oj_dispatch 命令
        ├── sample/       oj 的一个业务模块
        │   ├── manifest.yaml   模块清单（**必须有**，否则该模块不认）
        │   └── api.ts          oj handler（→ 路由 /v1/api/sample/）
        └── ...（可能还有其他文件）
    └── examples/
        └── verify_dispatch.rs  后端回归验证脚本（直连 App::dispatch）
```

### 2.1 为什么 `src-tauri/Cargo.toml` 是独立 workspace

文件里有一行空的 `[workspace]` 表。这是**故意的**：仓库根是个大 workspace（oj / only-js / server / plugins…），如果不声明独立 workspace，这个 Tauri 工程会被根 workspace 吸进去，导致各种路径和依赖解析乱套。加了 `[workspace]` 之后它自成一体，只通过 path 依赖引用根仓库的 `oj` 和 `only-js`：

```toml
oj = { path = "../../oj" }
only-js = { path = "../.." }   # “..” 到 oj-bin 根目录，那里就是 only-js 这个 crate
```

（`../..` 从 `sample-desktop/src-tauri/` 往上两级 = `oj-bin`，正好是 only-js 的根 crate。）

### 2.2 `tauri.conf.json` 几个关键开关

```jsonc
{
  "build": {
    "beforeDevCommand": "../bin/oj serve -c src-tauri/config.yaml --app-path public",
    "devUrl": "http://localhost:5173",     // dev 模式前端从这取
    "frontendDist": "../public"            // release 模式把前端打进 .app 的目录
  },
  "app": {
    "withGlobalTauri": true,   // 关键：把 Tauri API 挂到 window.__TAURI__（前端不用 import 包）
    "windows": [ { "title": "...", "width": 900, "height": 640 } ]
  },
  "bundle": {
    "active": true,   // 出 .app 包；设 false 就不打包（那你就没法双击运行）
    "targets": "app"  // macOS 只出 .app（不出 dmg）
  }
}
```

- **`withGlobalTauri: true`**：让前端能直接 `window.__TAURI__` 拿到 Tauri 能力，**不用 npm 装 `@tauri-apps/api`**。但这也带来一个坑（见第 8 节）——v2 的 `invoke` 在 `window.__TAURI__.core.invoke`，不是 `window.__TAURI__.invoke`。
- **`frontendDist: "../public"`**：release 打包时把 `public/` 的内容塞进 `.app`。所以你改了 `public/` 后**必须重新 `cargo tauri build`** 才能生效。
- **`beforeDevCommand` 用 `oj serve` 托管前端（不再用 `python3 -m http.server`）**：注意 Tauri 执行 `beforeDevCommand` 的工作目录是**项目根 `sample-desktop/`**（即 `src-tauri` 的父目录），所以二进制写 `../bin/oj`（→ 仓库根 `bin/oj`）、配置文件写 `src-tauri/config.yaml`、静态目录写 `public`，不要写成 `../../`。地址/端口来自 `config.yaml` 的 `server.host/port`。跑之前先 `cargo xtask bin` 产出 `bin/oj`。

---

## 3. oj 后端在这套里是怎么跑的

### 3.1 核心就一个 `App`

正常 `oj serve` 会构造一个 `App`、再起 axum 监听端口。这里我们**只构造 `App`，不监听端口**。

构造在哪？`src/lib.rs` 的 `run()` 里：

```rust
let cfg = only_js::config::load_from(&manifest, Some("config.yaml"))?;  // 读 oj 配置
let base = cfg.server.api_prefix.clone();   // "/v1/api"
let dir  = manifest.join("src");           // api 根目录（放业务模块的地方）

// 关键一行：和 oj serve 用的是同一个装配函数，只是不监听端口
let app = tauri::async_runtime::block_on(async {
    App::from_config(cfg, &manifest, dir, base, /*ts=*/ true, /*fixtures=*/ false, None)
})?;
```

`App::from_config` 干了这些事（和 `oj serve` 一模一样）：
1. 读配置、开 sqlite 库、加载业务模块（扫描 `src/` 下所有带 `manifest.yaml` 的目录）。
2. 建路由表（把每个 `api.ts` 挂到对应路径）。
3. 校验证书（缺了直接报错退出）。
4. 准备好一批 V8 runtime 池（热启动用）。

> 上面的代码片段是化简版。真实 `src/lib.rs` 把这段包在 `.setup(|app| { ... })` 闭包里，并做
> **dev / release 分叉**：dev（debug）用 `CARGO_MANIFEST_DIR` 取 `config_dir`/`src`，release
> 用 `app.path().resource_dir()`（打进 `.app/Contents/Resources` 的副本）；且 release 下会把
> `db.default` 重定向到 `app.path().app_data_dir()`（每用户私有可写目录，不进只读 bundle）。
> 详见第 9 节两条「已解决」项。

### 3.2 `App::dispatch` = "不监听端口的 HTTP 服务"

`App` 有个方法 `dispatch(req: Request) -> Response`。它内部走的**正是 HTTP 服务器那条完整管线**：路由匹配 → 前置处理 → 鉴权/租户 → 证书 GET 门禁 → 跑 handler → 写回信封。**唯一区别**是它不发 TCP，而是你直接把 `Request` 塞进去、拿 `Response` 出来。

> 记住：WS（WebSocket）在 `dispatch` 里**不跑帧循环**（这是 oj 的设计边界，详见第 9 节），所以本工程目前只验证 JSON/REST，不带 WS。

### 3.3 oj 的接口（handler）怎么写

业务代码在 `src/sample/api.ts`。oj 的路由是**目录镜像**：文件位置决定 URL。

```
src/sample/api.ts   →   GET/POST… /v1/api/sample/
src/sample/ping/api.ts  →  /v1/api/sample/ping
```

**写法有硬性约定**（踩过坑）：必须是 `export default { 方法名() {} }` 这种"默认导出一个对象"的形式，**不是** `export function get()`：

```ts
// ✅ 正确写法
export default {
  get() {
    return json.ok({ hello: "oj tauri", now: Date.now() });
  },
};
```

`json.ok(...)` 会自动包成 `{ code: 0, msg: "ok", data: {...} }` 信封。

每个模块根目录还**必须**有 `manifest.yaml`（哪怕最简）：

```yaml
name: "sample"
desc: "一句话描述"
version: "0.1.0"
```

没有 `manifest.yaml`，那个目录就不会被当成业务模块，路由就是 0 条。

### 3.4 启动日志怎么看

应用起来后会在 stderr 打两行（正常现象，说明后端装好了）：

```
module sample v0.1.0 — oj × Tauri 演示：无端口 App::dispatch 直调
routes: 1 method-row(s), 1 pattern(s), 1 api file(s)
```

- `routes: 0` 就是有问题（handler 没被识别，见第 8 节）。
- 如果连 `module sample` 都没出现，说明 `App::from_config` 直接 panic 了（多半是证书/路径）。

---

## 4. 前端怎么调后端（IPC 这一道桥）

### 4.1 数据流

```
用户在窗口点按钮
   │
   ▼
main.js: window.__TAURI__.core.invoke("oj_dispatch", { method, uri, headers, body })
   │  ← Tauri 进程内 IPC，不是网络
   ▼
src/lib.rs 的 #[tauri::command] oj_dispatch(...)
   │  把参数拼成 axum::http::Request
   ▼
app.dispatch(req)        ← 就是第 3.2 节那个"不监听端口的 HTTP 服务"
   │
   ▼
oj 路由 → 跑 sample/api.ts 的 get() → 返回信封 {code,msg,data}
   │
   ▼
oj_dispatch 把 Response 拆成 { status, headers, body } 回给前端
   │
   ▼
main.js 把 body 显示到页面上
```

### 4.2 `oj_dispatch` 命令（后端的"桥"）

`src/lib.rs` 里就这么一点：

```rust
#[tauri::command]
async fn oj_dispatch(
    state: tauri::State<'_, AppState>,   // 拿之前构造好的 App
    method: String,
    uri: String,
    headers: HashMap<String, String>,
    body: Option<String>,
) -> Result<OjResp, String> {
    // 1. 把前端传来的参数拼成标准的 HTTP Request
    let req = axum::http::Request::builder()
        .method(method.as_str()).uri(uri.as_str())
        .body(axum::body::Body::from(body.unwrap_or_default()))?;

    // 2. 丢给 oj 的 dispatch（进程内路由 + 完整管线）
    let resp = state.app.dispatch(req).await;

    // 3. 把 Response 拆成 {status, headers, body} 回给前端
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await?;
    Ok(OjResp { status: parts.status.as_u16(), headers: ..., body: ... })
}
```

`AppState` 就是个壳，里面抱着那个 `App`：

```rust
pub struct AppState { pub app: App }
```

构造好之后 `tauri::Builder::default().manage(AppState { app })` 注册进去，命令里就能用 `state.app` 拿它。

### 4.3 前端长这样（`public/main.js`）

```js
const tauri = window.__TAURI__;
// ⚠️ v2 的 invoke 在 .core 下面！不是 window.__TAURI__.invoke
const invoke = (tauri?.core?.invoke) || (tauri?.invoke);

document.getElementById("call").addEventListener("click", async () => {
  const resp = await invoke("oj_dispatch", {
    method: "GET",
    uri: "/v1/api/sample/",   // 注意要带完整前缀 /v1/api
    headers: {},
    body: null,
  });
  out.textContent = "HTTP " + resp.status + "\n\n" + resp.body;
});
```

**新人最容易栽的两个点**：
1. `invoke` 路径写错 → 报 `ERROR: {}`（第 8 节详解）。
2. `uri` 必须带完整前缀 `/v1/api/...`，`dispatch` 是按完整路径匹配的。

---

## 5. 编译与运行（重点：别踩坑）

### 5.1 出 release 包（推荐，正经用法）

```bash
cd sample-desktop/src-tauri
cargo tauri build
open target/release/bundle/macos/oj-desktop-demo.app
```

`cargo tauri build` 会：编译 release → 把 `public/` 前端打进 `.app` → 生成 `target/release/bundle/macos/oj-desktop-demo.app`。
`open` 那个 `.app` 才会正常显示窗口。

### 5.2 开发模式（改前端能热更，但它是 debug 构建）

```bash
cd sample-desktop/src-tauri
cargo tauri dev
```

dev 模式会用 `beforeDevCommand`（**`oj serve`** 托管 `public/`，监听 `127.0.0.1:5173`）把前端跑起来，前端改动即时生效，不用重打包。
`oj serve` 的地址/端口来自 `config.yaml` 的 `server.host/port`；它用仓库根的 `bin/oj`，所以**先 `cargo xtask bin` 产出它**。
**注意**：dev 是 debug 构建，和本仓库"只用 release"的约定相悖，只建议本地调试前端时用，别拿它当正式产物。
**debug 构建会自动弹出 WebKit DevTools**（real devtool，`src/lib.rs` 在 `cfg(debug_assertions)` 下调 `open_devtools()`），可直接 Inspect 桌面前端；release 构建无此窗口。

### 5.3 🚫 千万别干这件事

**不要**直接跑二进制，也**不要** `cargo run --release`：

```bash
./target/release/oj-desktop-demo        # ❌ 白屏
cargo run --release                     # ❌ 同样白屏
```

原因：macOS 上 WKWebView 需要应用处在正规的 `.app` 包上下文里才能正常渲染页面。裸二进制不在 `.app` 里，窗口能弹出来但**一片空白**。这是 macOS 的机制，不是你代码错。所以一律走 5.1 / 5.2。

---

## 6. 怎么验证"真的跑通了"

### 6.1 肉眼验证（最直观）

打开 `.app` 后，点窗口里的按钮「调用 GET /v1/api/sample/」，页面应显示：

```
HTTP 200

{"code":0,"msg":"ok","data":{"hello":"oj tauri","mode":"no-port","via":"App::dispatch","now":...}}
```

### 6.2 后端自动化验证（不用点界面）

仓库里给了个脚本，绕过前端、直接打 `App::dispatch`，验证后端没问题：

```bash
cd sample-desktop/src-tauri
cargo run --example verify_dispatch --release
```

输出里有：

```
IPC-BACKEND status=200
IPC-BACKEND body={"code":0,"msg":"ok","data":{"hello":"oj tauri",...}}
IPC-BACKEND OK: App::dispatch 返回预期信封
```

这个脚本和 `oj_dispatch` 命令走的是**完全相同的调用路径**（同样的 `App::from_config` + `App::dispatch`），只是少了 Tauri 那层壳。

### 6.3 验证"无端口"（设计卖点）

```bash
lsof -iTCP -sTCP:LISTEN | grep -i oj-desktop
# 应该什么都没有 —— 业务 API 没有监听任何端口
```

### 6.4 看启动日志

运行 `.app` 时把它的 stderr 抓出来看（直接跑包内二进制、后台启动再读日志即可）：

```bash
cd sample-desktop/src-tauri
./target/release/bundle/macos/oj-desktop-demo.app/Contents/MacOS/oj-desktop-demo > /tmp/oj.log 2>&1 &
sleep 5
cat /tmp/oj.log     # 看 module sample / routes: 1 / 有没有 panic
```

---

## 7. 手把手：加一个新的接口

假设要在 `sample` 模块下加一个 `GET /v1/api/sample/ping`，返回 `"pong"`。

**第 1 步**：新建文件 `src/sample/ping/api.ts`：

```ts
export default {
  get() {
    return json.ok({ pong: true });
  },
};
```

（同一个模块，不用改 `manifest.yaml`。新模块才需要在模块根加 `manifest.yaml`。）

**第 2 步**：前端想调就改 `public/main.js` 里的 `uri`：

```js
uri: "/v1/api/sample/ping",
```

**第 3 步**：重新出包（改了 api.ts 也走这个，因为它在 `src/` 里随 Rust 构建；改了 public/ 更要重打包）：

```bash
cargo tauri build
open target/release/bundle/macos/oj-desktop-demo.app
```

**第 4 步**：验证。可以先用 `cargo run --example verify_dispatch --release` 把 uri 临时改一下打到 `/v1/api/sample/ping` 看返回，再点界面。

> 想加 POST、带参数、连 sqlite ？看 `docs/devkit/api-manual.md`（oj 的 JS API 权威文档）和 `sample/src/` 里那些更完整的例子（order、auth_demo 等）。本工程只是最小演示。

---

## 8. 排错锦囊（我们踩过的坑，全在这）

### 8.1 点按钮报 `ERROR: {}`
**十有八九是 `invoke` 路径写错。** Tauri v2 全局 API 里 `invoke` 在 `window.__TAURI__.core.invoke`，不是 `window.__TAURI__.invoke`。旧写法取到 `undefined`，一调用就抛错，`JSON.stringify` 出来正好是个空对象 `{}`。
**修法**：`const invoke = window.__TAURI__.core.invoke`（本工程 `main.js` 已修好，并加了 `e.message` 显示真实错误）。

### 8.2 窗口一片空白（白屏）
**没走 `.app` 包。** 你是直接跑了二进制 / `cargo run --release`。见 5.3。
**修法**：`cargo tauri build` 出 `.app` 再 `open`。

### 8.3 启动日志 `routes: 0 method-row(s)`
后端没识别到任何接口。常见原因：
- `api.ts` 写成了 `export function get()`（错），要 `export default { get() {} }`（对）。见 3.3。
- 模块根目录缺 `manifest.yaml`。见 3.3。
- 文件不叫 `api.ts`，或不在模块目录下。

### 8.4 启动直接 panic：`module 'xxx' missing manifest.yaml`
同上，补 `manifest.yaml` 即可。

### 8.5 启动 panic：`load config.yaml` / 证书相关
- 证书没生成：回 1.2 跑 `oj-cert gen`。
- `config.yaml` 里证书路径和 `certs/` 实际文件名对不上：别手改路径，默认就好。

### 8.6 编译报 `no field base on type ServerCfg` / `Config::load_from` 找不到
这是写 Rust 时的 API 误用，跟新人关系不大，但提一句：oj 的配置字段是 `api_prefix`（旧键名 `base` 只是别名，配文件能用，代码里要写 `api_prefix`）；`load_from` 是 `config` 模块的**自由函数**（`only_js::config::load_from`），不是 `Config` 的关联方法。

### 8.7 想确认有没有监听端口
见 6.3。没有就是正常（无端口正是目标）。

---

## 9. 已知边界（别指望它现在就能干这些）

按设计文档，这是 **Phase 0 的 PoC（概念验证）**，只验证"无端口 + JSON/REST 能跑通"。以下还没做：

- **WebSocket 没接**：`App::dispatch` 走的 `oneshot` 不跑 WS 帧循环。真要 WS，设计文档给的方案是"混合监听"——REST 继续无端口，WS 在本地 `127.0.0.1` 开一个最小端口。本工程没实现。
- **前端零改 `fetch` 的自定义协议**：设计文档里有个更省的玩法（Tauri 自定义协议伪装同源，业务代码直接 `fetch` 不用改）。本工程用的是更直白的 `invoke` 方案，已经够用。
- **`oj tauri` 子命令生成器**：已经过设计（见设计文档 §13），规划为 `oj tauri new|dev|build`——
  `new` 一键生成与本工程同构的桌面骨架（自动 `oj-cert` 证书、回填 `oj-tauri` 依赖），`dev`/`build`
  委托 `cargo tauri`。`oj-tauri` 是独立运行时 crate，持有全部 Tauri 桥接逻辑，使 `oj` 主 CLI 不链
  Tauri。本 `sample-desktop` 就是该生成器的**手写验证物**，子命令落地后产物应与本工程同构。
- **blob 二进制返回**：`oj_dispatch` 现在只回文本 body；二进制（文件下载等）是后续项（设计文档 Q5）。
- **打包后资源定位（已解决）**：`cargo tauri build` 经 `tauri.conf.json` 的 `bundle.resources`
  （`["config.yaml","certs","src"]`）把配置/证书/业务源码随包打进
  `.app/Contents/Resources`；`src/lib.rs` 的 `setup` 在 release 用
  `app.path().resource_dir()` 取资源目录、dev 用 `CARGO_MANIFEST_DIR`（dev 下
  `resource_dir()` 返回 `target/debug` 无 config，必须分叉）。**打好的 `.app` 拷到别的机器
  直接 `open` 即可跑，不再依赖构建机路径。**（设计文档 §6 写的是 `resolve_resource`，Tauri
  2.12 实际是 `app.path().resource_dir()`，实现以其为准。）
- **sqlite 安全/读写（已解决）**：release 下 `src/lib.rs` 把 `db.default` 重定向到每用户私有
  可写目录 `app.path().app_data_dir()`（macOS =
  `~/Library/Application Support/com.oj.desktop.demo`），**绝不落在只读 bundle 内**——
  满足 macOS 打包对数据库文件的读写要求。该目录由 Tauri 创建（本机实测 mode 755），但其父
  `~/Library/Application Support` 在 macOS 默认即 700（仅属主可进入），故其他用户无法读
  到其中的 `app.sqlite`，安全性由平台默认目录权限保证。dev 仍用 config 里的本地 `app.sqlite`。（已据本机 `cargo tauri build` 出包后实测：`routes: 1`、`app.sqlite` 落于该目录。）

---

## 10. 跟设计文档的关系

- 设计文档：`docs/plans/2026-09-29-tauri-subcommand-design.md`
  - 讲清了三种集成模式（侧车 / 单进程内嵌 / 无端口直调），以及为什么选无端口。
  - 含三路专家评审（架构 / 工程 / 产品）的处置表，本工程的很多取舍来自那里（比如强制证书、零改 fetch 方案、先 demo 验证价值）。
- 本手册对应的就是设计文档里的 **Phase 0**：用 `sample-desktop` 这个最小 demo 把"无端口能跑通"验证掉。

---

## 11. 常用命令速查

```bash
# 生成证书（首次 / 证书丢失时）
cargo run -p oj-cert -- gen -o sample-desktop/src-tauri/certs

# 出 release 包并打开（正经用法）
cd sample-desktop/src-tauri && cargo tauri build
open target/release/bundle/macos/oj-desktop-demo.app

# 开发模式（改前端热更；debug 构建，仅本地调试）
cd sample-desktop/src-tauri && cargo tauri dev

# 后端回归验证（不打界面，直连 App::dispatch）
cd sample-desktop/src-tauri && cargo run --example verify_dispatch --release

# 确认无端口监听
lsof -iTCP -sTCP:LISTEN | grep -i oj-desktop   # 应无输出

# 看启动日志（直接跑包内二进制）
cd sample-desktop/src-tauri
./target/release/bundle/macos/oj-desktop-demo.app/Contents/MacOS/oj-desktop-demo > /tmp/oj.log 2>&1 &
sleep 5 && cat /tmp/oj.log
```

---

> 写不动了就先看到这。遇到文档没覆盖的坑，把它补到第 8 节"排错锦囊"里——后来的人会感谢你。
