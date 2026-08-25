# 插件系统实施计划：JS 绑定层全量插件化（cdylib 动态库装配）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 db/blob/bus/kv/es 五个后端轴从编译期写死改造为「core 持 op + 注册表，插件为纯工厂 cdylib，启动期 libloading 装配」的插件系统。

**Architecture:** 四层：装配层（oj/server，只解析配置调 PluginLoader）→ 插件层（oj-db-mysql 等 cdylib，只依赖 oj-plugin-ffi + 各自 SDK）→ 框架层（core：五注册表 + PluginLoader + ffi.rs 全部 unsafe）→ 契约层（oj-plugin-ffi：stabby repr(C) 容器、vtable、FfiFuture、HostContext、PluginDescriptor、入口宏）。ops 全部留 core，插件不含 Extension。完整设计见 spec：`docs/superpowers/specs/2026-08-25-plugin-system-design.md`（下称"spec"），冲突以 spec 为准。

**Tech Stack:** Rust edition 2024 / deno_core 0.410 / sqlx 0.9 / reqwest 0.13 / tokio 1 / libloading + stabby（spike 定版）/ cargo-xtask。

## Global Constraints

- **ABI_VERSION（u32，严格相等）是唯一硬门禁**；构建指纹不匹配仅告警（spec §3）。
- 插件必须 `panic=unwind` 编译；一切 host→plugin 调用经契约入口宏内建 `catch_unwind` 收敛为 RResult（spec §3）。
- `Library` 句柄加载成功**立即 `mem::forget`**，任何路径不 dlclose；「加载 + forget」封装为 ffi.rs 单一函数（spec §决策表）。
- 插件唯一形态 = 纯工厂 cdylib；**只有 `oj-plugin-ffi` 的类型允许跨边界**；tokio/tracing 不跨界（spec §3）。
- 插件自建 tokio runtime；跨边界 async = FFI 同步方法返回 FfiFuture 句柄；宿主不注入 spawn（spec §3）。
- 产物布局 `plugins/<target-triple>/lib<name>.{so,dylib}` / `<name>.dll`；加载路径四级：`OJ_PLUGINS_DIR` > `oj.toml plugins_dir` > `<exe>/plugins` > build.rs dev 后备；相对路径一律相对 oj.toml 所在目录（spec §4）。
- 注册冲突（重名/scheme 交集）一律启动期 fail fast，无静默降级（spec §6）。
- **每步可编译、全测试绿**；TDD：先写失败测试再实现；每任务结束 commit，提交信息尾注 `unix@vip.qq.com ai`。
- 测试命令约定：`cargo test -p mdm-base-rust <过滤词>`。**已知存量问题**：`bridge::tests::infinite_loop_times_out_and_bridge_survives`（src/bridge/mod.rs:995）在 master 上即 SIGSEGV，与本计划无关——跑全套测试一律加 `-- --skip infinite_loop`。
- env-gated 集成测试（`OJ_TEST_S3`/`OJ_TEST_ES`/`OJ_TEST_REDIS`/`OJ_TEST_MYSQL`/`OJ_TEST_PG`）保留，未设 env 时 skip。
- **每阶段最后一个任务 = 更新任务状态**：勾选本阶段全部复选框、git commit 进度（消息尾注同上）。正式验收与 review 集中在阶段 6，中间阶段只做工程纪律级的编译+测试绿。

---

## 阶段 0 — 五轴注册表（静态链接，行为不变）

目标：五轴统一为注册表解析，es 抽 trait，Extras/StableState 改注册表载体。**JS 行为、配置格式、op 签名全部不变。**

### Task 0.1: NamedRegistry\<T\> 公共注册表

**Files:**
- Create: `src/bridge/named_registry.rs`
- Modify: `src/bridge/mod.rs`（`pub mod named_registry;` + re-export）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Produces（后续全部任务依赖）:

```rust
// src/bridge/named_registry.rs
use std::collections::HashMap;
use std::sync::Arc;
use super::BridgeResult;

/// 五轴注册表公共件：存储 + 重名 fail fast + 自省遍历。
/// 各轴在其上包一层实现自己的冲突/认领语义（spec §2 泛型化裁决）。
pub struct NamedRegistry<T> {
    items: HashMap<String, Arc<T>>,
    order: Vec<String>, // 注册顺序，自省展示用
}

impl<T> Default for NamedRegistry<T> {
    fn default() -> Self { Self::new() }
}

impl<T> NamedRegistry<T> {
    pub fn new() -> Self {
        Self { items: HashMap::new(), order: Vec::new() }
    }
    /// 重名 → Err（插件 vs 插件、插件 vs 内置均不允许覆盖，spec §2 注册冲突语义）。
    pub fn register(&mut self, name: &str, item: Arc<T>) -> BridgeResult<()> {
        if self.items.contains_key(name) {
            return Err(format!("registry: duplicate name '{name}'").into());
        }
        self.items.insert(name.to_string(), item);
        self.order.push(name.to_string());
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<Arc<T>> {
        self.items.get(name).cloned()
    }
    pub fn contains(&self, name: &str) -> bool {
        self.items.contains_key(name)
    }
    /// 按注册顺序遍历名字（op_plugins 自省用）。
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.order.iter().map(String::as_str)
    }
    pub fn len(&self) -> usize { self.items.len() }
}
```

- [x] **Step 1: 写失败测试**（同文件 `#[cfg(test)] mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_get_and_names_in_order() {
        let mut r = NamedRegistry::new();
        r.register("b", Arc::new(2)).unwrap();
        r.register("a", Arc::new(1)).unwrap();
        assert_eq!(*r.get("a").unwrap(), 1);
        assert_eq!(r.names().collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(r.len(), 2);
        assert!(r.get("missing").is_none());
    }

    #[test]
    fn duplicate_name_fails() {
        let mut r = NamedRegistry::new();
        r.register("x", Arc::new(1)).unwrap();
        let e = r.register("x", Arc::new(2)).unwrap_err();
        assert!(e.to_string().contains("duplicate name 'x'"));
        assert_eq!(*r.get("x").unwrap(), 1); // 未被覆盖
    }
}
```

- [x] **Step 2: 跑测试确认编译失败**

Run: `cargo test -p mdm-base-rust named_registry`
Expected: FAIL（`named_registry` 模块不存在）

- [x] **Step 3: 实现**（上方 Produces 代码全文写入，mod.rs 加 `pub mod named_registry; pub use named_registry::NamedRegistry;`）

- [x] **Step 4: 跑测试确认通过**

Run: `cargo test -p mdm-base-rust named_registry -- --skip infinite_loop`
Expected: 2 passed

- [x] **Step 5: Commit**

```bash
git add src/bridge/named_registry.rs src/bridge/mod.rs
git commit -m "feat(bridge): NamedRegistry<T> 公共注册表——重名 fail fast + 注册序自省

unix@vip.qq.com ai"
```

### Task 0.2: DbBackend trait + DbBackendRegistry + 内置后端

**Files:**
- Create: `src/bridge/db_backend.rs`
- Modify: `src/bridge/mod.rs`（`pub mod db_backend;`）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: `NamedRegistry`（Task 0.1）不直接用——db 是认领式，用有序 Vec；`DataAccessor`/`InMemoryAccessor`（db.rs）、`SqlxAccessor`（accessor_sqlx.rs）、`Dialect`/`dialect_of`（db.rs:33）。
- Produces:

```rust
// src/bridge/db_backend.rs
use std::path::{Path, PathBuf};
use std::sync::Arc;
use async_trait::async_trait;
use super::accessor_sqlx::SqlxAccessor;
use super::db::{DataAccessor, InMemoryAccessor};
use super::BridgeResult;

/// db 轴后端工厂（认领式）：按 DSN scheme 认领连接（spec §2）。
#[async_trait]
pub trait DbBackend: Send + Sync {
    fn name(&self) -> &str;
    /// 认领的 scheme 前缀列表（如 &["mysql://"]）；注册时做交集检查。
    fn schemes(&self) -> &'static [&'static str];
    /// config_dir：sqlite 相对路径归一化的基准（oj.toml 所在目录）。
    async fn connect(&self, dsn: &str, config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>>;
}

/// 有序注册表：首个 scheme 命中者胜出；未知 scheme 明确报错（spec §2 resolve_dsn 归属）。
#[derive(Default)]
pub struct DbBackendRegistry {
    backends: Vec<Arc<dyn DbBackend>>,
}

impl DbBackendRegistry {
    pub fn new() -> Self { Self::default() }
    /// 内置四后端：sqlite / mysql / postgres / memory（顺序即优先级）。
    pub fn builtin() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(SqliteBackend)).unwrap();
        r.register(Arc::new(MySqlBackend)).unwrap();
        r.register(Arc::new(PostgresBackend)).unwrap();
        r.register(Arc::new(MemoryBackend)).unwrap();
        r
    }
    /// scheme 交集冲突 → fail fast（含插件 vs 内置）。
    pub fn register(&mut self, b: Arc<dyn DbBackend>) -> BridgeResult<()> {
        for existing in &self.backends {
            for s in b.schemes() {
                if existing.schemes().contains(s) {
                    return Err(format!(
                        "db backend '{}': scheme '{s}' already claimed by '{}'",
                        b.name(), existing.name()
                    ).into());
                }
            }
        }
        self.backends.push(b);
        Ok(())
    }
    /// 无认领 → 未知 scheme 报错（列出已知 scheme 便于排障）。
    pub async fn connect(&self, dsn: &str, config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        for b in &self.backends {
            if b.schemes().iter().any(|s| dsn.starts_with(s)) {
                return b.connect(dsn, config_dir).await;
            }
        }
        let known: Vec<_> = self.backends.iter().flat_map(|b| b.schemes()).collect();
        Err(format!("unknown db scheme in dsn '{dsn}' (known: {known:?})").into())
    }
    /// 自省：已注册后端名（op_plugins 用）。
    pub fn backend_names(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }
}

pub struct SqliteBackend;
#[async_trait]
impl DbBackend for SqliteBackend {
    fn name(&self) -> &str { "sqlite" }
    fn schemes(&self) -> &'static [&'static str] { &["sqlite://", "sqlite:"] }
    async fn connect(&self, dsn: &str, config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(SqlxAccessor::arc(&normalize_sqlite_dsn(dsn, config_dir)?).await?)
    }
}

pub struct MySqlBackend;
#[async_trait]
impl DbBackend for MySqlBackend {
    fn name(&self) -> &str { "mysql" }
    fn schemes(&self) -> &'static [&'static str] { &["mysql://"] }
    async fn connect(&self, dsn: &str, _config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(SqlxAccessor::arc(dsn).await?)
    }
}

pub struct PostgresBackend;
#[async_trait]
impl DbBackend for PostgresBackend {
    fn name(&self) -> &str { "postgres" }
    fn schemes(&self) -> &'static [&'static str] { &["postgres://", "postgresql://"] }
    async fn connect(&self, dsn: &str, _config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(SqlxAccessor::arc(dsn).await?)
    }
}

pub struct MemoryBackend;
#[async_trait]
impl DbBackend for MemoryBackend {
    fn name(&self) -> &str { "memory" }
    fn schemes(&self) -> &'static [&'static str] { &["memory://", ":memory:"] }
    async fn connect(&self, _dsn: &str, _config_dir: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
        Ok(Arc::new(InMemoryAccessor::new()))
    }
}
```

- [x] **Step 1: 先读现状**——读 `oj/src/server_cmd.rs:280-300` 的 `resolve_dsn` 全文与其测试（`server_cmd.rs:385` 起），把 sqlite 归一化规则（相对路径相对 config_dir 绝对化、`sqlite::memory:` 直通、建空库等）原样提炼为纯函数。

- [x] **Step 2: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builtin_connects_sqlite_memory_and_memory() {
        let r = DbBackendRegistry::builtin();
        let dir = std::path::Path::new("/tmp");
        r.connect("sqlite::memory:", dir).await.unwrap();
        r.connect("memory://x", dir).await.unwrap();
        assert_eq!(r.backend_names(), ["sqlite", "mysql", "postgres", "memory"]);
    }

    #[tokio::test]
    async fn unknown_scheme_errors_with_known_list() {
        let r = DbBackendRegistry::builtin();
        let e = r.connect("oracle://x", std::path::Path::new("/tmp")).await.unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("unknown db scheme"), "{msg}");
        assert!(msg.contains("mysql://"), "{msg}");
    }

    #[test]
    fn scheme_conflict_fails_on_register() {
        struct Fake;
        #[async_trait]
        impl DbBackend for Fake {
            fn name(&self) -> &str { "fake-mysql" }
            fn schemes(&self) -> &'static [&'static str] { &["mysql://"] }
            async fn connect(&self, _: &str, _: &Path) -> BridgeResult<Arc<dyn DataAccessor>> {
                unreachable!()
            }
        }
        let mut r = DbBackendRegistry::builtin();
        let e = r.register(Arc::new(Fake)).unwrap_err();
        assert!(e.to_string().contains("mysql://"));
    }

    #[test]
    fn sqlite_relative_path_resolves_against_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dsn = normalize_sqlite_dsn("sqlite://data/app.db", tmp.path()).unwrap();
        assert!(dsn.contains(tmp.path().to_str().unwrap()), "{dsn}");
    }
}
```

（`tempfile` 若不在 dev-dependencies 则加入；若仓库已有等价临时目录工具（如测试里已有 tempdir 用法）则复用。）

- [x] **Step 3: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust db_backend`
Expected: FAIL（模块不存在）

- [x] **Step 4: 实现** `db_backend.rs`（上方代码 + `normalize_sqlite_dsn` 从 `resolve_dsn` 提炼；`SqlxAccessor::arc` 签名见 accessor_sqlx.rs:29 `pub async fn connect(url: &str)` 的包装——沿用现状）

- [x] **Step 5: 跑测试确认通过**

Run: `cargo test -p mdm-base-rust db_backend -- --skip infinite_loop`
Expected: 4 passed

- [x] **Step 6: Commit**

```bash
git add src/bridge/db_backend.rs src/bridge/mod.rs Cargo.toml
git commit -m "feat(bridge): DbBackend/DbBackendRegistry——认领式注册表 + 内置四方言后端

未知 scheme 报错移入注册表；sqlite 归一化随 SqliteBackend。

unix@vip.qq.com ai"
```

### Task 0.3: resolve_dsn 两处改道 registry.connect

**Files:**
- Modify: `oj/src/server_cmd.rs:82-88`（db 循环）与 `:280`（删 `resolve_dsn`）
- Modify: `oj/src/build_cmd.rs:186-189`（内省内存库）
- Modify: `server/src/lib.rs`（若有 DSN 解析点，grep 确认）
- Test: `oj/src/server_cmd.rs` 内既有 `resolve_dsn_dispatches_by_scheme` 测试迁移

**Interfaces:**
- Consumes: `DbBackendRegistry::builtin()` + `connect(dsn, config_dir)`（Task 0.2）。
- Produces: `pub async fn connect_dbs(cfg_db: &HashMap<String,String>, registry: &DbBackendRegistry, config_dir: &Path) -> Result<HashMap<String, Arc<dyn DataAccessor>>, String>`（放 `oj/src/server_cmd.rs`，server 与 build 共用；build_cmd 传 memory 注册表或直接用 `MemoryBackend`）。

- [x] **Step 1: 确认全部 DSN 解析调用点**

Run: `grep -rn "SqlxAccessor::arc\|resolve_dsn" oj/src/ server/src/ src/ --include='*.rs' | grep -v accessor_sqlx.rs | grep -v db_backend.rs`
Expected: 列出全部调用点；逐一核对落入本任务改动清单。

- [x] **Step 2: 迁移/改写测试**——`resolve_dsn_dispatches_by_scheme` 改为经 `DbBackendRegistry::builtin().connect` 断言：sqlite 相对路径归一化不变、未知 scheme（如 `oracle://`）报 `unknown db scheme`、mysql/pg 不真连（只断言错误文案来自连接层而非 scheme 层）。新增 `connect_dbs` 单测：两库（sqlite 内存 + memory://）成功、未知 scheme 库名出现在错误里。

- [x] **Step 3: 跑测试确认失败**（`connect_dbs` 未定义）

Run: `cargo test -p oj connect_dbs`
Expected: FAIL

- [x] **Step 4: 实现**——server_cmd.rs 的 db 循环改为：

```rust
let registry = mdm_base_rust::bridge::db_backend::DbBackendRegistry::builtin();
let mut dbs = connect_dbs(&cfg.db, &registry, config_dir).await?;
```

`connect_dbs` 实现：

```rust
pub async fn connect_dbs(
    cfg_db: &std::collections::HashMap<String, String>,
    registry: &mdm_base_rust::bridge::db_backend::DbBackendRegistry,
    config_dir: &std::path::Path,
) -> Result<std::collections::HashMap<String, std::sync::Arc<mdm_base_rust::bridge::DataAccessor>>, String> {
    let mut dbs = std::collections::HashMap::new();
    for (name, dsn) in cfg_db {
        let acc = registry.connect(dsn, config_dir).await
            .map_err(|e| format!("open db '{name}': {e}"))?;
        dbs.insert(name.clone(), acc);
    }
    Ok(dbs)
}
```

build_cmd.rs:186-189 的 `SqlxAccessor::arc("sqlite::memory:")` 改为 `DbBackendRegistry::builtin().connect("sqlite::memory:", &root).await`。删除旧 `resolve_dsn`（其 sqlite 归一化已在 Task 0.2 进 `normalize_sqlite_dsn`）。

- [x] **Step 5: 跑测试确认通过 + 全量回归**

Run: `cargo test -p oj -- --skip infinite_loop && cargo test -p mdm-base-rust -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 6: Commit**

```bash
git add oj/src/server_cmd.rs oj/src/build_cmd.rs server/src/
git commit -m "refactor(oj): DSN 解析改道 DbBackendRegistry——未知 scheme 报错归注册表

unix@vip.qq.com ai"
```

### Task 0.4: EsBackend trait 抽取

**Files:**
- Modify: `src/bridge/es.rs`（抽 trait，EsClient 改名为 HTTP 实现或保留名外包一层）
- Modify: `src/bridge/mod.rs`（StableState.es 类型）
- Test: `src/bridge/es.rs` 内既有测试适配

**Interfaces:**
- Consumes: 现状 `EsClient { endpoint, http }`（es.rs:18）+ 三 op（es.rs:69-121）。
- Produces:

```rust
// src/bridge/es.rs 顶部新增
/// es 轴后端契约（spec §2：先抽 trait，HTTP 实现作首个后端）。
#[async_trait::async_trait]
pub trait EsBackend: Send + Sync {
    async fn search(&self, index: &str, body: serde_json::Value) -> BridgeResult<serde_json::Value>;
    async fn index_doc(&self, index: &str, id: &str, body: serde_json::Value) -> BridgeResult<serde_json::Value>;
    async fn delete_doc(&self, index: &str, id: &str) -> BridgeResult<serde_json::Value>;
}

#[async_trait::async_trait]
impl EsBackend for EsClient { /* 三方法体 = 现 op 内 路径拼装+请求+es_resp 直通逻辑 原样上移 */ }
```

- [x] **Step 1: 写失败测试**——新增 mock 后端验证 op 层改走 trait：

```rust
#[tokio::test]
async fn ops_dispatch_via_es_backend_trait() {
    struct Stub;
    #[async_trait::async_trait]
    impl EsBackend for Stub {
        async fn search(&self, index: &str, _: serde_json::Value) -> BridgeResult<serde_json::Value> {
            Ok(serde_json::json!({"stub": index}))
        }
        async fn index_doc(&self, _: &str, _: &str, _: serde_json::Value) -> BridgeResult<serde_json::Value> { unreachable!() }
        async fn delete_doc(&self, _: &str, _: &str) -> BridgeResult<serde_json::Value> { unreachable!() }
    }
    // 用 Stub 构造 Bridge，执行 es.search("idx", {}) JS，断言返回 {"stub":"idx"}
    // 构造方式沿用 es.rs 既有 op 测试的 Bridge 夹具
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust es:: -- --skip infinite_loop`
Expected: FAIL（`EsBackend` 未定义）

- [x] **Step 3: 实现**——三 op 改为：取 `Arc<dyn EsBackend>` → 校验 `valid_ident`（留在 op 层，防注入是 op 职责）→ 调 trait 方法。`StableState.es` 与 `Extras.es` 类型改 `Option<Arc<dyn EsBackend>>`；`oj/src/server_cmd.rs:141` 的 `Arc<EsClient>` 注入处加 `as Arc<dyn EsBackend>`  coercion。op 内错误文案 `es not configured` 保持不变。

- [x] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cargo test -p mdm-base-rust es:: -- --skip infinite_loop && cargo test -p mdm-base-rust -- --skip infinite_loop`
Expected: 全绿（含 `OJ_TEST_ES` 未设时 skip）

- [x] **Step 5: Commit**

```bash
git add src/bridge/es.rs src/bridge/mod.rs oj/src/server_cmd.rs
git commit -m "refactor(bridge): es 轴抽 EsBackend trait——EsClient 为首个 HTTP 实现

unix@vip.qq.com ai"
```

### Task 0.5: StableState/Extras 改注册表载体 + 全部调用点迁移

**Files:**
- Modify: `src/bridge/mod.rs:69-96`（StableState/Extras 字段改形）
- Modify: `src/bridge/blob.rs`（op 取数路径改经 BlobRegistry）
- Modify: `oj/src/server_cmd.rs:120-160`（blob/es/bus 装配段）、`server/src/ws.rs`（共享注入点）、全部测试夹具
- Test: 既有全部测试 + 新增 broker 共享语义回归

**Interfaces:**
- Consumes: Task 0.1-0.4 全部产物。
- Produces（阶段 0 终态类型，后续阶段依赖）:

```rust
// src/bridge/mod.rs
pub struct StableState {
    pub kv: Arc<dyn KVStore>,
    pub dbs: HashMap<String, Arc<dyn DataAccessor>>,
    pub client: reqwest::Client,
    pub registry: Arc<SchemaRegistry>,
    pub loader: Option<Arc<module_loader::LoaderShared>>,
    /// blob 注册表（阶段 0 仅单后端形态：至多一个名为 "default" 的后端；
    /// 阶段 1 扩展命名多后端）。op 取数路径：blob_registry.get("default")。
    pub blobs: Arc<BlobRegistry>,
    /// bus 保持不变（阶段 2 才注册表化）；同一实例跨 actor 池与全部 WS 连接共享。
    pub bus: Arc<dyn bus::EventBroker>,
    pub es: Option<Arc<dyn es::EsBackend>>,
}

#[derive(Default)]
pub struct Extras {
    pub blobs: Option<Arc<BlobRegistry>>, // None = 零后端（blob.* 报 notConfigured）
    pub bus: Option<Arc<dyn bus::EventBroker>>,
    pub es: Option<Arc<dyn es::EsBackend>>,
}
```

```rust
// src/bridge/blob.rs 新增（单后端形态，阶段 1 扩展）
/// blob 轴注册表（键选式）。阶段 0 仅支持名为 "default" 的至多一个后端。
pub struct BlobRegistry { inner: crate::bridge::NamedRegistry<dyn BlobBackend> }
impl BlobRegistry {
    pub fn new() -> Self;
    /// 阶段 0：name 必须 == "default"，否则 Err（阶段 1 放开）。
    /// &mut self：注册全部发生在装配期，装进 Arc 前完成（NamedRegistry 无内部可变性）。
    pub fn register(&mut self, name: &str, b: Arc<dyn BlobBackend>) -> BridgeResult<()>;
    pub fn default(&self) -> Option<Arc<dyn BlobBackend>>;
}
```

（`NamedRegistry` 内部可变性：StableState 创建后不可变，注册全部发生在装配期——`BlobRegistry` 在装配时是可变局部量，装进 Arc 前完成注册；故 `NamedRegistry` 无需内部可变性，`register` 收 `&mut self`，装配完成后再 `Arc::new`。Task 0.1 代码即如此，无需改动。）

- [x] **Step 1: 列出全部受影响的构造/取数点**

Run: `grep -rn "extras.blob\|\.blob\b\|Extras {" src/ oj/src/ server/src/ --include='*.rs' | grep -v "//" | head -30`
Expected: blob.rs 五个 op 的取数点 + server_cmd.rs 装配段 + 测试夹具，全部入改动清单。

- [x] **Step 2: 先改类型再修编译错误**——StableState/Extras 改形后 `cargo check 2>&1 | grep '^error'` 列出的每个点逐一迁移：blob op 从 `state...blob.clone()` 改为 `state...blobs.default()`；`server_cmd.rs` blob 装配段构造 `BlobRegistry` 注册 `default` 后注入；测试夹具同理。ws.rs 的 bus 共享注入不动。

- [x] **Step 3: 新增 broker 共享语义回归测试**（server/src/ws.rs 或既有 ws 测试旁）：

```rust
/// 同一 broker 实例跨两个 Bridge 注入时，A 发布的消息 B 的订阅者能收到
/// （"跨 actor 池与全部 WS 连接共享"语义回归，spec §2 Extras 迁移）。
#[tokio::test]
async fn shared_broker_broadcasts_across_bridges() {
    let bus = mdm_base_rust::bridge::broker::build_broker(&None).await.unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    bus.subscribe("t", tx).await.unwrap();
    // 用同一 bus 构造第二个 Bridge（沿用既有 Bridge 测试夹具），经其 op_bus_publish 发布
    // 断言 rx 收到 payload
}
```

- [x] **Step 4: 全量回归**

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿；JS 层行为不变（blob.* 未配置时报 notConfigured 文案不变）

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "refactor(bridge): StableState/Extras 改注册表载体——blob 入 BlobRegistry、es 入 dyn EsBackend

broker 跨 actor/WS 共享语义回归测试钉死。

unix@vip.qq.com ai"
```

### Task 0.6: 阶段 0 任务状态更新

- [x] **Step 1: 勾选本阶段 Task 0.1-0.5 全部复选框**（计划文件内 `- [x]` → `- [x]`）

- [x] **Step 2: 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 阶段 0 完成——五轴注册表静态链接落地，行为不变

unix@vip.qq.com ai"
```

---

## 阶段 1 — blob 命名多后端

目标：`blob(name)` 工厂 + 命名多后端配置 + 下载路由仅服务 default 裁决（spec §2 blob 条）。

### Task 1.1: BlobRegistry 放开命名多后端 + 配置段

**Files:**
- Modify: `src/config.rs`（`Config.blob: Option<BlobCfg>` → 新增 `BlobBackendsCfg`）
- Modify: `src/bridge/blob.rs`（BlobRegistry 放开任意名字 + 注册名入工厂）
- Modify: `oj/src/server_cmd.rs` blob 装配段
- Test: blob.rs、config.rs、server_cmd.rs

**Interfaces:**
- Produces:

```rust
// src/config.rs（BlobCfg 保留为单后端旧格式，新增复数段）
/// [blob.backends.<name>] 命名多后端；每个条目形态与旧 BlobCfg 相同 + name。
/// 旧 [blob] 单后端段 = 语法糖，等价于 [blob.backends.default]。
pub struct BlobBackendsCfg { pub backends: HashMap<String, BlobCfg> }
```

```rust
// src/bridge/blob.rs
impl BlobRegistry {
    /// 任意名字可注册；重名 fail fast（NamedRegistry 语义）。
    /// &mut self：注册全部发生在装配期（同 Task 0.5）。
    /// 配置声明了名字但装配时无对应后端 → 启动期报错（装配层职责）。
    pub fn register(&mut self, name: &str, b: Arc<dyn BlobBackend>) -> BridgeResult<()>;
    pub fn get(&self, name: &str) -> Option<Arc<dyn BlobBackend>>;
    pub fn names(&self) -> Vec<String>;
}
```

- [x] **Step 1: 写失败测试**

```rust
#[test]
fn blob_backends_cfg_parses_named_sections() {
    let toml = r#"
[blob.backends.default]
driver = "local"
root = "uploads"
[blob.backends.img]
driver = "s3"
bucket = "b"
region = "r"
"#;
    let cfg: crate::config::Config = toml::from_str(toml).unwrap(); // 按现状 config 解析方式（serde_yaml 则改 yaml 样例）
    assert!(cfg.blob_backends.backends.contains_key("img"));
}

#[tokio::test]
async fn registry_multi_backend_and_duplicate_fails() {
    let mut r = BlobRegistry::new();
    r.register("default", Arc::new(LocalBlob::new(/* tmp */).unwrap())).unwrap();
    r.register("img", Arc::new(/* 第二后端 */)).unwrap();
    assert!(r.get("img").is_some());
    assert!(r.register("img", Arc::new(/* 任意 */)).is_err());
}
```

（注意：现状配置解析器是 serde_yaml 还是 toml——先 `grep -n "from_str\|serde_yaml\|toml" src/config.rs` 确认，测试样例用现状格式。）

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust blob -- --skip infinite_loop`
Expected: FAIL

- [x] **Step 3: 实现**——BlobRegistry 去掉"仅 default"限制（改为直接包 NamedRegistry）；config 新增 `blob_backends` 段（旧 `[blob]` 段保留映射为 default，向后兼容）；server_cmd.rs 装配：遍历配置逐个构造（local root 相对 config_dir 绝对化、s3 走 `S3Blob::new` 现状校验），配置声明的名字全部成功注册，缺一 → `blob backend '<name>': ...` 启动期报错。

- [x] **Step 4: 跑测试确认通过 + 回归**

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(blob): 命名多后端——[blob.backends.<name>] 配置 + BlobRegistry 放开注册

旧 [blob] 段兼容映射为 default。

unix@vip.qq.com ai"
```

### Task 1.2: blob(name) JS 工厂 + op 层 name 参数贯穿

**Files:**
- Modify: `src/bridge/blob.rs`（五个 op 加 name 参数）
- Modify: `src/bridge/bootstrap.js:106-114`（blob 装配改工厂形态）
- Test: blob.rs op 测试

**Interfaces:**
- Produces（JS 面，spec §2）:

```js
// bootstrap.js
const __blobBackends = {}; // name → true（op 侧按 name 查注册表，JS 不缓存句柄）
globalThis.blob = (name) => ({
  put: (key, bytes, ct) => op_blob_put(String(name), String(key), bytes, ct === undefined ? null : String(ct)),
  get: (key) => op_blob_get(String(name), String(key)),
  del: (key) => op_blob_del(String(name), String(key)),
  url: (key) => op_blob_url(String(name), String(key)),
  contentType: (key) => op_blob_content_type(String(name), String(key)),
});
// 向后兼容：旧代码 globalThis.blob.put(...) 直调 = blob("default")
const __blobDefault = globalThis.blob("default");
globalThis.blob = Object.assign(globalThis.blob, __blobDefault);
```

op 签名变化（五 op 同构）：

```rust
#[op2(async)]
pub async fn op_blob_get(state: Rc<RefCell<OpState>>, #[string] name: String, #[string] key: String) -> Result<Vec<u8>, JsErrorBox>
```

op 取数：`blobs.get(&name)` → None 时：name == "default" 报 `blob not configured`（旧文案），否则报 `blob backend '<name>' not configured`（首次调用期报错，spec §2）。

- [x] **Step 1: 写失败测试**——op 层：注册 `default`+`img` 两后端，JS 调 `blob("img").put/get` 命中 img 后端、`blob.put`（无 name）命中 default、`blob("ghost").get` 报 `blob backend 'ghost' not configured`。

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust blob -- --skip infinite_loop`
Expected: FAIL（op 参数不匹配）

- [x] **Step 3: 实现**——五 op 加 name 参数 + bootstrap.js 工厂装配（上方代码）。

- [x] **Step 4: 跑测试确认通过 + 回归**（重点：既有 blob 测试不经 name 的旧调用经 default 兼容层仍绿）

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(blob): blob(name) 工厂——op 层 name 参数贯穿，default 兼容旧代码

unix@vip.qq.com ai"
```

### Task 1.3: 下载路由仅服务 default + 非 default local url() 报错

**Files:**
- Modify: `server/src/routes.rs`（blob 下载路由，grep `blob` 定位）
- Modify: `src/bridge/blob.rs`（LocalBlob::url 携带注册名裁决）
- Test: routes.rs / blob.rs

**Interfaces:**
- Consumes: Task 1.1 的注册表 + 注册名。
- Produces: 裁决规则（spec §2）——HTTP 下载路由仅服务名为 `default` 的后端；`LocalBlob` 在注册名 ≠ "default" 时 `url()` 报 `blob url() is only available for the 'default' backend (use get() or an s3 presign)`；s3 presign 不受影响。实现形态：`BlobRegistry::register` 把注册名透传给后端（`LocalBlob` 增 `name: String` 字段，`S3Blob` 忽略）。

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn local_blob_url_errors_when_not_default() {
    let b = LocalBlob::named("img", /* tmp root */, /* base */).unwrap();
    let e = b.url("k").await.unwrap_err();
    assert!(e.to_string().contains("only available for the 'default' backend"));
}

#[tokio::test]
async fn download_route_serves_default_only() {
    // 注册 default(local A) + img(local B)，各 put 不同内容；
    // 请求下载路由 → 字节与 default 后端内容一致（字节一致回归）
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust blob -- --skip infinite_loop`
Expected: FAIL

- [x] **Step 3: 实现**——LocalBlob 增 name 字段（`LocalBlob::new` 保留 = name "default" 委托 `named`）；routes.rs 下载路由从 `StableState.blobs.get("default")` 取后端（现状若从 Extras.blob 取则改道）。

- [x] **Step 4: 跑测试确认通过 + 回归**

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(blob): 下载路由仅服务 default——非 default local url() 明确报错

unix@vip.qq.com ai"
```

### Task 1.4: 阶段 1 任务状态更新

- [x] **Step 1: 勾选 Task 1.1-1.3 复选框**
- [x] **Step 2: 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 阶段 1 完成——blob 命名多后端落地

unix@vip.qq.com ai"
```

---

## 阶段 2 — bus 注册表轴

### Task 2.1: BusBackend trait + BusBackendRegistry + build_broker 改道

**Files:**
- Modify: `src/bridge/broker/mod.rs`（build_broker 改注册表查表）
- Create: `src/bridge/bus_backend.rs`（trait + 注册表 + local/kafka/rabbitmq 三后端注册化）
- Modify: `src/bridge/mod.rs`（mod 声明）
- Test: bus_backend.rs

**Interfaces:**
- Consumes: `EventBroker`（bus.rs:71）、`BrokerCfg { kind, brokers }`（config.rs:85）、`NamedRegistry`（Task 0.1）。
- Produces:

```rust
// src/bridge/bus_backend.rs
/// bus 轴后端工厂（键选式，按 broker.kind 单选，spec §2）。
#[async_trait::async_trait]
pub trait BusBackend: Send + Sync {
    fn kind(&self) -> &str; // "local" / "kafka" / "rabbitmq"
    async fn connect(&self, cfg: &crate::config::BrokerCfg) -> BridgeResult<Arc<dyn EventBroker>>;
}

/// kind → 工厂查表；重名 kind 注册 fail fast（NamedRegistry 语义）。
pub struct BusBackendRegistry { inner: NamedRegistry<dyn BusBackend> }
impl BusBackendRegistry {
    /// 内置：local（零依赖）；kafka/rabbitmq 按 feature 注册（阶段 4 迁插件）。
    pub fn builtin() -> Self;
    pub fn register(&mut self, b: Arc<dyn BusBackend>) -> BridgeResult<()>;
    /// kind 未注册 → `unknown broker kind '<kind>' (known: [...])`。
    pub async fn connect(&self, cfg: &Option<BrokerCfg>) -> BridgeResult<Arc<dyn EventBroker>>;
    pub fn kinds(&self) -> Vec<String>;
}
```

`build_broker(&cfg)` 保留为薄包装 = `BusBackendRegistry::builtin().connect(&cfg).await`（签名不变，调用点零改动）。`op_bus_kind` 返回值语义不变（当前选中后端的 kind 字符串）。

- [x] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn registry_connects_local_by_default_and_kind() {
    let r = BusBackendRegistry::builtin();
    r.connect(&None).await.unwrap(); // None → local
    let cfg = BrokerCfg { kind: "local".into(), brokers: vec![] };
    r.connect(&Some(cfg)).await.unwrap();
    assert!(r.kinds().contains(&"local".to_string()));
}

#[tokio::test]
async fn unknown_kind_errors_with_known_list() {
    let r = BusBackendRegistry::builtin();
    let cfg = BrokerCfg { kind: "nats".into(), brokers: vec![] };
    let e = r.connect(&Some(cfg)).await.unwrap_err();
    assert!(e.to_string().contains("unknown broker kind 'nats'"));
}

#[test]
fn duplicate_kind_fails() {
    let mut r = BusBackendRegistry::builtin();
    assert!(r.register(Arc::new(LocalBusBackend)).is_err());
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust bus_backend -- --skip infinite_loop`
Expected: FAIL

- [x] **Step 3: 实现**——`LocalBusBackend`/`KafkaBusBackend`/`RabbitMqBusBackend` 各实现 `BusBackend::connect`（Kafka/RabbitMQ 的 connect 体 = 现 broker/kafka.rs、broker/rabbitmq.rs 构造逻辑上移，feature gate 保留）；`build_broker` 改薄包装。

- [x] **Step 4: 跑测试确认通过 + 回归**（broker/mod.rs 既有三测试不动应仍绿；`op_bus_kind` 语义回归）

Run: `cargo test --workspace --features rabbitmq -- --skip infinite_loop`（kafka 视本机 librdkafka 而定，不可用则注明并跑 default+rabbitmq）
Expected: 全绿

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(bus): BusBackend/BusBackendRegistry——kind 查表单选，三实现注册化

build_broker 保留为薄包装，调用点零改动。

unix@vip.qq.com ai"
```

### Task 2.2: 阶段 2 任务状态更新

- [x] **Step 1: 勾选 Task 2.1 复选框**
- [x] **Step 2: 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 阶段 2 完成——bus 注册表轴落地

unix@vip.qq.com ai"
```

---

## Spike — 阶段 3 前置门槛

目标：用最小可运行样例钉死三个技术选型，产出决策记录。**Spike 产物是临时 crate，不进 workspace 主依赖；决策记录在案后决定阶段 3 的后端对象形态。** 失败回退预案 = 保留编译期链接 feature（spec §8）。

### Task S.1: stabby vs abi_stable 选型样例

**Files:**
- Create: `spikes/ffi-select/`（独立 mini workspace：`host` bin + `plugin` cdylib 两 crate，各试 stabby 与 abi_stable 两种绑定）
- Create: `spikes/ffi-select/NOTES.md`（决策记录）

**Interfaces:**
- Produces: 选型结论（写入 NOTES.md 与本计划 Task 3.1 的 `use` 决策）；样例证明：RString/RBytes/RResult 跨界 roundtrip、布局不匹配时 canary 报错而非 UB。

- [x] **Step 1: 搭样例**——plugin cdylib 导出 `extern "C" fn echo(input: RString) -> RResult<RString, RString>` 与 `abi_version() -> u32`；host 用 libloading 加载调用，断言 roundtrip 一致。

- [x] **Step 2: 验证布局防护**——host 与 plugin 用**不同版本**的契约 struct 编译（人为改一个字段），断言加载/调用时被 canary/layout 校验拒绝而非静默错乱。两个绑定库各跑一遍。

- [x] **Step 3: 决策记录**——NOTES.md 写：选型（默认 stabby，除非样例暴露硬伤）、layout 校验实测行为、rust 版本要求、依赖体积。结论同步进 spec §3 若与默认假设冲突。

- [x] **Step 4: Commit**

```bash
git add spikes/
git commit -m "spike(ffi): stabby vs abi_stable 选型样例 + 决策记录

unix@vip.qq.com ai"
```

### Task S.2: FfiFuture + 插件自建 tokio runtime 最小样例

**Files:**
- Create: `spikes/ffi-async/`（复用 S.1 选定的绑定库）
- Create: `spikes/ffi-async/NOTES.md`

**Interfaces:**
- Produces: FfiFuture 最小定义（阶段 3 契约 crate 的原型）:

```rust
/// repr(C) future 句柄：插件侧 runtime 驱动的 oneshot 共享状态。
/// poll_ready：宿主非阻塞查询；block_on/await 桥接由宿主侧适配器实现。
/// drop = 宿主放弃结果，插件任务允许跑完，不保证取消（spec §3）。
#[repr(C)]
pub struct FfiFuture {
    pub state: *mut std::ffi::c_void,      // 插件侧共享状态（opaque）
    pub poll: extern "C" fn(*mut std::ffi::c_void) -> i32, // 0 pending / 1 ready / -1 error
    pub take: extern "C" fn(*mut std::ffi::c_void) -> RResult<RBytes, RString>,
    pub free: extern "C" fn(*mut std::ffi::c_void),
}
```

- [x] **Step 1: 写样例**——plugin init 时 `tokio::runtime::Builder::new_multi_thread().enable_all().build()` 自建 runtime；导出 `sleep_ms(ms: u64) -> FfiFuture`（内部 `rt.spawn(async { tokio::time::sleep(...).await; ... })`）；host 在**自己的** tokio runtime 里 await 该 FfiFuture（`tokio::task::yield_now` 轮询 poll 或 spawn_blocking block_on），断言结果正确。

- [x] **Step 2: 关键断言**——插件内执行真实 `tokio::time::sleep` +（若样例含 sqlx/reqwest 则）真实异步调用**不 panic**（"there is no reactor running" 不复现 = 插件 TLS 在插件自己那份 tokio 上成立）。

- [x] **Step 3: drop 语义样例**——host 提前 drop FfiFuture，断言插件任务跑完（用插件侧 AtomicUsize 计数验证）、无 UB、无泄漏（`free` 被调）。

- [x] **Step 4: 决策记录 + Commit**——NOTES.md 写 FfiFuture 最终形态（含 take 后的状态清理时序）。

```bash
git add spikes/
git commit -m "spike(ffi): FfiFuture + 插件自建 tokio runtime 样例——sleep 不 panic、drop 语义实测

unix@vip.qq.com ai"
```

### Task S.3: tx 句柄化 DataAccessor FFI 样例 + 后端对象形态决策

**Files:**
- Create: `spikes/ffi-tx/`
- Create: `spikes/ffi-tx/NOTES.md`

**Interfaces:**
- Produces: vtable 形态定案（保守 opaque handle + vtable vs 升级 stabby dynptr）；tx 句柄化模式原型：

```rust
// begin -> tx_id(u64)；插件内部 HashMap<u64, Box<dyn TxSession>> 查表。
// tx_query(handle, tx_id, sql, params_json) -> FfiFuture
// tx_commit(handle, tx_id) -> FfiFuture ；句柄表随 commit/rollback 移除条目
```

- [x] **Step 1: 写样例**——plugin 内嵌一个极简内存 DataAccessor（不依赖 sqlx），导出 vtable：`connect(cfg) -> u64 handle` / `query(handle, sql, params) -> FfiFuture` / `begin(handle) -> FfiFuture(tx_id)` / `tx_exec(handle, tx_id, ...) -> FfiFuture` / `tx_commit/tx_rollback(handle, tx_id) -> FfiFuture` / `close(handle)`。

- [x] **Step 2: host 侧走通**——connect → begin → tx_exec → tx_commit 全链路；再测 tx_id 未 commit 直接 close handle（= drop-rollback 语义的 FFI 映射：host 侧适配器 drop 时调 tx_rollback，见 Task 3.3）。

- [x] **Step 3: dynptr 评估**——尝试把同一接口用 stabby dynptr 直出 `dyn Trait` 对象替代 vtable；记录编译复杂度、文档成熟度、panic 收敛兼容性。门槛：样例必须证明 dynptr 形态下 panic 收敛宏与 FfiFuture 仍成立，否则**保持保守 vtable 形态**（spec §3 默认）。

- [x] **Step 4: 决策记录 + Commit**——NOTES.md 定案后端对象形态；若升级 dynptr，回写 spec §3 与 Task 3.1/3.3 的 vtable 描述（标注"经 spike S.3 升级为 dynptr"）。

```bash
git add spikes/
git commit -m "spike(ffi): tx 句柄化样例 + 后端对象形态决策（vtable 保守 / dynptr 升级）

unix@vip.qq.com ai"
```

### Task S.4: 阶段 Spike 任务状态更新

- [x] **Step 1: 勾选 Task S.1-S.3 复选框**；若决策改变了阶段 3+ 的接口形态，**先回写本计划对应任务再勾选**。
- [x] **Step 2: 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md docs/superpowers/specs/
git commit -m "docs(plan): spike 完成——FFI 选型定案，阶段 3 门槛通过

unix@vip.qq.com ai"
```

---

## 阶段 3 — 契约 crate + FFI 边界 + oj-es 试点

目标：`oj-plugin-ffi` + PluginLoader + core 适配器层 + xtask + 首个 cdylib 插件（oj-es）走通全链路。（若 S.3 升级为 dynptr，本阶段 vtable 相关代码相应替换，任务边界不变。）

### Task 3.1: oj-plugin-ffi 契约 crate

**Files:**
- Create: `oj-plugin-ffi/Cargo.toml`、`oj-plugin-ffi/src/lib.rs`（+ 按需要拆 `future.rs`/`descriptor.rs`/`host.rs`）
- Modify: `Cargo.toml`（workspace members += "oj-plugin-ffi"）
- Test: `oj-plugin-ffi/src/lib.rs` 内

**Interfaces:**
- Produces（全部插件与 core ffi.rs 依赖）:

```rust
// oj-plugin-ffi/src/lib.rs
pub use stabby::{string::RString, vec::RVec, bytes::RBytes, result::RResult, sync::RArc};

/// 唯一硬门禁：严格相等才允许加载（spec §3）。
pub const ABI_VERSION: u32 = 1;

/// 插件描述（repr(C)：任何字段变更 = ABI_VERSION bump，spec §3 契约演进总则）。
#[repr(C)]
pub struct PluginDescriptor {
    pub name: RString,
    pub semver: RString,
    pub abi_version: u32,
    /// 构建指纹：rustc 版本 + oj-plugin-ffi 版本 + target triple（诊断用，不匹配仅告警）。
    pub fingerprint: RString,
}

/// 宿主回调集（RArc 共享所有权传入，进程级有效；不提供 registry lookup——插件互不可见）。
#[repr(C)]
pub struct HostContext {
    /// 日志上送：插件日志经此回调进宿主 tracing。
    pub log: extern "C" fn(level: u8, msg: RString),
    // bus deliver 回调在 Task 4.3 加入（加回调 = ABI bump，本期一次设计好预留位）。
}

/// 插件入口两符号（由宏生成，禁止手写 #[no_mangle] 绕过，spec §3）：
///   oj_plugin_abi_version() -> u32
///   oj_plugin_init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString>
/// 宏内建 catch_unwind(AssertUnwindSafe(..))，panic 映射为 RResult 错误。
#[macro_export]
macro_rules! oj_plugin_entry {
    ($init:expr) => { /* 生成两导出符号，体 = catch_unwind 包装 $init */ };
}

pub use crate::future::FfiFuture; // Task S.2 定稿形态
```

`Cargo.toml` 关键：

```toml
[package]
name = "oj-plugin-ffi"
version = "0.1.0"
edition = "2024"

[dependencies]
stabby = "…" # 版本按 Task S.1 选型记录
```

- [x] **Step 1: 写失败测试**——`oj_plugin_entry!` 宏展开的两符号存在且 abi_version 返回 `ABI_VERSION`；模拟 init 内 panic 的插件函数经宏包装后返回 `RResult::Err` 而非 unwind 跨界（单 crate 内直调测试即可）。

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p oj-plugin-ffi`
Expected: FAIL（crate 不存在）

- [x] **Step 3: 实现**（宏用 `std::panic::catch_unwind`；`#[no_mangle] extern "C"` 两符号；FfiFuture 从 spike S.2 样例迁入）

- [x] **Step 4: 跑测试确认通过**

Run: `cargo test -p oj-plugin-ffi`
Expected: 全绿

- [x] **Step 5: Commit**

```bash
git add oj-plugin-ffi/ Cargo.toml Cargo.lock
git commit -m "feat(ffi): oj-plugin-ffi 契约 crate——ABI_VERSION/PluginDescriptor/HostContext/入口宏/FfiFuture

unix@vip.qq.com ai"
```

### Task 3.2: PluginLoader + 四级路径解析 + 错误分类

**Files:**
- Create: `src/bridge/ffi.rs`（全部 unsafe 收敛于此；「加载 + forget」单一函数）
- Create: `src/bridge/plugin_loader.rs`
- Modify: `src/bridge/mod.rs`、`Cargo.toml`（libloading、oj-plugin-ffi 依赖）
- Test: plugin_loader.rs 内 + `tests/fixtures/`（预编译的测试插件产物由 build.rs 或 xtask 准备——见 Step 1）

**Interfaces:**
- Consumes: oj-plugin-ffi（Task 3.1）。
- Produces:

```rust
// src/bridge/plugin_loader.rs
/// 加载路径四级解析（spec §4）：OJ_PLUGINS_DIR > oj.toml plugins_dir >
/// <exe>/plugins > build.rs dev 后备（workspace root 常量）。
/// relative 一律相对 oj.toml 所在目录。返回最终目录 = <plugins_dir>/<host-triple>/。
/// 显式配置 1/2 而目录不存在 → Err；默认 3/4 不存在 → Ok(None)（零插件）。
pub fn resolve_plugins_dir(config_dir: &Path, toml_plugins_dir: Option<&Path>)
    -> Result<Option<PathBuf>, String>;

/// 七类加载失败（spec §4），各自独立错误文案：
pub enum PluginLoadError {
    FileMissing { path: PathBuf },
    PlatformMismatch { path: PathBuf, detail: String },      // 含 glibc 基线不满足
    DependencyResolution { path: PathBuf, loader_text: String }, // 透出 loader 原始错误
    AbiMismatch { plugin: u32, host: u32 },
    SymbolMissing { path: PathBuf, symbol: &'static str },
    IdentityMismatch { expected: String, actual: String },
    InitFailed { name: String, detail: String },             // init 返回错误或 panic
}

/// 按清单装配：文件缺失/任何校验失败 → fail fast（Err）。
/// manifest: 插件名 + 可选 "@semver" pin。
pub fn load_manifest(
    dir: &Path, manifest: &[PluginManifestEntry],
    host: RArc<HostContext>, cfg_for: &dyn Fn(&str) -> String,
) -> Result<Vec<LoadedPlugin>, PluginLoadError>;

/// 扫描装配（缺省模式）：加载 dir 下全部符合命名约定的库文件；
/// 目录不存在/为空 → Ok(vec![])；扫描到但校验失败 → Err（不静默跳过，spec §5）。
pub fn load_scanned(dir: &Path, host: RArc<HostContext>)
    -> Result<Vec<LoadedPlugin>, PluginLoadError>;

pub struct LoadedPlugin {
    pub descriptor: PluginDescriptor,
    /// init 后宿主经 descriptor 内注册回调指针取得（各轴工厂槽位，未实现轴为 None；
    /// db/blob/bus/kv 槽位随阶段 4 各任务逐一加入本结构）。
    pub registrations: Registrations,
}

#[derive(Default)]
pub struct Registrations {
    pub es: Option<&'static oj_plugin_ffi::EsBackendVtable>, // Task 3.3 起填
}

/// 清单条目：插件名 + 可选 "@semver" pin（spec §决策表）。
pub struct PluginManifestEntry {
    pub name: String,
    pub semver_pin: Option<String>,
}
```

```rust
// src/bridge/ffi.rs
/// 唯一 dlopen 点：加载成功立即 mem::forget（进程期存活，任何路径不 dlclose，spec §决策表）。
/// unsafe 审计清单：Library 句柄不 drop / 插件 panic=unwind profile / 符号签名与契约一致。
pub(crate) unsafe fn load_forget(path: &Path) -> Result<&'static libloading::Library, PluginLoadError>;
```

- [x] **Step 1: 准备测试插件**——`tests/plugins/` 加一个 mini cdylib crate（`oj-plugin-ffi` 的 `oj_plugin_entry!`，init 返回固定 descriptor），xtask 或 build.rs 在测试前编译并拷到 `target/test-plugins/<triple>/`。（此夹具后续全部加载测试复用。）

- [x] **Step 2: 写失败测试**——路径解析四级优先级各一例（env 覆盖 toml、toml 覆盖 exe 旁、显式不存在报错、默认不存在为零插件）；清单模式文件缺失 → `FileMissing`；ABI 不符 → `AbiMismatch`（测试插件编译时用环境变量覆盖其报告版本）；身份不符 → `IdentityMismatch`；扫描模式空目录 → 零插件、坏插件 → Err。

- [x] **Step 3: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust plugin_loader -- --skip infinite_loop`
Expected: FAIL

- [x] **Step 4: 实现**——`resolve_plugins_dir`（build.rs 捕获 workspace root：`println!("cargo:rustc-env=OJ_WORKSPACE_ROOT={}", ...)`）；`load_forget` + 两模式加载（含**指纹比对：不符仅 eprintln 告警不 fail**，spec §3）；**宿主 panic hook 安装**（装配首个插件前安装一次，输出当前插件上下文与构建指纹用于归因，spec §3）；Windows 分支用 `load_with_flags(LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32)`（cfg(windows)，本期编译过关即可，实测归 CI）。

- [x] **Step 5: 跑测试确认通过**

Run: `cargo test -p mdm-base-rust plugin_loader ffi -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(bridge): PluginLoader——四级路径解析 + 清单/扫描双模式 + 七类失败分类 + 加载即 forget

unix@vip.qq.com ai"
```

### Task 3.3: core 侧适配器层 FfiEsBackend

**Files:**
- Modify: `src/bridge/ffi.rs`
- Test: ffi.rs 内（vtable mock，不起真插件）

**Interfaces:**
- Consumes: `EsBackend` trait（Task 0.4）、oj-plugin-ffi vtable 定义（Task 3.1 内 es 轴 vtable）。
- Produces（spec §3 适配器层——每轴一个，es 先行）:

```rust
// oj-plugin-ffi 内 es 轴 vtable
#[repr(C)]
pub struct EsBackendVtable {
    pub search: extern "C" fn(handle: u64, index: RString, body: RString) -> FfiFuture,
    pub index_doc: extern "C" fn(handle: u64, index: RString, id: RString, body: RString) -> FfiFuture,
    pub delete_doc: extern "C" fn(handle: u64, index: RString, id: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}

// src/bridge/ffi.rs
/// 实现 core EsBackend，内部持 opaque handle、经 vtable + FfiFuture 转发。
/// 插件侧永远不直接产出 dyn Trait 跨边界（spec §3）。
pub struct FfiEsBackend { handle: u64, vtable: &'static EsBackendVtable }
#[async_trait::async_trait]
impl EsBackend for FfiEsBackend {
    async fn search(&self, index: &str, body: Value) -> BridgeResult<Value> {
        let fut = (self.vtable.search)(self.handle, index.into(), serde_json::to_string(&body)?.into());
        let bytes = await_ffi(fut).await?; // FfiFuture → host async 桥（S.2 定稿形态）
        Ok(serde_json::from_slice(&bytes)?)
    }
    // index_doc/delete_doc 同构
}
impl Drop for FfiEsBackend { fn drop(&mut self) { (self.vtable.close)(self.handle); } }
```

- [x] **Step 1: 写失败测试（vtable mock）**——构造一个 Rust 函数指针填充的假 vtable（search 返回固定 JSON、close 置 AtomicBool），断言：FfiEsBackend.search 转发参数正确并反序列化返回值；Drop 时 close 被调；FfiFuture 返回 error 时映射为 BridgeResult::Err。

- [x] **Step 2: 跑测试确认失败**

Run: `cargo test -p mdm-base-rust ffi -- --skip infinite_loop`
Expected: FAIL

- [x] **Step 3: 实现** + `await_ffi` 桥（轮询/oneshot 形态按 S.2 记录）

- [x] **Step 4: 跑测试确认通过**

Run: `cargo test -p mdm-base-rust ffi -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(ffi): FfiEsBackend 适配器——vtable 转发 + FfiFuture 桥 + Drop close

unix@vip.qq.com ai"
```

### Task 3.4: oj-es cdylib 插件

**Files:**
- Create: `oj-es/Cargo.toml`（`crate-type = ["cdylib"]`、`panic = "unwind"` 显式声明）、`oj-es/src/lib.rs`
- Modify: `Cargo.toml`（workspace members）
- Test: oj-es 内（单测）+ 阶段 6 全链路验收

**Interfaces:**
- Consumes: oj-plugin-ffi（Task 3.1）；es HTTP 逻辑（从 core `es.rs` 的 `EsClient` 迁入）。
- Produces: cdylib 产物 `liboj_es.so/.dylib` / `oj_es.dll`；插件侧结构：

```rust
// oj-es/src/lib.rs
struct EsPluginState { rt: tokio::runtime::Runtime, clients: Mutex<HashMap<u64, EsClientInner>>, next: AtomicU64 }
// init：建 runtime（Task S.2 形态），解析 cfg JSON（endpoint），注册 es vtable 工厂
oj_plugin_ffi::oj_plugin_entry!(init);
fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> { /* ... */ }
```

- [x] **Step 1: 迁移 EsClient HTTP 实现**入 oj-es（url_for/valid_ident 中 url_for 随实现走、valid_ident 留在 core op 层）；core es.rs 删 HTTP 细节，保留 trait + ops。
  > 注：core es.rs 的 HTTP 细节**删除随 Task 3.7 装配接线一并做**——`oj/src/server_cmd.rs` 当前仍构造 `EsClient`（3.7 才切插件路径），此步先删会破坏编译；oj-es 已持有完整迁入实现。

- [x] **Step 2: 写插件内单测**——vtable 三方法经插件自建 runtime 执行（httptest mock http；PLUGIN 单例经 init 建立，`EsClientInner` 直测与 vtable 直测分离避免单例竞争）。

- [x] **Step 3: 编译验证产物**

Run: `cargo build -p oj-es && ls target/debug/liboj_es.*`
Expected: 产出 `.dylib`（本机 macOS）/`.so`
实际：`target/debug/liboj_es.dylib`（4 单测绿，`[profile.release] panic="unwind"` 因非 root 被忽略 → 提升至 workspace root）

- [x] **Step 4: Commit**

```bash
git add oj-es/ Cargo.toml Cargo.lock src/bridge/es.rs
git commit -m "feat(oj-es): 首个 cdylib 插件——es HTTP 实现迁入，插件自建 tokio runtime

unix@vip.qq.com ai"
```

### Task 3.5: cargo xtask plugin

**Files:**
- Create: `xtask/Cargo.toml`、`xtask/src/main.rs`
- Modify: `Cargo.toml`（workspace members += "xtask"；`.cargo/config.toml` 加 `alias xtask`）

**Interfaces:**
- Produces:

```
cargo xtask plugin <name>        # 编译 oj-<name>（release）+ 拷入 仓库根 ./plugins/<host-triple>/
cargo xtask plugin <name> --check # loader dry-run：ABI/semver/符号预检，不注册不驻留
```

- [x] **Step 1: 实现**——`std::process::Command` 调 `cargo build -p oj-<name> --release`；产物名按平台映射（`liboj_<name>.so|dylib` / `oj_<name>.dll`，`-`→`_`）；host triple 用 `rustc -vV` 解析；拷贝目标 = 仓库根 `./plugins/<triple>/`（与 dev 后备查找对应，spec §4）。`--check` 复用 Task 3.2 的 PluginLoader（加载→校验→**不 forget 直接 drop 句柄仅用于 dry-run**？——不：dry-run 在子进程里跑，forget 无妨，复用同一入口保证预检与真实加载一致）。
  > 命名约定落地：编译产物（crate `oj-<name>` → `liboj_<name>.*`）拷贝时**改名**为 loader 的 `plugin_file_name(name)`（`lib<name>.*`，descriptor.name 对齐，同 plugin_loader 测试对 mini 的做法）；xtask 内复制该命名（loader 的 `plugin_file_name` 是 pub(crate)）。

- [x] **Step 2: 自验**

Run: `cargo xtask plugin es && ls plugins/$(rustc -vV | sed -n 's/host: //p')/`
Expected: `libes.dylib` 就位；`cargo xtask plugin es --check` 退出码 0
实际：`libes.dylib` 就位，`--check` 输出 `ok: es 0.1.0 (abi 1)`，退出码 0

- [x] **Step 3: Commit**

```bash
git add xtask/ .cargo/ Cargo.toml
git commit -m "feat(xtask): cargo xtask plugin——单独编译 + 拷入 ./plugins/<triple>/ + --check 预检

unix@vip.qq.com ai"
```

### Task 3.6: op_plugins 自省 op

**Files:**
- Modify: `src/bridge/mod.rs`（extension! ops += op_plugins）、`src/bridge/bootstrap.js`
- Create: `src/bridge/plugins_op.rs`（或并入 mod.rs，按体量）
- Test: 同文件

**Interfaces:**
- Produces:

```rust
/// 自省：已加载插件名/semver/ABI/指纹 + 宿主当前 ABI_VERSION（spec §4 升级核对、§2 注册表自省并入）。
#[op2]
#[serde]
pub fn op_plugins(state: &mut OpState) -> Vec<PluginInfo>;
// JS：globalThis.plugins() → [{name, semver, abi_version, fingerprint, host_abi_version}]
```

- [x] **Step 1: 写失败测试**——装配一个测试插件后 JS 调 `plugins()` 断言字段齐全；零插件时返回空数组且含 host ABI。

- [x] **Step 2: 跑测试确认失败** → **Step 3: 实现**（装配结果存 StableState 新字段 `plugins: Vec<PluginInfo>`）→ **Step 4: 测试通过** → **Step 5: Commit**
  > `PluginInfo` 落 `plugin_loader.rs`（`From<&LoadedPlugin>`，host_abi_version = ABI_VERSION）；`Extras.plugins` 作装配注入载体（同 es）；`plugins_op.rs` 独立文件。

```bash
git commit -m "feat(bridge): op_plugins 自省——插件清单 + 宿主 ABI_VERSION 输出

unix@vip.qq.com ai"
```

### Task 3.7: oj.toml plugins 清单 + 装配流程接线

**Files:**
- Modify: `src/config.rs`（`plugins: Option<Vec<String>>`、`plugins_dir: Option<PathBuf>`）
- Modify: `oj/src/server_cmd.rs`（装配流程接线，spec §5 全流程）
- Test: server_cmd.rs / 装配集成测试

**Interfaces:**
- Consumes: Task 3.2 两模式加载 + Task 3.6 自省字段。
- Produces: 装配入口

```rust
/// spec §5 全流程：解析 plugins_dir → 清单严格 / 缺省扫描 → 去重 → 逐个加载校验
/// → 身份核对 → semver 对照 → 注册（插件先于内置）→ 内置后端注册。
pub async fn assemble_plugins(cfg: &Config, config_dir: &Path, registries: &mut Registries)
    -> Result<Vec<PluginInfo>, String>;
```

- [x] **Step 1: 写失败测试**——清单显式给出且文件缺失 → 启动报错；清单同名两次 → fail fast；缺省扫描零目录 → 仅内置后端正常启动；扫描到坏插件 → fail fast；配置声明 `[es]` 但插件未装 → 启动期报错（"配置声明了能力但插件未装"闸门）。

- [x] **Step 2: 跑测试确认失败** → **Step 3: 实现接线** → **Step 4: 全量回归**

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿（实测全绿；oj 51 项中 60s 为既有 `registry_connect_dispatches_by_scheme` sqlx 连接超时，非本任务引入）

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(oj): plugins 清单装配接线——清单严格/缺省扫描双模式 + 注册冲突 fail fast

unix@vip.qq.com ai"
```

### Task 3.8: 阶段 3 任务状态更新

- [x] **Step 1: 勾选 Task 3.1-3.7 复选框**
- [x] **Step 2: 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 阶段 3 完成——契约 crate + FFI 边界 + oj-es 试点走通

unix@vip.qq.com ai"
```

---

## 阶段 4 — 全量 cdylib 化

目标：db/blob/bus/kv 四轴全部插件化，core 收敛 sqlite-only。**每轴任务结构同构：oj-plugin-ffi 加 vtable → ffi.rs 加适配器 → 插件 crate 迁移 → 接线。**（重复代码块按轴给出，不互相引用——执行者可能乱序阅读。）

### Task 4.1: db 轴 FFI + oj-db-mysql / oj-db-postgres

**Files:**
- Modify: `oj-plugin-ffi/src/lib.rs`（db vtable + tx 句柄族，ABI_VERSION bump 到 2，全部已出插件同步重编）
- Modify: `src/bridge/ffi.rs`（FfiDataAccessor 适配器）
- Create: `oj-db-mysql/`、`oj-db-postgres/`（cdylib）
- Test: ffi.rs mock + `OJ_TEST_MYSQL`/`OJ_TEST_PG` env-gated 集成测试

**Interfaces:**
- Produces:

```rust
// oj-plugin-ffi
#[repr(C)]
pub struct DataAccessorVtable {
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,          // ok 值 = handle(u64) JSON
    pub query: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    pub exec: extern "C" fn(handle: u64, sql: RString, params: RString) -> FfiFuture,
    pub begin: extern "C" fn(handle: u64) -> FfiFuture,             // ok 值 = tx_id(u64)
    pub tx_query: extern "C" fn(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture,
    pub tx_exec: extern "C" fn(handle: u64, tx_id: u64, sql: RString, params: RString) -> FfiFuture,
    pub tx_commit: extern "C" fn(handle: u64, tx_id: u64) -> FfiFuture,
    pub tx_rollback: extern "C" fn(handle: u64, tx_id: u64) -> FfiFuture,
    pub dialect: extern "C" fn(handle: u64) -> RString,
    pub close: extern "C" fn(handle: u64),
}

// src/bridge/ffi.rs：FfiDataAccessor 实现 DataAccessor + FfiTxSession 实现 TxSession
// ActiveTx 语义映射：core 侧 FfiTxSession Drop 时调 tx_rollback（= 现状"reset 丢弃 ActiveTx
// = drop 自带回滚"的 FFI 保留，spec §3 FfiFuture drop 条）。
```

- [x] **Step 1: 写失败测试（vtable mock）**——FfiDataAccessor 全方法转发；FfiTxSession drop → tx_rollback 被调（AtomicU64 记录）；commit 后 drop 不再 rollback。

- [x] **Step 2: 跑测试确认失败** → **Step 3: 实现适配器 + 两插件 crate**（插件内嵌各自 sqlx DataAccessor 实现，插件边界内可用 `sqlx::Any` 单方言 feature；共享逻辑抽插件侧公共 crate 或接受复制——决策在此步做并记录 commit message）→ **Step 4: 测试通过 + env-gated 真连测试**（`OJ_TEST_MYSQL=mysql://… cargo test -p oj-db-mysql`）→ **Step 5: 接线**（DbBackendRegistry 注册 FFI 版 mysql/postgres 工厂，内置 SqliteBackend/MemoryBackend 保留）→ **Step 6: Commit**

```bash
git commit -m "feat(db): db 轴 FFI——tx 句柄化 + oj-db-mysql/oj-db-postgres cdylib

unix@vip.qq.com ai"
```

### Task 4.2: blob 轴 FFI + oj-blob-s3

**Files:**
- Modify: `oj-plugin-ffi`（blob vtable）、`src/bridge/ffi.rs`（FfiBlobBackend）
- Create: `oj-blob-s3/`（cdylib；LocalBlob 留 core 内置）
- Test: ffi.rs mock + `OJ_TEST_S3` env-gated

**Interfaces:**

```rust
#[repr(C)]
pub struct BlobBackendVtable {
    pub connect: extern "C" fn(name: RString, cfg: RString) -> FfiFuture, // 注册名透传（url 裁决）
    pub put: extern "C" fn(handle: u64, key: RString, bytes: RBytes, content_type: RString) -> FfiFuture,
    pub get: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub del: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub url: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub content_type: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
```

- [x] **Step 1: vtable mock 失败测试**（五方法转发 + Drop close）
- [x] **Step 2: 确认失败** → **Step 3: 实现适配器 + oj-blob-s3**（S3Blob 从 core blob.rs 迁入插件）→ **Step 4: 测试通过 + `OJ_TEST_S3` 回归** → **Step 5: 接线**（BlobRegistry 的 s3 工厂改经 FFI）→ **Step 6: Commit**

```bash
git commit -m "feat(blob): blob 轴 FFI——oj-blob-s3 cdylib，local 留内置

unix@vip.qq.com ai"
```

### Task 4.3: bus 轴 FFI + oj-bus-kafka / oj-bus-rabbitmq（deliver 回调）

**Files:**
- Modify: `oj-plugin-ffi`（bus vtable + HostContext 增 deliver 回调 = ABI bump）、`src/bridge/ffi.rs`（FfiEventBroker）
- Create: `oj-bus-kafka/`、`oj-bus-rabbitmq/`（cdylib）
- Test: ffi.rs mock + env-gated kafka/rabbitmq 集成测试

**Interfaces:**

```rust
// HostContext 新增（spec §3：UnboundedSender 过不了边界，改宿主注入回调）
pub deliver: extern "C" fn(topic: RString, payload: RString), // 插件线程调用，须非阻塞投递

#[repr(C)]
pub struct EventBrokerVtable {
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    pub publish: extern "C" fn(handle: u64, topic: RString, data: RString) -> FfiFuture,
    pub subscribe: extern "C" fn(handle: u64, topic: RString) -> FfiFuture, // 插件侧起消费循环 → deliver 回调
    pub close: extern "C" fn(handle: u64),
}
```

- [x] **Step 1: vtable mock 失败测试**（publish 转发；subscribe 后模拟插件调 deliver → host 侧 UnboundedSender 收到——host 侧 deliver 实现 = `tx.send(...)` 非阻塞投递）
- [x] **Step 2: 确认失败** → **Step 3: 实现**（Kafka/RabbitMQ broker 从 core broker/ 迁插件；core 保留 local Bus 内置）→ **Step 4: 测试通过 + 共享语义回归**（FFI broker 下"同一实例跨 actor/WS 共享"仍成立——Task 0.5 回归测试在插件 broker 配置下重跑）→ **Step 5: 接线**（BusBackendRegistry 的 kafka/rabbitmq kind 改经 FFI）→ **Step 6: Commit**

```bash
git commit -m "feat(bus): bus 轴 FFI——deliver 回调注入 + kafka/rabbitmq cdylib

unix@vip.qq.com ai"
```

### Task 4.4: kv 轴 FFI + oj-kv-redis

**Files:**
- Modify: `oj-plugin-ffi`（kv vtable）、`src/bridge/ffi.rs`（FfiKVStore）
- Create: `oj-kv-redis/`（cdylib；InMemoryKV 留 core 内置兜底）
- Test: ffi.rs mock + `OJ_TEST_REDIS` env-gated

**Interfaces:**

```rust
#[repr(C)]
pub struct KVStoreVtable {
    pub connect: extern "C" fn(cfg: RString) -> FfiFuture,
    pub get: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub set: extern "C" fn(handle: u64, key: RString, value: RString, ttl_secs: u64) -> FfiFuture,
    pub del: extern "C" fn(handle: u64, key: RString) -> FfiFuture,
    pub close: extern "C" fn(handle: u64),
}
```

（方法面以 core `KVStore` trait 现状为准——先看 src/bridge/kv.rs:19 的完整方法列表再定稿 vtable，缺的方法补齐，签名形态同上。）

- [x] **Step 1: vtable mock 失败测试** → **Step 2: 确认失败** → **Step 3: 实现**（RedisKV 迁插件）→ **Step 4: 测试通过 + `OJ_TEST_REDIS` 回归** → **Step 5: 接线**（redis.default 配置改经 FFI，未配仍 InMemoryKV）→ **Step 6: Commit**

```bash
git commit -m "feat(kv): kv 轴 FFI——oj-kv-redis cdylib，InMemoryKV 内置兜底

unix@vip.qq.com ai"
```

### Task 4.5: core 瘦身 + CI 平台矩阵

**Files:**
- Modify: `Cargo.toml`（core sqlx features 去掉 mysql/postgres；rdkafka/lapin/redis 依赖移除或转 dev）
- Create/Modify: `.github/workflows/*.yml`（或现状 CI 配置——先 `ls .github/workflows/` 确认）
- Test: `cargo tree` 验证

- [x] **Step 1: core 依赖收敛**——sqlx features 收敛 `["runtime-tokio","macros","any","sqlite","json"]`（SqliteBackend 经 `sqlx::Any`（SqlxAccessor），保留 any 仅 sqlite 驱动；去掉 mysql/postgres）；redis/rdkafka/lapin 从 core dependencies 移除。

- [x] **Step 2: 瘦身验证**

Run: `cargo tree -p mdm-base-rust -e normal | grep -Ei 'mysql|postgres|redis|rdkafka|lapin'`
Expected: 无输出（core 二进制不再链接这些驱动）

- [x] **Step 3: 全量回归**

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿

- [x] **Step 4: CI 平台矩阵**——宿主 + 全部第一方插件按矩阵构建（`x86_64-unknown-linux-gnu`（最旧受支持 glibc 镜像为基线）/ 按需 `x86_64-unknown-linux-musl` / `aarch64-apple-darwin` / `x86_64-pc-windows-msvc`），产物归置 `plugins/<triple>/` 布局；Windows 构建优先 vendored 依赖。

- [x] **Step 5: Commit**

```bash
git add -A
git commit -m "build(core): 瘦身收敛 sqlite-only + CI 平台矩阵产物归置 plugins/<triple>/

unix@vip.qq.com ai"
```

### Task 4.6: 阶段 4 任务状态更新

- [x] **Step 1: 勾选 Task 4.1-4.5 复选框**
- [x] **Step 2: 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 阶段 4 完成——五轴全量 cdylib 化，core 收敛 sqlite-only

unix@vip.qq.com ai"
```

---

## 阶段 5 — 文档

### Task 5.1: 手册与插件开发指南

**Files:**
- Modify: `docs/dev-manual.md`、`docs/user-manual.md`
- Create: `docs/plugin-development.md`（第三方插件开发指南）

- [x] **Step 1: 写文档**——清单与扫描双模式语义、目录布局 `plugins/<triple>/`、升级回滚流程（`.new`/`.bak` + `cargo xtask plugin --check` + ABI bump 部署顺序）、FFI 契约（oj-plugin-ffi 类型面、ABI_VERSION 纪律、契约演进总则）、第三方插件开发指南（插件须自包含或显式声明系统依赖、panic=unwind、入口宏用法）、panic 归因流程（panic hook 输出 + symbols/ 目录）。

- [x] **Step 2: Commit**

```bash
git add docs/
git commit -m "docs: 插件系统手册——装配语义/布局/升级回滚/FFI 契约/第三方开发指南

unix@vip.qq.com ai"
```

### Task 5.2: 阶段 5 任务状态更新

- [x] **Step 1: 勾选 Task 5.1 复选框 + 进度提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 阶段 5 完成——文档落地

unix@vip.qq.com ai"
```

---

## 阶段 6 — 总验收与 review

**全部正式验收集中在本阶段（用户要求）。中间阶段的测试绿只是工程纪律，不代表验收通过。**

### Task 6.1: 硬验收全项

**Files:**
- Test: `tests/acceptance/`（或既有集成测试位，按仓库布局）

- [x] **Step 1: FFI 运行时验收（spec §8 阶段 3 硬验收）**——oj-es 插件 cdylib 内真实执行：sqlx 连接查询（插件内嵌 sqlite 测试库）+ reqwest 请求（本地 mock server）+ `tokio::time::sleep`，**不 panic**（无 "there is no reactor running"）。

- [x] **Step 2: 全链路集成测试**——host op → FFI 同步方法 → 插件 runtime 异步执行 → FfiFuture 完成 → host 拿结果，五轴各至少一条链路。

- [x] **Step 3: 瘦身验收**——`cargo tree -p mdm-base-rust -e normal | grep -Ei 'mysql|postgres|redis|rdkafka|lapin'` 无输出。

- [x] **Step 4: blob 裁决验收**——非 default local 后端 `url()` 报指定文案；default 下载路由字节一致回归。

- [x] **Step 5: broker 共享语义验收**——插件 broker（kafka/rabbitmq，env-gated）配置下，同一实例跨 actor 池与全部 WS 连接共享（Task 0.5 回归测试在插件配置下通过）。

- [x] **Step 6: fail fast 矩阵验收**——逐一构造并断言启动报错文案：清单文件缺失 / ABI 不符 / semver 不满足 @约束 / 插件身份不符 / 注册名冲突 / scheme 交集冲突 / 配置声明但插件未装 / 扫描到损坏插件。

- [x] **Step 7: op_plugins 输出核对**——插件名/semver/ABI/指纹 + 宿主 ABI_VERSION 齐全。

- [x] **Step 8: panic 围堵验收**——测试插件 init 期 panic 与运行期 panic 各一例：宿主进程不终止，错误归因含插件上下文与构建指纹。

- [x] **Step 9: 全量测试绿**

Run: `cargo test --workspace -- --skip infinite_loop` + 各 `OJ_TEST_*` env 可用时全跑
Expected: 全绿

- [x] **Step 10: Commit**

```bash
git add tests/
git commit -m "test(acceptance): 插件系统硬验收全项落地

unix@vip.qq.com ai"
```

### Task 6.2: 代码 review

- [ ] **Step 1: 调用 superpowers:requesting-code-review**——对本计划全部产出做完成度 review（重点：ffi.rs 全部 unsafe 的审计清单两项「Library 句柄不 drop」「panic=unwind profile」、适配器层转发正确性、插件互不可见边界、spec §6 fail fast 清单逐条对应）。

- [ ] **Step 2: 吸收 review 意见**——按 superpowers:receiving-code-review 处理；修复项各自 TDD 提交。

- [ ] **Step 3: Commit**

```bash
git commit -m "fix(review): 插件系统完成度 review 意见吸收

unix@vip.qq.com ai"
```

### Task 6.3: 收尾

- [ ] **Step 1: 全量最终回归**

Run: `cargo test --workspace -- --skip infinite_loop`
Expected: 全绿

- [ ] **Step 2: 勾选 Task 6.1-6.3 复选框 + 计划终态提交**

```bash
git add docs/superpowers/plans/2026-08-25-plugin-system.md
git commit -m "docs(plan): 插件系统计划全部落地——阶段 0-6 收官

unix@vip.qq.com ai"
```

- [ ] **Step 3: spec 状态头更新**——`docs/superpowers/specs/2026-08-25-plugin-system-design.md` 状态改「已实现」并提交。
