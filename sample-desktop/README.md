# sample-desktop（oj × Tauri 无端口集成 PoC）

> **新人先看 [`DEV-GUIDE.md`](./DEV-GUIDE.md)**（人话版开发手册：它是什么、怎么跑、怎么加接口、坑在哪）。

验证设计 `docs/plans/2026-09-29-tauri-subcommand-design.md` 的 **Phase 0**：把 oj 后端以
桌面应用形态交付，**本地不起业务 TCP 端口**——前端经 Tauri IPC 调 `oj_dispatch`，oj 在进程内
用 `App::dispatch`（`router.oneshot`）处理，返回 `{code,msg,data}` 信封。

## 结构

```
sample-desktop/
  public/           前端静态资源（vanilla JS，用 window.__TAURI__.core.invoke）
    index.html
    main.js
  examples/
    verify_dispatch.rs  # 直连 App::dispatch 的后端回归验证（等同 oj_dispatch 调用路径）
  src-tauri/        Tauri 工程（Rust）
    Cargo.toml      依赖 oj + tauri（oj 不链 Tauri，依赖隔离）
    tauri.conf.json withGlobalTauri: true
    config.yaml     oj 配置（base / 证书 / sqlite）
    certs/          自签名证书（生成，见下）
    src/
      lib.rs        setup 构造 App::from_config + 注册 oj_dispatch 命令
      main.rs       入口
      sample/api.ts oj handler（→ /v1/api/sample/）
```

## 前置

- Rust 工具链；Tauri CLI：`cargo install tauri-cli`（或 `npm i -g @tauri-apps/cli`）。
- 系统 WebView：macOS 内置；Linux 需 `webkit2gtk-4.1`；Windows 需 WebView2。
- `oj` CLI 二进制（`bin/oj`）：**dev 静态服务器用它托管前端，不再依赖 python3**。仓库根先构建一次：
  ```bash
  cd <仓库根>/oj-bin && cargo xtask bin   # 产出 oj-bin/bin/oj
  ```
- `node`（可选，仅前端工程化时需要；本 demo 是纯 vanilla JS，不需要）。

## 运行

```bash
# 1) 仓库根生成自签名证书（oj-cert 写 private.pem / public.pem / cert.jws）
cargo run -p oj-cert -- gen -o sample-desktop/src-tauri/certs

# 2) 打包并启动（release；推荐：前端随包内嵌，窗口正常渲染）
cd sample-desktop/src-tauri
cargo tauri build
open target/release/bundle/macos/oj-desktop-demo.app
```

> 注意：**不要**裸跑 `./target/release/oj-desktop-demo` 或 `cargo run --release`——
> 非 `.app` 包上下文下 macOS WKWebView 会渲染空白。必须经 `cargo tauri build` 出包后
> 用 `open` 启动（或 `cargo tauri dev` 走 dev 形态）。

### dev 形态（`cargo tauri dev`）

`tauri.conf.json` 的 `beforeDevCommand` 已改为用 **`oj serve`** 托管前端（`../bin/oj serve
-c src-tauri/config.yaml --app-path public`，监听 `127.0.0.1:5173`，见 `config.yaml` 的
`server.host/port`），不再依赖 `python3 -m http.server`。改了 `public/` 后 dev 模式**热重载
即生效**，无需重新打包。前提：仓库根已 `cargo xtask bin` 产出 `bin/oj`，且证书已生成（上一步）。

```bash
cd sample-desktop/src-tauri
cargo tauri dev      # 自动起 oj serve 托管前端 + 起桌面窗口
```

`cargo tauri dev` 是 **debug 构建**，`src/lib.rs` 会自动弹出 WebKit DevTools（real devtool），
可直接调试桌面前端；release 构建无此窗口。

窗口打开后点「调用 GET /v1/api/sample/」，应显示：

```
HTTP 200

{"code":0,"msg":"ok","data":{"hello":"oj tauri","mode":"no-port","via":"App::dispatch","now":...}}
```

## 后端回归验证（无需点界面）

```bash
cd sample-desktop/src-tauri
cargo run --example verify_dispatch --release
# → IPC-BACKEND status=200 / IPC-BACKEND body={...} / IPC-BACKEND OK
```

该示例绕过 Tauri 前端、直接用与 `oj_dispatch` 相同的 `App::from_config` + `App::dispatch`
调用，确认后端返回信封。

## 验证「无端口」

```bash
lsof -iTCP -sTCP:LISTEN   # 业务 API 无监听；本 demo 未接 WS，故应看不到 127.0.0.1 业务端口
```

## 已知边界（设计文档 §7/§11）

> 已解决项（v0.1.31）：**资源自包含** 与 **sqlite 安全/读写** 已落地，见下。

- **资源自包含（已解决）**：`cargo tauri build` 经 `bundle.resources` 把
  `config.yaml` / `certs/` / `src/` 随包打进 `.app/Contents/Resources`；`src/lib.rs` 的
  `setup` 在 release 用 `app.path().resource_dir()` 取资源目录（dev 仍用
  `CARGO_MANIFEST_DIR`——`resource_dir()` 在 dev 返回 `target/debug` 无 config，故分叉）。
  打出来的 `.app` 拷到别的机器直接 `open` 即可运行，不依赖构建机路径。
  （设计 §6 原写的 `resolve_resource` 在 Tauri 2.12 实际是 `app.path().resource_dir()`，
  实现以其为准。）
- **sqlite 安全/读写（已解决）**：release 下 `src/lib.rs` 把 `db.default` 重定向到每用户私有
  可写目录 `app.path().app_data_dir()`（macOS =
  `~/Library/Application Support/com.oj.desktop.demo`），**绝不落在只读 bundle 内**——
  满足 macOS 打包的读写要求。该目录由 Tauri 创建（本机实测 mode 755），但其父
  `~/Library/Application Support` 在 macOS 默认即 700（仅属主可进入），故其他用户无法读
  到其中的 `app.sqlite`，安全性由平台默认目录权限保证。dev 仍用 config 里的
  本地 `app.sqlite`。handler 当前未读写表，仅验证「无插件本地 DB」开通。
- **dev 自动 DevTools（已解决）**：`cargo tauri dev`（debug 构建）会自动弹出 WebKit
  DevTools，便于桌面前端调试（real devtool）；release 构建无此窗口。
- **WS 未接**：`App::dispatch`/`oneshot` 不跑 WS 帧循环（app.rs:6-8）；WS 需混合监听（127.0.0.1
  最小端口），本 demo 仅验证 JSON/REST。
- **证书**：随包自签名，无 `--no-cert` 后门（遵守 from_config 证书强制校验不变量）。
