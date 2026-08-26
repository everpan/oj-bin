# oj server v0.1 + user/order sample 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现独立 CLI `oj server`（目录镜像路由 + ESM 执行 + TS 转译 + 两级编译缓存 + node_modules 解析），并以 `sample/`（user/order 双模块）作为 15 用例验收载体。

**Architecture:** 新 workspace member `../../../oj/`（bin `oj`）做装配；`server` crate 换新路由（目录镜像）；根 crate bridge 增加 `ModuleLoader`（FS + deno_ast 转译 + 全局转译缓存）与 `Bridge::run_module`（TLA driver 模块，复用 KillSwitch 408）。旧 devserver/旧 router 删除。

**Tech Stack:** Rust 2024 / deno_core 0.410（`load_main_es_module_from_code` + `mod_evaluate`）/ deno_ast（transpile）/ axum 0.8 / sqlx sqlite / serde_yaml。

**Spec:** `docs/superpowers/specs/2026-08-22-oj-server-sample-design.md`

## Global Constraints

- deno_core = 0.410（已锁）。`RuntimeOptions.module_loader: Option<Rc<dyn ModuleLoader>>`；`ModuleLoader::{resolve, load}`；`load_main_es_module_from_code(specifier, code) -> ModuleId`；`mod_evaluate(id)` 返回 future，须配 `run_event_loop` 驱动（TLA 支持）。
- deno_core 扩展 JS 源（`bootstrap.js` 及任何 esm=[…] 文件）必须 7-bit ASCII（debug 校验严格，见 docs/cli.md P0 教训）。**新增 bootstrap 代码只用 ASCII 注释。**
- `json.fail(code, msg)`：code>0 时 HTTP status = code（`envelope::fail`）——405/408 信封直接用它。
- DSN 仅 `sqlite://`（含 `sqlite::memory:`）；其余启动报错。db 相对路径相对 config 目录。
- 方法映射全表：GET→get、POST→post、PUT→put、DELETE→del、PATCH→patch、HEAD→head、OPTIONS→options；未映射动词→405；无 api 文件→404；方法未导出→405（driver 内 `json.fail(405,…)`）。
- 每任务收尾 `cargo test --workspace` 双绿（debug；release 在 T14 统一复验）。
- commit 用 conventional 风格，消息末尾加行 `unix@vip.qq.com ai`。
- 代码注释密度对齐现有文件（中文、说明 why）。

## File Structure

```
cli/                          # 新 workspace member（package 名 oj）
├── Cargo.toml
├── src/main.rs               # 入口：子命令分发
├── src/args.rs               # oj 参数解析（纯函数）
├── src/manifest.rs           # manifest.yaml 加载 + 校验
├── src/server_cmd.rs         # server 装配：config→db→seed→manifest→actor→serve
└── tests/e2e.rs              # UC 集成测试（sample + 临时目录）
src/bridge/
├── transpile.rs              # 新：deno_ast strip types + 全局转译缓存
├── module_loader.rs          # 新：OjModuleLoader（相对/裸/CJS 包装/__ojRequire op）
├── kv.rs                     # 改：KVStore::del + op_kv_del
├── bootstrap.js              # 改：http.param、kv 全局、__ojRequire
├── mod.rs                    # 改：新模块声明、run_module、with_dbs_and_loader
└── runtime.rs                # 改：RuntimePool 工厂带 loader
src/config.rs                 # 重写：新 schema（host/port + DSN 字符串 map）
server/src/
├── routes.rs                 # 新：目录镜像路由（strip base + 安全 + api 文件映射）
├── router.rs                 # T10 删除（旧 Go 同款 Resolve）
├── lib.rs                    # 改：handle 走 routes.rs + actor.run_module
├── actor.rs                  # 改：run_module 报文
└── devserver.rs + bin/       # T2 删除
sample/                       # T11 全量（config/seed/manifest/api.ts/dist/vendor）
```

---

### Task 1: cli crate 骨架 + 参数解析

**Files:**
- Create: `../../../oj/Cargo.toml`, `../../../oj/src/main.rs`, `../../../oj/src/args.rs`（含 tests）
- Modify: `/Users/ever/git/golang/only-js/Cargo.toml`（members 加 `"cli"`）

**Interfaces:**
- Produces: `oj::args::{Command, ServerArgs}`；`Command::Server(ServerArgs { config: String, base: String, dir: String, dev: bool })`、`Command::Build(Vec<String>)`、`Command::None`；`parse(&[String]) -> Command`。T11 消费 `ServerArgs`。

- [ ] **Step 1: 建 crate 与依赖**

`../../../oj/Cargo.toml`：
```toml
[package]
name = "oj"
version = "0.1.0"
edition = "2024"

[dependencies]
only-js = { path = ".." }
mdm-server = { path = "../server" }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net", "time"] }

[dev-dependencies]
reqwest = { version = "0.13", default-features = false, features = ["rustls", "webpki-roots", "json"] }
```
根 `Cargo.toml`：`members = ["server", "cli"]`。

- [ ] **Step 2: 写失败测试**（`../../../oj/src/args.rs` 底部）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_branches() {
        // 无参 → None；server 默认值；显式覆盖；build 占位。
        assert!(matches!(parse(&args(&[])), Command::None));
        let Command::Server(a) = parse(&args(&["server"])) else { panic!() };
        assert_eq!((a.config.as_str(), a.base.as_str(), a.dir.as_str(), a.dev),
                   ("config.yaml", "/v1/api", "src", true));
        let Command::Server(a) = parse(&args(&["server", "-c", "c.yaml", "-b", "/api",
                                               "-d", "dist"])) else { panic!() };
        assert_eq!((a.config.as_str(), a.base.as_str(), a.dir.as_str(), a.dev),
                   ("c.yaml", "/api", "dist", false));
        let Command::Server(a) = parse(&args(&["server", "--dev", "-d", "x"])) else { panic!() };
        assert!(a.dev && a.dir == "x");
        assert!(matches!(parse(&args(&["build", "moduleA"])), Command::Build(_)));
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p oj`
Expected: 编译失败（`Command`/`parse` 未定义）。

- [ ] **Step 4: 实现**（`../../../oj/src/args.rs`）

```rust
//! oj 参数解析（纯函数）。v0.1 子命令：server（build 占位）。

/// server 子命令参数。
pub struct ServerArgs {
    pub config: String,
    pub base: String,
    pub dir: String,
    pub dev: bool,
}

/// 解析结果。None = 无子命令（main 打用法）。
pub enum Command {
    Server(ServerArgs),
    Build(Vec<String>),
    None,
}

/// `oj server [-c config.yaml] [-b /v1/api] [-d src|dist] [--dev]`。
/// -d 缺省：--dev → src，否则 dist。
pub fn parse(args: &[String]) -> Command {
    let mut it = args.iter();
    match it.next().map(|s| s.as_str()) {
        Some("build") => Command::Build(args[1..].to_vec()),
        Some("server") => {
            let (mut config, mut base, mut dir, mut dev) =
                (String::new(), String::new(), String::new(), false);
            let mut cur = it.clone().peekable();
            while let Some(a) = cur.next() {
                match a.as_str() {
                    "-c" | "-b" | "-d" => {
                        if let Some(v) = cur.next() {
                            match a.as_str() {
                                "-c" => config = v.clone(),
                                "-b" => base = v.clone(),
                                _ => dir = v.clone(),
                            }
                        }
                    }
                    "--dev" => dev = true,
                    _ => {}
                }
            }
            let dir = if dir.is_empty() { if dev { "src" } else { "dist" } } else { dir };
            Command::Server(ServerArgs {
                config: if config.is_empty() { "config.yaml".into() } else { config },
                base: if base.is_empty() { "/v1/api".into() } else { base },
                dir,
                dev,
            })
        }
        _ => Command::None,
    }
}
```

`../../../oj/src/main.rs`：
```rust
mod args;
mod server_cmd; // T11 填充；先放占位模块见 Step 6

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&argv) {
        args::Command::None => {
            eprintln!("usage: oj <server|build> [flags]\n  oj server -c config.yaml -b /v1/api -d src --dev");
            std::process::exit(2);
        }
        args::Command::Build(_) => {
            eprintln!("oj build: not implemented (v0.1)");
            std::process::exit(2);
        }
        args::Command::Server(a) => {
            if let Err(e) = server_cmd::run(a) {
                eprintln!("oj server: {e}");
                std::process::exit(1);
            }
        }
    }
}
```

- [ ] **Step 5: 占位 server_cmd**（`../../../oj/src/server_cmd.rs`，T11 替换）

```rust
//! server 装配层（T11 实现）。
use crate::args::ServerArgs;

pub fn run(_a: ServerArgs) -> Result<(), String> {
    Err("not implemented".into())
}
```

- [ ] **Step 6: 跑测试确认通过 + 提交**

Run: `cargo test -p oj && cargo build --workspace`
Expected: 1 passed；workspace 编译通过。

```bash
git add Cargo.toml Cargo.lock oj
git commit -m "feat(oj): cli crate skeleton with arg parsing

unix@vip.qq.com ai"
```

---

### Task 2: 删除 devserver 旧资产

**Files:**
- Delete: `server/src/devserver.rs`, `server/src/bin/devserver.rs`
- Modify: `server/src/lib.rs:4`（删 `pub mod devserver;`）
- Modify: `server/Cargo.toml`（若有 `[[bin]] devserver` 段则删）

**Interfaces:**
- Consumes: 无。
- Produces: server crate 不再暴露 devserver；`config.rs` 的唯一消费者消失（T3 重写自由）。

- [ ] **Step 1: 确认引用面**

Run: `grep -rn "devserver" --include="*.rs" . | grep -v target`
Expected: 仅 `server/src/lib.rs` 的 mod 行与文件自身。

- [ ] **Step 2: 删除并清 mod 行**

删两个文件；`server/src/lib.rs` 去掉 `pub mod devserver;`。检查 `server/Cargo.toml` 的 `[[bin]]` 段（`src/bin/devserver.rs` 是自动 bin，无显式段则不动）。

- [ ] **Step 3: 双绿验证 + 提交**

Run: `cargo test --workspace`
Expected: 全绿（devserver 测试随之消失，server 数量减少）。

```bash
git add -A server
git commit -m "refactor(server): drop devserver (superseded by oj CLI)

unix@vip.qq.com ai"
```

---

### Task 3: config 新 schema 重写

**Files:**
- Rewrite: `src/config.rs`
- Modify: `src/lib.rs`（若 pub use 路径变化则同步）

**Interfaces:**
- Produces: `config::{Config, ServerCfg}`；`Config { server: ServerCfg, db: HashMap<String,String>, redis: HashMap<String,String> }`；`ServerCfg { host: String, port: u16, timeout: String, pool_size: u32 }`（默认 localhost/778/"30s"/4）；`load_from(dir: &Path, explicit: Option<&str>) -> Result<Config, String>`；保留 `parse_duration(&str) -> Result<Duration, String>`。T11 消费。

- [ ] **Step 1: 写失败测试**（新 `src/config.rs` 的 tests 模块；先清空旧实现再写）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_file() {
        let c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
        assert_eq!((c.server.host.as_str(), c.server.port), ("localhost", 778));
        assert_eq!(parse_duration(&c.server.timeout).unwrap().as_secs(), 30);
        assert_eq!(c.server.pool_size, 4);
        assert!(c.db.is_empty() && c.redis.is_empty());
    }

    #[test]
    fn explicit_missing_errors() {
        let e = load_from(std::path::Path::new("."), Some("no-such.yaml")).unwrap_err();
        assert!(e.contains("not found"), "{e}");
    }

    #[test]
    fn parses_url_style_dsn_map() {
        let dir = std::env::temp_dir().join(format!("ojcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cfg.yaml"), concat!(
            "server:\n  host: 0.0.0.0\n  port: 9000\n  timeout: 5s\n  pool_size: 2\n",
            "db:\n  default: sqlite://db.sqlite\n",
            "redis:\n  default: redis://127.0.0.1:6379/1\n",
        )).unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.server.host, "0.0.0.0");
        assert_eq!(c.db["default"], "sqlite://db.sqlite");
        assert_eq!(c.redis.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_yaml_errors() {
        let dir = std::env::temp_dir().join(format!("ojcfgbad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cfg.yaml"), "server: [broken").unwrap();
        assert!(load_from(&dir, Some("cfg.yaml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p only-js config`
Expected: 编译失败（新字段/签名不存在）。

- [ ] **Step 3: 重写实现**

```rust
//! oj 配置（cli2.md 预案 schema）：server(host/port) + db/redis 的 URL 风格 DSN map。
//! 旧三层 env 叠加已删（预案即单文件）。

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    /// 时长字符串（如 "30s"），parse_duration 解析。
    pub timeout: String,
    pub pool_size: u32,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self { host: "localhost".into(), port: 778, timeout: "30s".into(), pool_size: 4 }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    /// name → DSN（sqlite://…；v0.1 仅 sqlite）。
    pub db: HashMap<String, String>,
    /// name → redis URL（v0.1 warn 后用内存 KV）。
    pub redis: HashMap<String, String>,
}

/// explicit=None 找默认 config.yaml，缺失静默用默认值；Some 指向缺失文件报错。
pub fn load_from(dir: &Path, explicit: Option<&str>) -> Result<Config, String> {
    let path = match explicit {
        Some(p) => {
            let full = dir.join(p);
            if !full.is_file() {
                return Err(format!("config file not found: {}", full.display()));
            }
            full
        }
        None => {
            let full = dir.join("config.yaml");
            if !full.is_file() {
                return Ok(Config::default());
            }
            full
        }
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_yaml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// "30s"/"500ms" → Duration（沿用旧实现语义）。
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
    let mult = match unit {
        "s" | "sec" | "secs" => 1.0,
        "ms" => 0.001,
        "m" | "min" => 60.0,
        _ => return Err(format!("invalid duration unit: {unit}")),
    };
    Ok(std::time::Duration::from_secs_f64(n * mult))
}
```
（`parse_duration` 若旧版可直接搬用则保留旧函数体，测试为准。）检查 `src/lib.rs` 对 config 的 pub use，保持 `pub mod config;` 不变。

- [ ] **Step 4: 跑测试确认通过 + workspace 检查 + 提交**

Run: `cargo test -p only-js && cargo test --workspace`
Expected: config 4 测绿；workspace 绿。

```bash
git add src/config.rs src/lib.rs
git commit -m "feat(config): rewrite to oj schema (host/port, URL-style DSN maps)

unix@vip.qq.com ai"
```

---

### Task 4: 新路由 routes.rs（目录镜像）

**Files:**
- Create: `server/src/routes.rs`（含 tests）
- Modify: `server/src/lib.rs`（加 `pub mod routes;`；旧 `pub mod router;` 保留至 T10）

**Interfaces:**
- Produces: `routes::{Routes, method_name}`；`Routes::new(base: &str, root: impl Into<PathBuf>, ts: bool)`；`Routes::resolve(&self, http_path: &str) -> Option<std::path::PathBuf>`；`method_name(http_method: &str) -> Option<&'static str>`。T10 消费。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(files: &[&str]) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "oj-routes-{}-{:p}", std::process::id(), files as *const _
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for rel in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "// api").unwrap();
        }
        base
    }

    #[test]
    fn mirrors_directory_tree_any_depth() {
        let root = fixture(&["user/account/api.ts", "user/profile/detail/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        assert_eq!(r.resolve("/v1/api/user/account/"), Some(root.join("user/account/api.ts")));
        assert_eq!(r.resolve("/v1/api/user/account"), Some(root.join("user/account/api.ts")));
        assert_eq!(r.resolve("/v1/api/user/profile/detail/"),
                   Some(root.join("user/profile/detail/api.ts")));
    }

    #[test]
    fn missing_or_traversal_is_none() {
        let root = fixture(&["user/account/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        assert_eq!(r.resolve("/v1/api/none/here/"), None);
        assert_eq!(r.resolve("/v1/api/../etc/"), None);
        assert_eq!(r.resolve("/v1/api//dbl/"), None);
        assert_eq!(r.resolve("/other/base/user/account/"), None);
        assert_eq!(r.resolve("/v1/api/"), None);
    }

    #[test]
    fn release_mode_maps_api_js() {
        let root = fixture(&["user/account/api.js"]);
        assert!(Routes::new("/v1/api", &root, false).resolve("/v1/api/user/account/").is_some());
        assert!(Routes::new("/v1/api", &root, true).resolve("/v1/api/user/account/").is_none());
    }

    #[test]
    fn method_table_complete() {
        assert_eq!(method_name("GET"), Some("get"));
        assert_eq!(method_name("DELETE"), Some("del"));
        assert_eq!(method_name("PATCH"), Some("patch"));
        assert_eq!(method_name("HEAD"), Some("head"));
        assert_eq!(method_name("OPTIONS"), Some("options"));
        assert_eq!(method_name("TRACE"), None);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-server routes`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现**

```rust
//! 目录镜像路由：URL = base 之后的目录路径 → `<root>/<path>/api.(ts|js)`。
//! 任意深度；无 api 文件的目录不是路由（可作纯工具代码目录）。

use std::path::{Path, PathBuf};

/// 目录镜像路由器。ts=true（--dev）找 api.ts，否则 api.js。
pub struct Routes {
    base: String,
    root: PathBuf,
    ts: bool,
}

impl Routes {
    pub fn new(base: &str, root: impl Into<PathBuf>, ts: bool) -> Self {
        // 归一 base：保证前后各一个 '/'（"/v1/api" 与 "/v1/api/" 等价）。
        let base = format!("/{}", base.trim_matches('/'));
        Self { base, root: root.into(), ts }
    }

    /// 解析 HTTP 路径 → api 文件绝对路径；目录不存在/越界/非文件 → None。
    pub fn resolve(&self, http_path: &str) -> Option<PathBuf> {
        let rel = http_path.strip_prefix(self.base.as_str())?;
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return None;
        }
        // 安全：拒绝空段与越界段（目录穿越按 404 处理）。
        if rel.split('/').any(|s| {
            s.is_empty() || s == ".." || s == "." || s.contains('\\') || s.contains('\0')
        }) {
            return None;
        }
        let file = self.root.join(rel).join(if self.ts { "api.ts" } else { "api.js" });
        file.is_file().then_some(file)
    }
}

/// HTTP 动词 → handler 方法名（全表；DELETE→del）。未映射 → None（405）。
pub fn method_name(m: &str) -> Option<&'static str> {
    match m {
        "GET" => Some("get"),
        "POST" => Some("post"),
        "PUT" => Some("put"),
        "DELETE" => Some("del"),
        "PATCH" => Some("patch"),
        "HEAD" => Some("head"),
        "OPTIONS" => Some("options"),
        _ => None,
    }
}

/// 收集 root 下全部路由相对路径（启动时打印路由表用，UC-8）。
pub fn route_table(root: &Path, ts: bool) -> Vec<String> {
    let ext = if ts { "api.ts" } else { "api.js" };
    let mut out = Vec::new();
    walk(root, ext, &mut Vec::new(), &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, ext: &str, rel: &mut Vec<String>, acc: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            rel.push(name);
            walk(&p, ext, rel, acc);
            rel.pop();
        } else if name == ext {
            acc.push(rel.join("/"));
        }
    }
}
```
`lib.rs` 加 `pub mod routes;`。

- [ ] **Step 4: 跑测试确认通过 + 提交**

Run: `cargo test -p mdm-server`
Expected: routes 4 测绿 + 既有全绿。

```bash
git add server/src/routes.rs server/src/lib.rs
git commit -m "feat(server): directory-mirror router (any depth, api.ts/js, method table)

unix@vip.qq.com ai"
```

---

### Task 5: bootstrap 增补（http.param + kv 全局）

**Files:**
- Modify: `src/bridge/kv.rs`（KVStore::del + op_kv_del + InMemoryKV::del）
- Modify: `src/bridge/bootstrap.js`（http.param、kv、__ojRequire 占位暂不加——T8 加）
- Modify: `src/bridge/mod.rs`（extension ops 列表加 `kv::op_kv_del`）

**Interfaces:**
- Produces: JS 侧 `http.param(name, default)`（query 取值带默认）、`kv.get/set/del`（与 `redis` 同底座）；Rust 侧 `KVStore::del(&self, key)`。sample（T11）消费。

- [ ] **Step 1: 写失败测试**（`src/bridge/mod.rs` tests 追加）

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn http_param_and_kv_global() {
        let (b, _) = new_bridge();
        let cap = b
            .run_with(
                r#"
                kv.set("k", "v");
                kv.get("k").then((v) => {
                    const hit = v;
                    kv.del("k");
                    kv.get("k").then((v2) => json.ok({
                        hit, gone: v2,
                        p1: http.param("id", 0),
                        p2: http.param("missing", "dft"),
                    }));
                }).catch((e) => json.fail(500, String(e)));
                "#,
                RequestInfo {
                    query: [("id".into(), "7".into())].into_iter().collect(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], json!({"hit": "v", "gone": null, "p1": "7", "p2": "dft"}), "{v}");
    }
```
注意：嵌套 then 链的完成依赖事件循环泵（run_to_completion 已泵尽 Promise），如书写时发现链未驱动，改为顶层 `Promise.all` 结构。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p only-js http_param_and_kv`
Expected: FAIL（`kv`/`http.param` undefined → ReferenceError → 500 或 body 断言失败）。

- [ ] **Step 3: 实现**

`kv.rs` trait 加：
```rust
    /// 删除键（幂等：不存在为成功）。
    async fn del(&self, key: &str) -> BridgeResult<()>;
```
`InMemoryKV` 实现：`self.mu.write().unwrap().remove(key); Ok(())`。
新 op（仿 op_kv_set）：
```rust
/// kv.del(key)：Promise<true>。
#[op2]
pub async fn op_kv_del(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<bool, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.del(&key).await.map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}
```
`mod.rs` extension ops 加 `kv::op_kv_del,`。

`bootstrap.js`（ASCII 注释）：
```js
// ----- http helpers -----
globalThis.http = new Proxy({}, {
  get: (_t, p) => {
    if (p === "param") {
      return (name, def) => {
        const v = httpInfo().query[name];
        return v === undefined ? def : v;
      };
    }
    return httpInfo()[p];
  },
});

// ----- kv: same in-memory KV as redis global (spec name for oj handlers) -----
globalThis.kv = {
  get: (key) => op_kv_get(String(key)),
  set: (key, value) => op_kv_set(String(key), String(value)),
  del: (key) => op_kv_del(String(key)),
};
```
（替换原 http Proxy 段；`redis` 段保留不动。import 列表加 `op_kv_del`。）

- [ ] **Step 4: 跑测试确认通过 + 提交**

Run: `cargo test -p only-js`
Expected: 全绿（含新测）。

```bash
git add src/bridge
git commit -m "feat(bridge): http.param helper and kv global (get/set/del)

unix@vip.qq.com ai"
```

---

### Task 6: TranspileCache（deno_ast）

**Files:**
- Modify: 根 `Cargo.toml`（加 deno_ast）
- Create: `src/bridge/transpile.rs`（含 tests）
- Modify: `src/bridge/mod.rs`（`mod transpile;`）

**Interfaces:**
- Produces: `transpile::{transpile_src, cached_transpile, transpile_hits}`；`transpile_src(path: &Path, src: &str) -> Result<String, String>`（strip types，错误带 `文件:行:列`）；`cached_transpile(path: &Path) -> Result<String, String>`（读盘 + mtime 单槽缓存，全局共享跨 actor）；`#[doc(hidden)] transpile_hits() -> usize`（实际转译次数，UC-14 断言用）。T7/T9 消费。

- [ ] **Step 1: 加依赖**

Run: `cargo add deno_ast -p only-js --features transpile`
（版本由解析器定；与 deno_core 无版本耦合。）

- [ ] **Step 2: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_type_annotations() {
        let out = transpile_src(Path::new("a.ts"),
            "const x: number = 1;\nfunction f(a: string): string { return a; }\nexport default 1;\n").unwrap();
        assert!(out.contains("const x = 1;"), "{out}");
        assert!(out.contains("return a;"), "{out}");
        assert!(!out.contains(": number"), "{out}");
    }

    #[test]
    fn syntax_error_has_position() {
        let e = transpile_src(Path::new("bad.ts"), "function {{{{").unwrap_err();
        assert!(e.contains("bad.ts"), "{e}");
    }

    #[test]
    fn cache_hits_on_second_call_same_mtime() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("oj-tr-{}-{:p}.ts", std::process::id(), &p_placeholder()));
        std::fs::write(&p, "const a: number = 1;\n").unwrap();
        let before = transpile_hits();
        let s1 = cached_transpile(&p).unwrap();
        let s2 = cached_transpile(&p).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(transpile_hits(), before + 1, "second call must hit cache");
        // 内容变更 → mtime 变 → 重转译。
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&p, "const b: number = 2;\n").unwrap();
        let s3 = cached_transpile(&p).unwrap();
        assert!(s3.contains("const b"), "{s3}");
        assert_eq!(transpile_hits(), before + 2);
        let _ = std::fs::remove_file(&p);
    }

    fn p_placeholder() -> usize { 0 } // 使路径唯一；实现时不需此函数可直接用静态计数
}
```
（实现时把 `p_placeholder` 换成静态 AtomicUsize 计数器，与其他测试的 TempDir 模式一致。）

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p only-js transpile`
Expected: 编译失败。

- [ ] **Step 4: 实现**

```rust
//! TS→JS 转译（deno_ast strip types）+ 全局转译缓存。
//! 缓存按 (path, mtime) 单槽条目：改文件即失效替换，容量天然有界。
//! ponytail: 进程级全局（跨 Bridge/actor 共享）；测试临时目录路径各异，条目随进程消亡。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// 实际发生转译的次数（UC-14 缓存断言用）。
static TRANSPILE_COUNT: OnceLock<std::sync::atomic::AtomicUsize> = OnceLock::new();

#[doc(hidden)]
pub fn transpile_hits() -> usize {
    TRANSPILE_COUNT
        .get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
        .load(std::sync::atomic::Ordering::Relaxed)
}

type Cache = Mutex<HashMap<PathBuf, (SystemTime, String)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 读盘 + mtime 缓存 + 转译（.ts）或原文（.js 直读不转译）。
pub fn cached_transpile(path: &Path) -> Result<String, String> {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if let Some((t, src)) = cache().lock().unwrap().get(path) {
        if *t == mtime {
            return Ok(src.clone());
        }
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let out = if path.extension().is_some_and(|e| e == "ts") {
        transpile_src(path, &raw)?
    } else {
        raw
    };
    cache().lock().unwrap().insert(path.to_path_buf(), (mtime, out.clone()));
    Ok(out)
}

/// 纯转译：deno_ast 解析 TypeScript → transpile（strip types）。
pub fn transpile_src(path: &Path, src: &str) -> Result<String, String> {
    TRANSPILE_COUNT
        .get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let parsed = deno_ast::parse_module(deno_ast::ParseParams {
        specifier: path.display().to_string(),
        text_info: deno_ast::SourceText::from(src),
        media_type: deno_ast::MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let out = parsed
        .transpile(&deno_ast::TranspileOptions::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(out.text)
}
```
（若解析出的 deno_ast 版本 `ParseParams`/`TranspileOptions` 字段有出入，以该版本 docs 为准调整字段名——测试是契约。）

- [ ] **Step 5: 跑测试确认通过 + 提交**

Run: `cargo test -p only-js`
Expected: 全绿。

```bash
git add Cargo.toml Cargo.lock src/bridge/transpile.rs src/bridge/mod.rs
git commit -m "feat(bridge): deno_ast transpile with mtime-keyed global cache

unix@vip.qq.com ai"
```

---

### Task 7: OjModuleLoader（相对导入 + 加载 + CJS 包装）

**Files:**
- Create: `src/bridge/module_loader.rs`（含 tests）
- Modify: `src/bridge/mod.rs`（`mod module_loader;` + pub use）

**Interfaces:**
- Produces: `module_loader::{LoaderShared, OjModuleLoader, versioned_specifier}`；`LoaderShared { project_root: PathBuf, ts: bool }`；`versioned_specifier(path: &Path) -> Result<deno_core::ModuleSpecifier, String>`（`file://<abs>?v=<mtime nanos>`）；`OjModuleLoader` 实现 `deno_core::ModuleLoader`。内部纯函数（可单测）：`resolve_relative(base_dir: &Path, spec: &str, ts: bool) -> Result<PathBuf, String>`、`wrap_cjs(src: &str) -> String`、`looks_cjs(src: &str) -> bool`。T8/T9 消费。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fx(files: &[(&str, &str)]) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "oj-ldr-{}-{}", std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (rel, content) in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        base
    }

    #[test]
    fn relative_resolution_completes_extensions() {
        let root = fx(&[
            ("user/_shared/validate.ts", "export function f() {}\n"),
            ("user/_shared/mod/index.ts", "export const x = 1;\n"),
            ("user/plain.js", "export const y = 2;\n"),
        ]);
        let dir = root.join("user/account");
        let ts = true;
        assert!(resolve_relative(&dir, "../_shared/validate", ts).unwrap().ends_with("validate.ts"));
        assert!(resolve_relative(&dir, "../_shared/mod", ts).unwrap().ends_with("mod/index.ts"));
        assert!(resolve_relative(&dir, "../plain", ts).unwrap().ends_with("plain.js"));
        let err = resolve_relative(&dir, "../nope", ts).unwrap_err();
        assert!(err.contains("tried"), "{err}");
    }

    #[test]
    fn versioned_specifier_roundtrip() {
        let root = fx(&[("a.ts", "export default 1;\n")]);
        let p = root.join("a.ts");
        let url = versioned_specifier(&p).unwrap();
        assert!(url.as_str().starts_with("file://"), "{url}");
        assert!(url.as_str().contains("?v="), "{url}");
    }

    #[test]
    fn cjs_detection_and_wrap() {
        assert!(looks_cjs("module.exports = { a: 1 };\n"));
        assert!(!looks_cjs("export default 1;\n"));
        assert!(!looks_cjs("import x from 'y';\nmodule.exports = x;\n"));
        let wrapped = wrap_cjs("module.exports = { a: 1 };\n");
        assert!(wrapped.contains("__oj_cjs_module"), "{wrapped}");
        assert!(wrapped.contains("export default __oj_cjs_module.exports"), "{wrapped}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p only-js module_loader`
Expected: 编译失败。

- [ ] **Step 3: 实现**

```rust
//! oj 的 ESM 模块加载器：相对导入（Deno 风格补全）+ 裸 specifier（node_modules，T8）
//! + CJS 包装互操作。?v=<mtime> 版本化 specifier 让 V8 模块缓存天然按内容失效。
//! ponytail: 旧版本模块不可卸载，按编辑次数缓慢积累（dev 重启清零，release 有界）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use deno_core::ModuleSpecifier;
use deno_core::error::ModuleLoaderError;
use deno_core::modules::*;

use super::transpile::cached_transpile;

/// loader 共享配置（project_root 用于 node_modules 回溯上界与 CJS require）。
pub struct LoaderShared {
    pub project_root: PathBuf,
    /// dev 模式（.ts 可达）。release 下 .ts 仍可被 import（dist 一般没有）。
    pub ts: bool,
}

/// deno_core ModuleLoader 实现。Rc<dyn ModuleLoader> 挂 RuntimeOptions，
/// 内部状态经 Arc 跨 actor 共享（转译缓存在 transpile 模块全局）。
pub struct OjModuleLoader {
    pub inner: Arc<LoaderShared>,
}

impl deno_core::ModuleLoader for OjModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        self.resolve_inner(specifier, referrer).map_err(ModuleLoaderError::from)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        ModuleLoadResponse::Sync(Self::load_specifier(module_specifier))
    }
}

impl OjModuleLoader {
    fn resolve_inner(&self, specifier: &str, referrer: &str) -> Result<ModuleSpecifier, String> {
        if let Ok(url) = ModuleSpecifier::parse(specifier) {
            // 绝对 file:// URL（driver 对 api 模块的 import）：原样通过。
            if url.scheme() == "file" {
                return Ok(url);
            }
            return Err(format!("unsupported scheme: {specifier}"));
        }
        let ref_dir = referrer_dir(referrer)?;
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let p = resolve_relative(&ref_dir, specifier, self.inner.ts)?;
            versioned_specifier(&p)
        } else {
            // 裸 specifier：T8 实现（本任务先报清晰错误）。
            Err(format!(
                "bare specifier '{specifier}' not supported yet (node_modules resolution lands in the next task)"
            ))
        }
    }

    /// load：剥 ?v= → 读盘（.ts 走缓存转译）→ CJS 则包装 → ModuleSource。
    fn load_specifier(spec: &ModuleSpecifier) -> Result<ModuleSource, ModuleLoaderError> {
        let path = spec
            .to_file_path()
            .map_err(|_| ModuleLoaderError::from(format!("not a file url: {spec}")))?;
        let src = cached_transpile(&path).map_err(ModuleLoaderError::from)?;
        let code = if looks_cjs(&src) { wrap_cjs(&src) } else { src };
        Ok(ModuleSource::new(
            ModuleType::JavaScript,
            code.into(),
            spec,
            None,
        ))
    }
}

/// referrer（file URL，可能带 ?v=）→ 所在目录。
fn referrer_dir(referrer: &str) -> Result<PathBuf, String> {
    let url = ModuleSpecifier::parse(referrer).map_err(|e| format!("bad referrer {referrer}: {e}"))?;
    let path = url
        .to_file_path()
        .map_err(|_| format!("referrer not a file url: {referrer}"))?;
    Ok(path.parent().map(|p| p.to_path_buf()).unwrap_or_default())
}

/// 相对导入解析：as-is → +.ts → +.js → /index.ts → /index.js（存在即命中）。
pub fn resolve_relative(base_dir: &Path, spec: &str, ts: bool) -> Result<PathBuf, String> {
    let joined = base_dir.join(spec);
    let mut tried = Vec::new();
    let stem = joined.clone();
    let mut candidates: Vec<PathBuf> = vec![stem.clone()];
    if ts {
        candidates.push(stem.with_extension("ts"));
    }
    candidates.push(stem.with_extension("js"));
    if ts {
        candidates.push(stem.join("index.ts"));
    }
    candidates.push(stem.join("index.js"));
    for c in &candidates {
        tried.push(c.display().to_string());
        if c.is_file() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "cannot resolve '{spec}' from '{}': tried [{}]",
        base_dir.display(),
        tried.join(", ")
    ))
}

/// 版本化 specifier：file://<abs>?v=<mtime nanos>（mtime 变 → 新模块 → 热重载）。
pub fn versioned_specifier(path: &Path) -> Result<ModuleSpecifier, String> {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    let nanos = mtime
        .duration_since(SystemTime)
        .map_err(|e| format!("bad mtime on {}: {e}", path.display()))?;
    let abs = std::fs::canonicalize(path).map_err(|e| format!("canonicalize {}: {e}", path.display()))?;
    let mut url = ModuleSpecifier::from_file_path(abs)
        .map_err(|_| format!("cannot build file url from {}", path.display()))?;
    url.set_query(Some(&format!("v={}", nanos.as_nanos())));
    Ok(url)
}
use std::time::SystemTime;

/// CJS 启发式：无 ESM 顶层语法且是 .js/.cjs（node_modules 包）。
/// ponytail: 启发式覆盖主流简单包；误判时报错信息可定位（module is not defined）。
pub fn looks_cjs(src: &str) -> bool {
    !src.contains("export ") && !src.contains("export{") && !src.contains("import ")
        && !src.contains("import(")
}

/// CJS → ESM 包装：default = module.exports；require 由 __ojRequire 全局提供（T8）。
pub fn wrap_cjs(src: &str) -> String {
    format!(
        "const __oj_cjs_module = {{ exports: {{}} }};\n(function (module, exports, require) {{\n{src}\n}})(__oj_cjs_module, __oj_cjs_module.exports, __ojRequire);\nexport default __oj_cjs_module.exports;\n"
    )
}
```
`mod.rs`：`mod module_loader;` + `pub use module_loader::{LoaderShared, OjModuleLoader, versioned_specifier};`
（`ModuleSource::new` 参数序 / `ModuleSourceCode` 的 `From<String>` 以 0.410 实际签名为准——编译器会指出，测试是契约。）

- [ ] **Step 4: 跑测试确认通过 + 提交**

Run: `cargo test -p only-js`
Expected: 全绿。

```bash
git add src/bridge
git commit -m "feat(bridge): ESM module loader (relative resolution, mtime-versioned specifiers, CJS wrap)

unix@vip.qq.com ai"
```

---

### Task 8: 裸 specifier（node_modules）+ __ojRequire

**Files:**
- Modify: `src/bridge/module_loader.rs`（resolve_bare + 接入 resolve_inner）
- Modify: `src/bridge/bootstrap.js`（`__ojRequire`）
- Modify: `src/bridge/mod.rs`（op 注册）

**Interfaces:**
- Produces: 内部 `resolve_bare(spec: &str, from_dir: &Path, root: &Path) -> Result<PathBuf, String>`（Node 算法：逐级 node_modules，pkg → package.json module→main→index.js，subpath 直映射）；JS 侧 `__ojRequire(name, referrerPath)` 同步 require（eval + 全局缓存）。sample 的 `order/account`（T11）消费。

- [ ] **Step 1: 写失败测试**（module_loader.rs tests 追加）

```rust
    #[test]
    fn bare_resolves_node_modules() {
        let root = fx(&[
            ("node_modules/escape-goat/index.js", "export const x = 1;\n"),
            ("node_modules/escape-goat/package.json",
             r#"{"name":"escape-goat","version":"4.0.0","type":"module"}"#),
            ("node_modules/cjspkg/main.js", "module.exports = { n: 1 };\n"),
            ("node_modules/cjspkg/package.json",
             r#"{"name":"cjspkg","version":"1.0.0","main":"main.js"}"#),
            ("node_modules/withmod/pkg/lib/util.js", "export const u = 1;\n"),
            ("node_modules/withmod/pkg/package.json", r#"{"name":"withmod"}"#),
        ]);
        let from = root.join("src/user");
        // ESM 包：type:module → index.js。
        assert!(resolve_bare("escape-goat", &from, &root).unwrap().ends_with("escape-goat/index.js"));
        // CJS 包：main 字段。
        assert!(resolve_bare("cjspkg", &from, &root).unwrap().ends_with("cjspkg/main.js"));
        // subpath 直映射。
        assert!(resolve_bare("withmod/pkg/lib/util.js", &from, &root).unwrap().ends_with("lib/util.js"));
        // 不存在 → 错误含提示。
        let e = resolve_bare("nope-pkg", &from, &root).unwrap_err();
        assert!(e.contains("node_modules"), "{e}");
        // 回溯：src/user/feat 深处也能找到根 node_modules。
        assert!(resolve_bare("escape-goat", &root.join("src/user/feat"), &root).is_ok());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p only-js module_loader`
Expected: `resolve_bare` 未定义，编译失败。

- [ ] **Step 3: 实现**

`module_loader.rs` 加：
```rust
/// 裸 specifier 解析（Node 算子简化版）：
/// pkg → <dir>/node_modules/<pkg>（从 from_dir 逐级向上至 root）→ package.json
/// 的 module → main → index.js；subpath（pkg/a.js）直映射包内文件。
/// ponytail: 不做 exports/conditions 映射与 pnpm 布局；主流简单包可用。
pub fn resolve_bare(spec: &str, from_dir: &Path, root: &Path) -> Result<PathBuf, String> {
    // pkg 名：@scope/name 占两段。
    let mut parts: Vec<&str> = spec.split('/').collect();
    let pkg = if parts.first().is_some_and(|s| s.starts_with('@')) && parts.len() >= 2 {
        format!("{}/{}", parts[0], parts[1])
    } else {
        parts[0].to_string()
    };
    let sub: Vec<&str> = if pkg.contains('/') { parts.split_off(2) } else { parts.split_off(1) };

    let mut tried = Vec::new();
    let mut dir = Some(from_dir);
    while let Some(d) = dir {
        let nm = d.join("node_modules").join(&pkg);
        if nm.is_dir() {
            if sub.is_empty() {
                let p = pkg_entry(&nm)?;
                return Ok(p);
            }
            let p = nm.join(sub.join("/"));
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p.display().to_string());
        } else {
            tried.push(nm.display().to_string());
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    Err(format!(
        "cannot resolve '{spec}' from '{}' (node_modules installed?): tried [{}]",
        from_dir.display(),
        tried.join(", ")
    ))
}

/// 包入口：package.json 的 module → main → index.js。
fn pkg_entry(pkg_dir: &Path) -> Result<PathBuf, String> {
    let pj = pkg_dir.join("package.json");
    if pj.is_file() {
        let text = std::fs::read_to_string(&pj).map_err(|e| format!("read {pj:?}: {e}"))?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        for field in ["module", "main"] {
            if let Some(m) = v[field].as_str() {
                let p = pkg_dir.join(m.trim_start_matches("./"));
                if p.is_file() {
                    return Ok(p);
                }
            }
        }
    }
    let idx = pkg_dir.join("index.js");
    if idx.is_file() {
        return Ok(idx);
    }
    Err(format!("package '{}' has no entry (module/main/index.js)", pkg_dir.display()))
}
```
`resolve_inner` 的裸分支替换为：
```rust
        } else {
            let p = resolve_bare(specifier, &ref_dir, &self.inner.project_root)?;
            versioned_specifier(&p)
        }
```

CJS require：`bootstrap.js` 加（ASCII 注释）：
```js
// ----- __ojRequire: sync require() for CJS interop (eval + process-wide cache) -----
const __ojReqCache = new Map();
globalThis.__ojRequire = (name, referrerPath) => {
  const key = referrerPath + "::" + name;
  if (!__ojReqCache.has(key)) {
    const resolved = __oj_resolve_cjs(name, referrerPath); // op: returns {path, code}
    const fn = new Function("module", "exports", "require", resolved.code);
    const m = { exports: {} };
    fn(m, m.exports, (n) => globalThis.__ojRequire(n, resolved.path));
    __ojReqCache.set(key, m.exports);
  }
  return __ojReqCache.get(key);
};
```
`mod.rs` 新 op（同步，走 resolve_bare + 读盘，.ts 不支持——CJS 依赖是 .js）：
```rust
/// CJS require 底座：解析 + 读源码（JS 侧 eval 执行）。
#[op2]
#[serde]
pub fn op_resolve_cjs(
    #[string] name: String,
    #[string] referrer: String,
) -> Result<serde_json::Value, deno_error::JsErrorBox> {
    let root = oj_project_root(); // LoaderShared 经 StableState 存 OpState（见下）
    let from = Path::new(&referrer).parent().unwrap_or(Path::new(".")).to_path_buf();
    let p = module_loader::resolve_bare(&name, &from, &root)
        .map_err(JsErrorBox::generic)?;
    let code = std::fs::read_to_string(&p).map_err(|e| JsErrorBox::generic(format!("read {}: {e}", p.display())))?;
    Ok(serde_json::json!({ "path": p.display().to_string(), "code": code }))
}
```
`LoaderShared` 存入 `StableState`（`pub loader: Option<Arc<LoaderShared>>`），op 从 OpState 取；`op_resolve_cjs` 里 bootstrap import 名为 `__oj_resolve_cjs`（deno_core 自动蛇形→驼峰映射）。extension ops 列表加 `op_resolve_cjs`。

- [ ] **Step 4: 跑测试确认通过 + 提交**

Run: `cargo test -p only-js`
Expected: 全绿。

```bash
git add src/bridge
git commit -m "feat(bridge): bare specifier node_modules resolution and CJS require interop

unix@vip.qq.com ai"
```

---

### Task 9: Bridge::run_module（driver TLA + 408/405）

**Files:**
- Modify: `src/bridge/mod.rs`（`with_dbs_and_loader`、`run_module`、StableState.loader）
- Modify: `src/bridge/runtime.rs`（RuntimePool 工厂带 loader）

**Interfaces:**
- Produces: `Bridge::with_dbs_and_loader(dbs, kv, registry, inspect, loader: Option<Arc<LoaderShared>>) -> Bridge`（`with_dbs` 委托 None）；`Bridge::run_module(&self, api_path: &Path, method: &str, req: RequestInfo, timeout: Duration) -> Result<Capture, RunError>`。T10/T11 消费。

- [ ] **Step 1: 写失败测试**（mod.rs tests 追加；本任务 = R1/R2 spike 的落点）

```rust
    fn mod_fx(files: &[(&str, &str)]) -> (std::path::PathBuf, std::path::PathBuf) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "oj-mod-{}-{}", std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (rel, content) in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        (base.clone(), base.join(files[0].0))
    }

    fn module_bridge(root: &std::path::PathBuf) -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(crate::LoaderShared { project_root: root.clone(), ts: true })),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runs_exported_method_from_ts_module() {
        let (root, api) = mod_fx(&[(
            "user/account/api.ts",
            "import { tag } from \"../_shared/util\";\n\
             function get(): void { json.ok({ ok: 1, tag: tag(\"x\") }); }\n\
             export default { get };\n",
        ), ("user/_shared/util.ts", "export function tag(s: string): string { return \"t-\" + s; }\n")]);
        let b = module_bridge(&root);
        let cap = b
            .run_module(&api, "get", RequestInfo::default(), std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], json!({"ok": 1, "tag": "t-x"}), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn method_not_exported_is_405() {
        let (root, api) = mod_fx(&[("u/f/api.ts", "export default { get() { json.ok({}); } };\n")]);
        let b = module_bridge(&root);
        let cap = b
            .run_module(&api, "del", RequestInfo::default(), std::time::Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(cap.status, 405, "{}", String::from_utf8_lossy(&cap.body));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn infinite_module_handler_times_out_and_bridge_survives() {
        // R1 spike：ESM/TLA 模型下 KillSwitch 复验。
        let (root, api) = mod_fx(&[("u/f/api.ts", "export default { get() { while (true) {} } };\n")]);
        let b = module_bridge(&root);
        let r = b
            .run_module(&api, "get", RequestInfo::default(), std::time::Duration::from_millis(200))
            .await;
        assert!(matches!(r, Err(RunError::Timeout)), "got: {r:?}");
        let cap = b
            .run(r#"json.ok({ alive: true });"#)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["alive"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn syntax_error_returns_core_error_with_position() {
        let (root, api) = mod_fx(&[("u/f/api.ts", "function {{{{\nexport default {};\n")]);
        let b = module_bridge(&root);
        let e = b
            .run_module(&api, "get", RequestInfo::default(), std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("api.ts"), "{}", e.to_string());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p only-js run_module`
Expected: 编译失败（`with_dbs_and_loader`/`run_module` 未定义）。

- [ ] **Step 3: 实现**

`runtime.rs`：`RuntimePool::new(stable, inspect, loader: Option<Arc<crate::LoaderShared>>)`，工厂闭包捕获 loader，`RuntimeOptions` 加：
```rust
module_loader: loader.map(|l| {
    std::rc::Rc::new(crate::module_loader::OjModuleLoader { inner: l }) as std::rc::Rc<dyn deno_core::ModuleLoader>
}),
```
`StableState` 加 `pub loader: Option<Arc<crate::LoaderShared>>`；`with_dbs` 全链路透传。

`mod.rs` 新增：
```rust
    /// 全量命名 DB + 模块加载器构造（oj server 专用路径）。
    pub fn with_dbs_and_loader(
        dbs: HashMap<String, Arc<dyn DataAccessor>>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
        loader: Option<Arc<crate::module_loader::LoaderShared>>,
    ) -> Self { /* 同 with_dbs，StableState.loader 与 RuntimePool::new 均带 loader */ }

    /// ESM 模式执行：TLA driver 模块 import api 模块并调 default[method]。
    /// KillSwitch/ReqState 复用 run_ws 的熔断与捕获路径。
    pub async fn run_module(
        &self,
        api_path: &std::path::Path,
        method: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<Capture, RunError> {
        let spec = crate::module_loader::versioned_specifier(api_path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        static DRIVER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = DRIVER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let driver_spec = deno_core::ModuleSpecifier::parse(&format!("file:///oj/driver/{n}.js"))
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e.to_string()))))?;
        // TLA driver：import 命中 V8 模块缓存（?v= 不变时零转译零重编译）；
        // 方法未导出 → json.fail(405)（envelope 映射 HTTP 405）。
        let code = format!(
            "const m = await import(\"{spec}\");\n\
             const fn = m.default && m.default[\"{method}\"];\n\
             if (typeof fn !== \"function\") json.fail(405, \"method '{method}' not exported by {api}\");\n\
             else await fn();\n",
            spec = spec, method = method, api = api_path.display(),
        );
        let mut rt = self.pool.checkout();
        let ptr: usize = {
            let iso: &mut deno_core::v8::Isolate = &mut *rt.v8_isolate();
            iso as *mut _ as usize
        };
        self.kill.arm(ptr, timeout);
        {
            let op_state = runtime::op_state(&rt);
            let mut g = op_state.borrow_mut();
            g.borrow_mut::<ReqState>().reset(req);
        }
        let result: Result<(), CoreError> = async {
            let id = rt
                .load_main_es_module_from_code(&driver_spec, code)
                .await?;
            let eval = rt.mod_evaluate(id);
            rt.run_event_loop(deno_core::PollEventLoopOptions::default()).await?;
            eval.await?;
            Ok(())
        }
        .await;
        if self.kill.disarm() {
            return Err(RunError::Timeout);
        }
        match result {
            Ok(()) => {
                let capture = {
                    let op_state = runtime::op_state(&rt);
                    let g = op_state.borrow();
                    let rs = g.borrow::<ReqState>();
                    Capture { status: rs.status, headers: rs.headers.clone(),
                              body: rs.response.clone().unwrap_or_default() }
                };
                self.pool.checkin(rt);
                Ok(capture)
            }
            Err(e) => Err(RunError::Core(e)),
        }
    }
```
注意：`mod_evaluate` 返回的 future 持有 `&mut JsRuntime` 借用——若与 `run_event_loop` 借用冲突，改为 deno runtime 的惯用法：先 `let eval = rt.mod_evaluate(id);` 再 `rt.run_event_loop(...).await?;` 然后 `eval.await?;`（future 是独立对象不借 rt——以 0.410 签名 `impl Future + use<>` 为准，编译器裁定顺序）。若 405 场景 `fn` 缺失但模块顶层已执行且 driver 正常结束，`json.fail` 已写 Capture——本测试覆盖。

- [ ] **Step 4: 跑测试确认通过（R1 spike 就此闭环）+ 提交**

Run: `cargo test -p only-js`
Expected: 全绿（含 4 个新测）。若 408 测试挂：先确认 KillSwitch arm 时序（driver 加载也在熔断窗口内——加载阶段被 terminate 也应产生 Timeout），修正 disarm 判定。

```bash
git add src/bridge
git commit -m "feat(bridge): run_module — TLA driver dispatch with KillSwitch 408 and 405 mapping

unix@vip.qq.com ai"
```

---

### Task 10: actor run_module + server 层切换新路由

**Files:**
- Modify: `server/src/actor.rs`（Module 报文 + run_module）
- Modify: `server/src/lib.rs`（handle 走 Routes + run_module；app/serve 签名改）
- Delete: `server/src/router.rs`
- Rewrite: `server/src/lib.rs` tests（新布局）

**Interfaces:**
- Consumes: T4 `Routes/method_name`、T9 `Bridge::run_module`。
- Produces: `mdm_server::{app, serve}` 新签名：`app(base: &str, dir: impl Into<PathBuf>, ts: bool, actor: JsActor, timeout: Option<Duration>) -> Router`；`serve(addr, base, dir, ts, actor, timeout)`。`JsActor::run_module(&self, path: PathBuf, method: String, req: RequestInfo, timeout: Option<Duration>) -> Result<Capture, RunFail>`。T11 消费。

- [ ] **Step 1: 写失败测试**（lib.rs tests 重写为镜像布局）

```rust
    #[tokio::test]
    async fn serves_mirror_route_with_envelope() {
        let t = routes(&[(
            "user/account/api.ts",
            r#"export default { get() { json.ok({ m: http.method, q: http.param("id", 0) }); } };"#,
        )]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(addr,
            "GET /v1/api/user/account/?id=7 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains("\"q\":\"7\""), "{resp}");
        assert!(resp.contains("\"m\":\"GET\""), "{resp}");
    }

    #[tokio::test]
    async fn missing_api_is_404_and_unmapped_verb_405() {
        let t = routes(&[]);
        let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
        let resp = raw_http(addr,
            "GET /v1/api/none/here/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
        // api 文件在但未导出 del → 405。
        let t2 = routes(&[("u/f/api.ts", "export default { get() { json.ok({}); } };")]);
        let addr2 = spawn_server("/v1/api", t2.0, true, None).await;
        let resp2 = raw_http(addr2,
            "DELETE /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
        assert!(resp2.starts_with("HTTP/1.1 405"), "{resp2}");
    }

    #[tokio::test]
    async fn handler_timeout_returns_408_envelope() {
        let t = routes(&[("u/f/api.ts", "export default { get() { while (true) {} } };")]);
        let addr = spawn_server("/v1/api", t.0, true, Some(std::time::Duration::from_millis(200))).await;
        let resp = raw_http(addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 408"), "{resp}");
    }

    #[tokio::test]
    async fn handler_error_returns_500_envelope() {
        let t = routes(&[("u/f/api.ts", "function {{{{\nexport default {};")]);
        let addr = spawn_server("/v1/api", t.0, true, None).await;
        let resp = raw_http(addr,
            "GET /v1/api/u/f/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
        assert!(resp.starts_with("HTTP/1.1 500"), "{resp}");
    }
```
（`spawn_server` 改签名 `(base, dir, ts, timeout)`；`actor()` 工厂需带 loader：`Bridge::with_dbs_and_loader(.., Some(Arc::new(LoaderShared { project_root: dir, ts: true })))`——注意 JsActor::new 工厂捕获 dir clone。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-server`
Expected: 编译失败（签名变化）。

- [ ] **Step 3: 实现**

`actor.rs`：报文枚举加 `Module { path: PathBuf, method: String, req: RequestInfo, timeout: Option<Duration>, reply: oneshot::Sender<Result<Capture, RunFail>> }`；执行侧调 `bridge.run_module(&path, &method, req, timeout.unwrap_or(DEFAULT))`（无限时用远大于配置的值如 3600s——与现有 `run` 的 None 语义对齐：看现有实现如何处理 None，照抄该模式）。公开方法：
```rust
    /// ESM 模块执行（oj server 路径）。
    pub async fn run_module(
        &self,
        path: std::path::PathBuf,
        method: String,
        req: RequestInfo,
        timeout: Option<std::time::Duration>,
    ) -> Result<Capture, RunFail> { /* 同 run 的 channel 往返，发 Module 报文 */ }
```
`lib.rs`：`AppState { routes: Routes, actor, timeout }`；`handle`：
```rust
    let Some(file) = st.routes.resolve(uri.path()) else {
        return fail_response(404, "no api file for route");
    };
    let Some(m) = crate::routes::method_name(method.as_str()) else {
        return fail_response(405, &format!("method {method} not mapped"));
    };
    // RequestInfo 组装同旧（query/headers/body；params 置空 map——新路由无路径参数）。
    match st.actor.run_module(file, m.to_string(), req, st.timeout).await {
        Ok(cap) => capture_response(cap),
        Err(e) if e.timeout => fail_response(408, &e.msg),
        Err(e) => fail_response(500, &e.msg),
    }
```
删除 `server/src/router.rs` 与 `pub mod router;`、旧 tests、`use crate::router::…`。

- [ ] **Step 4: 跑测试确认通过 + workspace（root 的 lib tests 不受影响）+ 提交**

Run: `cargo test --workspace`
Expected: 全绿。

```bash
git add server/src
git commit -m "feat(server): serve via directory-mirror routes and module execution

unix@vip.qq.com ai"
```

---

### Task 11: cli server 装配

**Files:**
- Create: `../../../oj/src/manifest.rs`（含 tests）
- Rewrite: `../../../oj/src/server_cmd.rs`（含 tests）

**Interfaces:**
- Consumes: T1 `ServerArgs`、T3 `config`、T4 `method_table/route_table`、T9 `with_dbs_and_loader`、T10 `serve`。
- Produces: `manifest::{Manifest, load_modules}`；`Manifest { name, desc, version, config }`；`load_modules(dir: &Path) -> Result<Vec<Manifest>, String>`；`server_cmd::{start, run}`；`start(cfg: Config, config_dir: &Path, dir: PathBuf, base: String, ts: bool) -> Result<(std::net::SocketAddr, tokio::task::JoinHandle<()>), String>`（T12/T13 消费）。

- [ ] **Step 1: manifest 失败测试**

```rust
// oj/src/manifest.rs tests
    #[test]
    fn loads_and_validates() {
        let d = tmp("mf-ok");
        write(d.join("user/manifest.yaml"), "name: user\ndesc: d\nversion: 0.1.0\n");
        let ms = load_modules(&d).unwrap();
        assert_eq!(ms[0].name, "user");

        let bad = tmp("mf-bad");
        write(bad.join("order/manifest.yaml"), "name: orderr\ndesc: d\nversion: 0.1.0\n");
        let e = load_modules(&bad).unwrap_err();
        assert!(e.contains("orderr") && e.contains("order"), "{e}");

        let none = tmp("mf-none");
        write(none.join("x/keep.txt"), "");
        let e2 = load_modules(&none).unwrap_err();
        assert!(e2.contains("manifest.yaml"), "{e2}");
    }
```

- [ ] **Step 2: 跑测试确认失败**：`cargo test -p oj manifest` → 编译失败。

- [ ] **Step 3: manifest 实现**

```rust
//! manifest.yaml：模块清单（name 必须等于目录名——启动期强约束）。

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub desc: String,
    pub version: String,
    #[serde(default)]
    pub config: serde_yaml::Value,
}

/// 加载 dir 首层全部模块清单并校验 name==目录名。
pub fn load_modules(dir: &Path) -> Result<Vec<Manifest>, String> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read module dir {}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let dirname = e.file_name().to_string_lossy().into_owned();
        let mf = p.join("manifest.yaml");
        if !mf.is_file() {
            return Err(format!("module '{dirname}' missing manifest.yaml"));
        }
        let m: Manifest = serde_yaml::from_str(
            &std::fs::read_to_string(&mf).map_err(|e| format!("read {mf:?}: {e}"))?,
        )
        .map_err(|e| format!("parse {mf:?}: {e}"))?;
        if m.name != dirname {
            return Err(format!(
                "manifest name {:?} != directory name {:?} (in {})",
                m.name, dirname, p.display()
            ));
        }
        out.push(m);
    }
    Ok(out)
}
```
（tests 辅助 `tmp/write` 按现有 server tests 的 TempDir 模式内联。）

- [ ] **Step 4: server_cmd 失败测试**

```rust
// oj/src/server_cmd.rs tests
    #[tokio::test]
    async fn rejects_non_sqlite_dsn_at_startup() {
        let mut cfg = Config::default();
        cfg.db.insert("default".into(), "mysql://u:p@localhost/test".into());
        let e = start(cfg, Path::new("/tmp"), PathBuf::from("src"), "/v1/api".into(), true)
            .await.err().unwrap_or_default();
        assert!(e.contains("sqlite"), "{e}");
    }

    #[tokio::test]
    async fn manifest_mismatch_blocks_startup() {
        let t = tmpdir("sc-md");
        std::fs::create_dir_all(t.join("src/user")).unwrap();
        std::fs::write(t.join("src/user/manifest.yaml"), "name: x\ndesc: d\nversion: 0.1.0\n").unwrap();
        let e = start(Config::default(), &t, t.join("src"), "/v1/api".into(), true)
            .await.err().unwrap_or_default();
        assert!(e.contains("name"), "{e}");
    }

    #[tokio::test]
    async fn seeds_and_serves_sqlite() {
        let t = tmpdir("sc-seed");
        std::fs::write(t.join("seed.sql"),
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);\n\
             INSERT OR IGNORE INTO t (id, v) VALUES (1, 'a');\n").unwrap();
        let mut cfg = Config::default();
        cfg.server.port = 0; // 随机端口
        cfg.db.insert("default".into(), format!("sqlite://{}/db.sqlite", t.display()));
        let (addr, _h) = start(cfg, &t, t.join("src"), "/v1/api".into(), true).await.unwrap();
        // 直接打一个临时 api.ts 验证全链路。
        std::fs::create_dir_all(t.join("src/u/f")).unwrap();
        std::fs::write(t.join("src/u/f/api.ts"),
            "export default { get() { db.query(\"select v from t where id = ?\", [1]).then(r => json.ok(r)); } };\n").unwrap();
        let resp = reqwest::get(format!("http://{addr}/v1/api/u/f/")).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["data"][0]["v"], "a", "{v}");
    }
```
（`start` 为 async——绑定监听需要 await；`run` 调 `start` 后 await handle。）

- [ ] **Step 5: 跑测试确认失败**：`cargo test -p oj` → 编译失败。

- [ ] **Step 6: server_cmd 实现**

```rust
//! oj server 装配：config → 逐 db 开库（仅 sqlite）→ seed → manifest 校验 →
//! actor 池 → axum serve。start() 返回 (addr, join_handle)，main 与测试共用。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use only_js::bridge::{Bridge, InMemoryKV, LoaderShared, SchemaRegistry, SqlxAccessor};
use only_js::config::{self, Config};
use mdm_server::actor::JsActor;
use mdm_server::routes;

use crate::args::ServerArgs;
use crate::manifest;

pub async fn run(a: ServerArgs) -> Result<(), String> {
    let config_path = PathBuf::from(&a.config);
    let config_dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let cfg = config::load_from(&config_dir, Some(config_path.file_name().and_then(|s| s.to_str())))
        .map_err(|e| format!("load config: {e}"))?;
    let (addr, h) = start(cfg, &config_dir, PathBuf::from(&a.dir), a.base.clone(), a.dev).await?;
    println!("oj server listening on http://{addr}{} (dir={}, {})",
             a.base, a.dir, if a.dev { "dev/ts" } else { "release/js" });
    h.await.map_err(|e| format!("server task: {e}"))
}

/// 装配并监听（port=0 → 随机端口，测试用）。
pub async fn start(
    cfg: Config,
    config_dir: &Path,
    dir: PathBuf,
    base: String,
    ts: bool,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), String> {
    for (name, url) in &cfg.redis {
        eprintln!("warn: redis '{name}' ({url}) configured but served by in-memory KV (v0.1)");
    }
    // 逐 db 开库：v0.1 仅 sqlite，其余 fail-fast。
    let mut dbs: HashMap<String, Arc<dyn SqlxAccessor>> = HashMap::new();
    for (name, dsn) in &cfg.db {
        let acc = SqlxAccessor::arc(&resolve_sqlite(dsn, config_dir)?)
            .await
            .map_err(|e| format!("open db '{name}': {e}"))?;
        dbs.insert(name.clone(), acc);
    }
    // 项目根 seed.sql（存在则对 default 库执行，语句按 ';' 切分——ponytail: seed 内不得有分号字面量）。
    let seed = config_dir.join("seed.sql");
    if seed.is_file() {
        let text = std::fs::read_to_string(&seed).map_err(|e| format!("read seed: {e}"))?;
        if let Some(db) = dbs.get("default") {
            for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                db.exec_with_params(stmt, &[]).await.map_err(|e| format!("seed: {e}"))?;
            }
        }
    }
    // manifest 校验 + 路由表打印（UC-8）。
    for m in manifest::load_modules(&dir)? {
        println!("module {} v{} — {}", m.name, m.version, m.desc);
    }
    for r in routes::route_table(&dir, ts) {
        println!("  {base}/{r}/");
    }
    // 绝对化 dir（Bridge loader 的 project_root 用 config_dir，api 相对 dir）。
    let dir = dir.canonicalize().unwrap_or(dir);
    let loader = Arc::new(LoaderShared { project_root: config_dir.canonicalize().unwrap_or_else(|_| config_dir.to_path_buf()), ts });
    let kv = Arc::new(InMemoryKV::new());
    let dbs: HashMap<String, Arc<dyn only_js::bridge::DataAccessor>> =
        dbs.into_iter().map(|(k, v)| (k, v as _)).collect();
    let n = cfg.server.pool_size.max(1) as usize;
    let timeout = config::parse_duration(&cfg.server.timeout).ok();
    let actor = JsActor::pool(n, {
        let (dbs, kv, loader) = (dbs.clone(), kv.clone(), loader.clone());
        move || Bridge::with_dbs_and_loader(dbs.clone(), kv.clone(), SchemaRegistry::new(), false, Some(loader.clone()))
    });
    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let addr = addr.to_socket_addrs_sync()?; // localhost → 127.0.0.1 解析
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind: {e}"))?;
    let bound = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
    let h = tokio::spawn(async move {
        let _ = mdm_server::serve_with_listener(listener, &base, dir, ts, actor, timeout).await;
    });
    Ok((bound, h))
}

/// DSN 归一：非 sqlite 报错；相对路径相对 config_dir；内存库原样。
fn resolve_sqlite(dsn: &str, config_dir: &Path) -> Result<String, String> {
    let rest = dsn.strip_prefix("sqlite://").or_else(|| {
        if dsn == "sqlite::memory:" { Some("") } else { None }
    });
    let Some(rest) = rest else {
        return Err(format!("v0.1 supports only sqlite:// DSN (got '{dsn}')"));
    };
    if rest.is_empty() {
        return Ok("sqlite::memory:".into()); // sqlite://（空）视作内存
    }
    if rest.starts_with(':') || rest.starts_with("//") {
        return Ok(dsn.to_string());
    }
    let p = Path::new(rest);
    if p.is_absolute() {
        Ok(dsn.to_string())
    } else {
        Ok(format!("sqlite://{}", config_dir.join(p).display()))
    }
}
```
注意实现细节：`mdm_server::serve`（T10）绑定在内部——为让 start() 拿到随机端口的 addr，`server/src/lib.rs` 增 `pub async fn serve_with_listener(listener, base, dir, ts, actor, timeout)`，`serve()` 变为其薄壳（bind 后转发）。`to_socket_addrs_sync` 用 `std::net::ToSocketAddrs`（`use std::net::ToSocketAddrs;`，阻塞 resolve 一次性启动开销可接受——ponytail 注释）。`SqlxAccessor::arc` 返回 `Arc<SqlxAccessor>` 还是 `Arc<dyn DataAccessor>` 以现有签名为准（devserver 旧代码 `SqlxAccessor::arc(&dsn).await` → `Arc<dyn DataAccessor>`，按旧代码写法照抄，去掉这里的类型歧义：直接 `HashMap<String, Arc<dyn DataAccessor>>`）。

- [ ] **Step 7: 跑测试确认通过 + 提交**

Run: `cargo test -p oj && cargo test --workspace`
Expected: 全绿。

```bash
git add oj server/src/lib.rs
git commit -m "feat(oj): server assembly (sqlite-only DSN, seed, manifest gate, route table)

unix@vip.qq.com ai"
```

---

### Task 12: sample 全量文件

**Files:**
- Create: `sample/config.yaml`、`sample/seed.sql`、`sample/package.json`、`sample/.gitignore`、`sample/README.md`
- Create: `sample/src/user/manifest.yaml`、`sample/src/user/account/api.ts`、`sample/src/user/profile/api.ts`、`sample/src/user/profile/detail/api.ts`、`sample/src/user/_shared/validate.ts`
- Create: `sample/src/order/manifest.yaml`、`sample/src/order/account/api.ts`、`sample/src/order/list/api.ts`、`sample/src/order/detail/api.ts`
- Create: `sample/node_modules/escape-goat/package.json`、`sample/node_modules/escape-goat/index.js`
- Create: `sample/dist/**`（src 全镜像的 .js 版 + 两份 manifest.yaml 拷贝）
- Modify: spec 文件（nanoid → escape-goat 偏差记录）

**Interfaces:**
- Consumes: 全部前置。
- Produces: T13 的验收载体。**偏差**：vendored 包用 `escape-goat`（纯 ESM、零依赖、纯字符串操作）替代 spec 原文的 nanoid——裸 deno_core 无 `crypto.getRandomValues`（Web API 由 Deno CLI 的扩展提供，core 不含），nanoid v5 依赖它；escape-goat 无此依赖。UC-15 语义不变（裸 specifier 参与请求处理）。

- [ ] **Step 1: 基础文件**

`sample/config.yaml`：
```yaml
server:
  host: "localhost"
  port: 778
db:
  default: "sqlite://db.sqlite"
redis:
  default: "redis://127.0.0.1:6379/1"   # v0.1: warn + 内存 KV 模拟
```
`sample/seed.sql`：
```sql
CREATE TABLE IF NOT EXISTS account (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, role TEXT NOT NULL)
CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY AUTOINCREMENT, no TEXT NOT NULL, account_id INTEGER NOT NULL, amount REAL NOT NULL)
INSERT OR IGNORE INTO account (id, name, role) VALUES (1, 'neo', 'admin')
INSERT OR IGNORE INTO account (id, name, role) VALUES (2, 'trinity', 'user')
INSERT OR IGNORE INTO orders (id, no, account_id, amount) VALUES (1, 'A-0001', 1, 99.5)
INSERT OR IGNORE INTO orders (id, no, account_id, amount) VALUES (2, 'A-0002', 2, 0.5)
```
`sample/.gitignore`：`db.sqlite`（+ `db.sqlite-*`）。
`sample/package.json`：
```json
{ "name": "oj-sample", "private": true, "type": "module",
  "dependencies": { "escape-goat": "4.0.0" } }
```
`sample/node_modules/escape-goat/package.json`：
```json
{ "name": "escape-goat", "version": "4.0.0", "type": "module",
  "description": "vendored copy (MIT) for oj sample; replace with npm install if desired" }
```
`sample/node_modules/escape-goat/index.js`（MIT，原文照抄 escape-goat 4.0.0 实现）：
```js
export function escapeHtml(string) {
	return string
		.replace(/&/g, '&amp;')
		.replace(/"/g, '&quot;')
		.replace(/'/g, '&#39;')
		.replace(/</g, '&lt;')
		.replace(/>/g, '&gt;');
}

export function unescapeHtml(htmlString) {
	return htmlString
		.replace(/&gt;/g, '>')
		.replace(/&lt;/g, '<')
		.replace(/&#39;/g, '\'')
		.replace(/&quot;/g, '"')
		.replace(/&amp;/g, '&');
}
```
`sample/README.md`：
```markdown
# oj sample — user/order

  cargo run -p oj -- server -c sample/config.yaml --dev     # dev（TS，热重载）
  curl http://localhost:778/v1/api/user/account/?id=1

  cargo run -p oj -- server -c sample/config.yaml           # release（dist 手写制品）

- 路由 = 目录镜像：src/user/profile/detail/api.ts → /v1/api/user/profile/detail/
- node_modules/escape-goat 为直接 vendor 的纯 ESM 包（可 npm install 替换）
- db.sqlite 由 seed.sql 初始化（幂等），已 gitignore
```

- [ ] **Step 2: user 模块**

`sample/src/user/manifest.yaml`：
```yaml
name: "user"
desc: "用户信息相关，记录账号、地址等个人信息"
version: "0.1.0"
```
`sample/src/user/_shared/validate.ts`：
```ts
// 模块内共享校验（无 api.ts → 非路由目录，UC-13 相对导入载体）
export function requireRole(role: string | undefined): string {
  if (role !== "admin" && role !== "user") throw new Error("invalid role: " + role);
  return role;
}

export function positiveId(raw: unknown): number {
  const n = Number(raw);
  if (!Number.isInteger(n) || n <= 0) throw new Error("invalid id: " + String(raw));
  return n;
}
```
`sample/src/user/account/api.ts`（UC-1/2/3/13）：
```ts
import { positiveId, requireRole } from "../_shared/validate";

function get(): void {
  const id = Number(http.param("id", 0));
  const rows = id > 0
    ? db.query("select id, name, role from account where id = ?", [id])
    : db.query("select id, name, role from account", []);
  rows.then((r) => json.ok(r)).catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { name?: string; role?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  const role = (() => { try { return requireRole(b.role ?? "user"); } catch (e) { return ""; } })();
  if (!role) { json.fail(400, "role must be admin|user"); return; }
  db.exec("insert into account (name, role) values (?, ?)", [b.name, role])
    .then(() => json.ok({ created: true }))
    .catch((e) => json.fail(500, String(e)));
}

function put(): void {
  const b = http.body as { id?: number; name?: string };
  const id = (() => { try { return positiveId(b.id); } catch { return 0; } })();
  if (!id || !b.name) { json.fail(400, "id and name required"); return; }
  db.exec("update account set name = ? where id = ?", [b.name, id])
    .then(() => json.ok({ updated: true }))
    .catch((e) => json.fail(500, String(e)));
}

function del(): void {
  const id = positiveId(http.param("id", 0));
  db.exec("delete from account where id = ?", [id])
    .then(() => json.ok({ deleted: true }))
    .catch((e) => json.fail(500, String(e)));
}

function patch(): void {
  const b = http.body as { id?: number; role?: string };
  const role = requireRole(b.role);
  db.exec("update account set role = ? where id = ?", [role, positiveId(b.id)])
    .then(() => json.ok({ patched: true }))
    .catch((e) => json.fail(500, String(e)));
}

function head(): void { get(); }

function options(): void {
  json.ok({ methods: ["get", "post", "put", "del", "patch", "head", "options"] });
}

export default { get, post, put, del, patch, head, options };
```
注意：`db.query/exec` 返回 Promise（op async）——所有 handler 用 `.then/.catch` 落信封（`await fn()` 在 driver 里等 Promise 链落定，事件循环泵到完）。

`sample/src/user/profile/api.ts`（UC-3 body）：
```ts
function get(): void {
  const id = Number(http.param("id", 0));
  db.query("select id, name, role from account where id = ?", [id])
    .then((r) => json.ok(r[0] ?? null))
    .catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { id?: number; name?: string };
  if (!b.id || !b.name) { json.fail(400, "id and name required"); return; }
  db.exec("update account set name = ? where id = ?", [b.name, b.id])
    .then(() => json.ok({ renamed: true }))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
```
`sample/src/user/profile/detail/api.ts`（UC-4 三层）：
```ts
function get(): void {
  json.ok({ path: "user/profile/detail", depth: 3, ts: true });
}

export default { get };
```

- [ ] **Step 3: order 模块**

`sample/src/order/manifest.yaml`：
```yaml
name: "order"
desc: "订单：建单、列表联查、详情缓存"
version: "0.1.0"
```
`sample/src/order/account/api.ts`（UC-15 裸 specifier）：
```ts
import { escapeHtml } from "escape-goat";

function post(): void {
  const b = http.body as { account_id?: number; amount?: number; no?: string };
  if (!b.account_id || !b.amount || !b.no) {
    json.fail(400, "account_id, amount, no required");
    return;
  }
  const no = escapeHtml(String(b.no)); // 裸 specifier 参与请求处理（UC-15）
  db.exec("insert into orders (no, account_id, amount) values (?, ?, ?)",
          [no, b.account_id, b.amount])
    .then(() => json.ok({ created: true, no }))
    .catch((e) => json.fail(500, String(e)));
}

function get(): void {
  const id = Number(http.param("id", 0));
  db.query("select id, no, account_id, amount from orders where account_id = ?", [id])
    .then((r) => json.ok(r))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
```
`sample/src/order/list/api.ts`（UC-5 联查 + UC-13 跨模块导入）：
```ts
import { requireRole } from "../../user/_shared/validate";

function get(): void {
  const role = requireRole(http.param("role", "admin")); // 跨模块相对导入（UC-13）
  db.query(
    `select o.id, o.no, o.amount, a.name as account_name, a.role
     from orders o join account a on a.id = o.account_id
     where a.role = ? order by o.id`,
    [role],
  )
    .then((r) => json.ok(r))
    .catch((e) => json.fail(500, String(e)));
}

export default { get };
```
`sample/src/order/detail/api.ts`（UC-9 KV 缓存）：
```ts
const key = (id: string) => "order:detail:" + id;

function get(): void {
  const id = http.param("id", "0");
  kv.get(key(id)).then((hit) => {
    if (hit !== null) {
      json.ok({ cached: true, data: JSON.parse(hit) });
      return;
    }
    db.query("select id, no, account_id, amount from orders where id = ?", [Number(id)])
      .then((rows) => {
        const row = rows[0] ?? null;
        kv.set(key(id), JSON.stringify(row)).then(() =>
          json.ok({ cached: false, data: row })
        );
      })
      .catch((e) => json.fail(500, String(e)));
  });
}

export default { get };
```

- [ ] **Step 4: dist 手写制品（UC-6）**

`sample/dist/` 与 `src/` 同构：两份 `manifest.yaml` 原样拷贝；`_shared/validate.js`、各 `api.js` 为去类型注解、`import` 路径不变的等价 JS（`.ts` → `.js` 后缀）。例 `sample/dist/user/account/api.js` 首行 `import { positiveId, requireRole } from "../_shared/validate.js";`——**注意**：相对导入补全规则（as-is → +.js）下 `../_shared/validate` 与 `../_shared/validate.js` 均可命中；统一写 `../_shared/validate.js`（与 vite 产物习惯一致）。裸导入 `escape-goat` 不变（dist 下 node_modules 回溯同样命中项目根）。

- [ ] **Step 5: spec 偏差记录 + 提交**

`docs/superpowers/specs/2026-08-22-oj-server-sample-design.md`：UC-15 与 §4 的 nanoid 改为 escape-goat，并加一行偏差说明（deno_core 无 crypto.getRandomValues）。

```bash
git add sample docs/superpowers/specs
git commit -m "feat(sample): user/order modules with manifests, seed, vendored escape-goat, dist mirror

unix@vip.qq.com ai"
```

---

### Task 13: E2E — sample 用例集

**Files:**
- Create: `../../../oj/tests/e2e.rs`

**Interfaces:**
- Consumes: T11 `server_cmd::start`、T6 `transpile_hits`、T12 sample 文件。

- [ ] **Step 1: 写全部用例测试（一次性写全，逐 UC 断言）**

```rust
//! E2E：sample 作为 oj server 验收载体（spec UC-1~6,8,9,13,14,15）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use oj::server_cmd;
use only_js::bridge::transpile::transpile_hits;
use only_js::config::Config;

fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../sample").canonicalize().unwrap()
}

async fn boot(dev: bool) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, PathBuf) {
    // 共享 db.sqlite 会串用例：每个 boot 复制一份独立 db 文件到临时目录。
    let tmp = std::env::temp_dir().join(format!(
        "oj-e2e-{}-{}", std::process::id(), dev as u32));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // seed.sql 拷到临时 config_dir；db 用独立文件。
    std::fs::copy(sample().join("seed.sql"), tmp.join("seed.sql")).unwrap();
    let mut cfg: Config = serde_yaml::from_str(&std::fs::read_to_string(sample().join("config.yaml")).unwrap()).unwrap();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), format!("sqlite://{}/db.sqlite", tmp.display()));
    let dir = sample().join(if dev { "src" } else { "dist" });
    let (addr, h) = server_cmd::start(cfg, &tmp, dir, "/v1/api".into(), dev).await.unwrap();
    (addr, h, tmp)  // tmp 供个别用例改文件（UC-14 用独立临时项目）
}

async fn req(addr: std::net::SocketAddr, method: &str, path: &str, body: Option<&str>)
    -> (u16, serde_json::Value) {
    let c = reqwest::Client::new();
    let mut r = c.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                          format!("http://{addr}{path}"));
    if let Some(b) = body { r = r.header("content-type", "application/json").body(b.to_string()); }
    let resp = r.send().await.unwrap();
    let status = resp.status().as_u16();
    let v = resp.json().await.unwrap();
    (status, v)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc1_method_table() {
    let (addr, _h, _t) = boot(true).await;
    for m in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
        let (s, _) = req(addr, m, "/v1/api/user/account/", None).await;
        assert_eq!(s, 200, "{m}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc2_uc3_crud_params_body() {
    let (addr, _h, _t) = boot(true).await;
    let name = format!("u-{}", std::process::id());
    // POST body 建号。
    let (s, v) = req(addr, "POST", "/v1/api/user/account/",
        Some(&format!(r#"{{"name":"{name}","role":"admin"}}"#))).await;
    assert_eq!((s, v["code"]), (200, 0), "{v}");
    // query 参数查回。
    let (s, v) = req(addr, "GET", &format!("/v1/api/user/account/?id=1"), None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"][0]["name"], "neo", "{v}");
    // PUT 改名后 GET 验证。
    let _ = req(addr, "PUT", "/v1/api/user/account/", Some(r#"{"id":1,"name":"neo2"}"#)).await;
    let (_, v) = req(addr, "GET", "/v1/api/user/account/?id=1", None).await;
    assert_eq!(v["data"][0]["name"], "neo2", "{v}");
    let (s, _) = req(addr, "DELETE", "/v1/api/user/account/?id=2", None).await;
    assert_eq!(s, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc4_nested_route_uc5_join() {
    let (addr, _h, _t) = boot(true).await;
    let (s, v) = req(addr, "GET", "/v1/api/user/profile/detail/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["depth"], 3, "{v}");
    let (s, v) = req(addr, "GET", "/v1/api/order/list/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"][0]["account_name"], "neo", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc6_release_mode_dist() {
    let (addr, _h, _t) = boot(false).await;
    let (s, v) = req(addr, "GET", "/v1/api/user/account/?id=1", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"][0]["name"], "neo", "{v}");
    let (s, v) = req(addr, "GET", "/v1/api/order/list/", None).await;
    assert_eq!((s, v["data"][0]["account_name"].as_str().unwrap()), (200, "neo"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc9_kv_cache_read_through() {
    let (addr, _h, _t) = boot(true).await;
    let (_, v1) = req(addr, "GET", "/v1/api/order/detail/?id=1", None).await;
    assert_eq!(v1["data"]["cached"], false, "{v1}");
    let (_, v2) = req(addr, "GET", "/v1/api/order/detail/?id=1", None).await;
    assert_eq!(v2["data"]["cached"], true, "{v2}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc13_uc15_imports_and_bare() {
    let (addr, _h, _t) = boot(true).await;
    // 裸 specifier：建单时 escapeHtml 生效（<script> 被转义）。
    let (s, v) = req(addr, "POST", "/v1/api/order/account/",
        Some(r#"{"account_id":1,"amount":9.9,"no":"<script>x</script>"}"#)).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["no"], "&lt;script&gt;x&lt;/script&gt;", "{v}");
    // 跨模块相对导入（requireRole）过滤 role=user（只回 trinity 的单）。
    let (_, v) = req(addr, "GET", "/v1/api/order/list/?role=user", None).await;
    assert_eq!(v["data"][0]["account_name"], "trinity", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc14_transpile_cache_and_hot_reload() {
    // 独立临时项目（不动 sample 文件）。
    let t = std::env::temp_dir().join(format!("oj-hot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/u/f")).unwrap();
    std::fs::write(t.join("src/u/f/api.ts"), "export default { get() { json.ok({ v: 1 }); } };\n").unwrap();
    let mut cfg = Config::default();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    std::fs::write(t.join("seed.sql"), "").unwrap();
    let (addr, _h, _x) = server_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await.unwrap();
    let before = transpile_hits();
    for _ in 0..3 {
        let (_, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
        assert_eq!(v["data"]["v"], 1);
    }
    // 3 次请求只发生 1 次实际转译（转译缓存全局共享，跨 actor）。
    assert_eq!(transpile_hits(), before + 1);
    // 热重载：改文件 → mtime 变 → 下次请求新结果。
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(t.join("src/u/f/api.ts"), "export default { get() { json.ok({ v: 2 }); } };\n").unwrap();
    let (_, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
    assert_eq!(v["data"]["v"], 2, "{v}");
    let _ = std::fs::remove_dir_all(&t);
}
```
（`oj::server_cmd` 与 `only_js::bridge::transpile` 需 pub 可达：`../../../oj/src/main.rs` 加 `pub mod server_cmd;` 等 pub 声明；transpile 模块 `pub mod transpile;`。boot 里 `tmp` 返回值给 UC-14 用独立目录，本例直接内联 start 调用。）

- [ ] **Step 2: 跑测试确认通过（允许的失败逐个修）**

Run: `cargo test -p oj --test e2e`
Expected: 全绿。常见坑：HEAD 响应 body 被要求解析 JSON——reqwest 对 HEAD 返回空 body，`resp.json()` 会挂：HEAD 用例只断言 status（把 `uc1` 中 HEAD 分支单独走 status-only 断言）。

- [ ] **Step 3: 提交**

```bash
git add oj
git commit -m "test(oj): sample e2e acceptance (UC-1..6,8,9,13,14,15)

unix@vip.qq.com ai"
```

---

### Task 14: E2E 临时目录用例 + 收尾

**Files:**
- Modify: `../../../oj/tests/e2e.rs`（追加 UC-7/10/11/12）
- Modify: `docs/cli2.md`（实现状态标注）
- Modify: `docs/superpowers/specs/2026-08-22-oj-server-sample-design.md`（收官注记）

**Interfaces:**
- Consumes: 同 T13。

- [ ] **Step 1: 追加失败/负向用例**

```rust
fn tmp_project(files: &[(&str, &str)]) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!(
        "oj-neg-{}-{}", std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    for (rel, c) in files {
        let p = t.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }
    t
}

fn base_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.server.port = 0;
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc7_manifest_mismatch_blocks_startup() {
    let t = tmp_project(&[("src/order/manifest.yaml", "name: orderr\ndesc: d\nversion: 0.1.0\n")]);
    let e = server_cmd::start(base_cfg(), &t, t.join("src"), "/v1/api".into(), true)
        .await.err().unwrap_or_default();
    assert!(e.contains("orderr") && e.contains("order"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc10_404_and_405() {
    let t = tmp_project(&[("src/u/f/api.ts", "export default { get() { json.ok({}); } };\n")]);
    let (addr, _h, _x) = server_cmd::start(base_cfg(), &t, t.join("src"), "/v1/api".into(), true)
        .await.unwrap();
    let (s, _) = req(addr, "GET", "/v1/api/none/here/", None).await;
    assert_eq!(s, 404);
    let (s, v) = req(addr, "DELETE", "/v1/api/u/f/", None).await;
    assert_eq!(s, 405);
    assert!(v["msg"].as_str().unwrap().contains("del"), "{v}");
    // 目录穿越按 404。
    let (s, _) = req(addr, "GET", "/v1/api/../etc/", None).await;
    assert_eq!(s, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc11_compile_error_envelope() {
    let t = tmp_project(&[("src/u/f/api.ts", "function {{{{\nexport default {};\n")]);
    let (addr, _h, _x) = server_cmd::start(base_cfg(), &t, t.join("src"), "/v1/api".into(), true)
        .await.unwrap();
    let (s, v) = req(addr, "GET", "/v1/api/u/f/", None).await;
    assert_eq!(s, 500);
    assert!(v["msg"].as_str().unwrap_or("").contains("api.ts"), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uc12_timeout_408_server_survives() {
    let t = tmp_project(&[("src/u/loop/api.ts", "export default { get() { while (true) {} } };\n"),
                          ("src/u/ok/api.ts", "export default { get() { json.ok({ alive: true }); } };\n")]);
    let mut cfg = base_cfg();
    cfg.server.timeout = "300ms".into();
    let (addr, _h, _x) = server_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await.unwrap();
    let (s, _) = req(addr, "GET", "/v1/api/u/loop/", None).await;
    assert_eq!(s, 408);
    let (s, v) = req(addr, "GET", "/v1/api/u/ok/", None).await;
    assert_eq!(s, 200);
    assert_eq!(v["data"]["alive"], true, "{v}");
}
```

- [ ] **Step 2: 跑全量 + 双绿**

Run: `cargo test --workspace && cargo test --workspace --release`
Expected: debug/release 全绿（根 + server + oj，含 E2E）。

- [ ] **Step 3: 文档收官 + 提交**

`docs/cli2.md` 末尾加实现记录段（commit 链、escape-goat 偏差、双绿数字）；spec 加收官注记。

```bash
git add oj docs
git commit -m "test(oj): negative-path e2e (manifest/404/405/500/408) and docs closeout

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec 覆盖**：UC-1~15 → T13/T14；路由镜像/安全 → T4；manifest → T11；TS/缓存 → T6/T9/T13(uc14)；裸 specifier/CJS → T7/T8/T12/T13(uc13/15)；config/DSN → T3/T11；删除旧资产 → T2/T10；双绿 → T14。§5.7 错误表全部有对应测试（404/405/500/408）。
- **偏差**：vendored 包 escape-goat 替代 nanoid（crypto.getRandomValues 不存在于裸 deno_core）——T12 内含 spec 修订步骤。
- **类型一致性**：`LoaderShared{project_root,ts}`、`versioned_specifier`、`run_module(path,method,req,timeout)`、`Routes::resolve`、`method_name`、`start(cfg,config_dir,dir,base,ts)` 跨任务签名已对齐；deno_core 0.410 具体 API（`ModuleSource::new` 参数序、`mod_evaluate` future 借用）在 T7/T9 标注了以编译器/测试为准的调整点。
