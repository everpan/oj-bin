# 插件化架构方案（blob / s3 / redis / ES / db 按方言）

> 状态：方案（未执行）。目标：把后端能力（blob、minio/s3、redis、ES、SQL 方言）以插件方式加载，
> 实现编译期/启动期可选、可装卸、可扩展，且不与现有 200+ 测试冲突。
> 本方案是对既有代码现状的改造计划，所有文件行号基于当前 `only-js` 工作区。

---

## 1. 现状与硬约束

### 1.1 已实现依赖倒置的部分（实现层已是"插件形态"）

后端**实现**已通过 trait + `Extras`/`StableState` 注入：

- `BlobBackend`（`LocalBlob` + `S3Blob`）、`KVStore`（`InMemoryKV` + `RedisKV`）、
  `EsClient`、`DataAccessor` —— 均为 `Arc<dyn Trait>`，在
  `Bridge::with_dbs_and_loader`（`src/bridge/mod.rs:234`）里塞进 `Extras`（`src/bridge/mod.rs:85`）。
  换一个后端**实现**今天就能做到，不动任何 JS。

### 1.2 真正写死的部分（绑定层）

1. `extension!` 的 `ops = [...]` 是**静态列表**（`src/bridge/mod.rs:128-172`）——
   所有 blob/es/redis/db 的 op 都编译进同一个 `bridge_ext`。
2. `bootstrap.js` 硬编码 `globalThis.blob` / `globalThis.es` / `globalThis.redis` 的装配。
3. db 方言：当前由 `SqlxAccessor` 经 `sqlx::Any` + `install_default_drivers` 多路复用，
   方言仅由 `dialect_of(dsn)`（`src/bridge/db.rs:33`）挑选 sea-query builder；
   DSN→Accessor 在 `oj/src/server_cmd.rs:82-88`、`oj/src/build_cmd.rs:186-189` 处硬编码为
   `SqlxAccessor::arc(dsn)`。

### 1.3 硬约束（决定"动态"能做到什么程度）

op 命名空间在 `JsRuntime::new` 时一次性固定
（`src/bridge/runtime.rs:44-49`，`extensions: vec![bridge_ext::init(...)]`）。
池化后每个 runtime 共享同一份 op 表。因此即便"运行时动态加载"，也只能是在
`RuntimePool::new` **之前**把插件清单拼进 extensions 列表——**做不到热插拔已跑起来的 runtime**。
真正能热换的只有"trait 背后的实现"（swap 那个 `Arc<dyn>`），不是 op 本身。

db 轴的注册表同理：必须在 `BridgeBuilder::build()` 解析 DSN **之前**完成装配。

---

## 2. 统一插件框架

```rust
pub trait Plugin {
    fn name(&self) -> &str;
    /// 自带 ops + esm + state 闭包的 Extension；state 闭包在 JsRuntime::new 时
    /// 把后端 Arc 放进 OpState（插件完全自持状态，不再依赖 StableState 的 blob/es/redis 字段）。
    fn extension(&self) -> deno_core::Extension;
    /// 把实现填进 StableStateBuilder（或注册表）。
    fn register(&self, b: &mut StableStateBuilder) -> Result<()>;
}
```

`BridgeBuilder::new(db, kv, registry, inspect).plugin(Box<dyn Plugin>).build()`：

- `build()` 先跑所有 `register` 攒齐 `StableState` 各字段与 `db_backends` 注册表；
- 用拼装好的扩展列表 `vec![bridge_ext::init(stable), plugin.extension()...]` 构造 `RuntimePool`
  （`RuntimePool::new` 改为接收 `Vec<Extension>`）；
- 扩展加载顺序固定为 **core 在前、插件在后**：core 的 `bootstrap.js` 先给
  `globalThis.blob/es/redis` 装**占位 stub**（`notConfigured('blob')` → 调用即报 "… not configured"，
  保留现有友好语义）；插件 esm 随后覆盖。

**零 unsafe**：方案为编译期链接同一工具链，无 `.so` 边界，`Arc<dyn>` 跨 crate 安全（与现状一致）。

---

## 3. db 按方言插件化（独立轴）

db 的 op 是方言无关的（`op_db_query`/`op_db_exec`/`op_db_tx_*` 在 `src/bridge/db.rs`），
故 db 的"插件单元"不是新 op 扩展，而是**方言连接工厂 `DbBackend`**——注册进
`DbBackendRegistry`，由 DSN 经注册表解析出 `Arc<dyn DataAccessor>`。

```rust
#[async_trait]
pub trait DbBackend: Send + Sync {
    fn name(&self) -> &str;                          // "sqlite"/"mysql"/"postgres"/"memory"
    fn dialect(&self) -> Dialect;                    // 选 sea-query builder 用
    fn accepts(&self, dsn: &str) -> bool;            // 按 DSN 前缀认领
    async fn connect(&self, dsn: &str)
        -> BridgeResult<Arc<dyn DataAccessor>>;      // 产出 Accessor（可 feature-gate）
}

/// 有序注册表：首个 accepts 命中者胜出。
pub struct DbBackendRegistry { backends: Vec<Arc<dyn DbBackend>> }
impl DbBackendRegistry {
    pub fn builtin() -> Self;                        // [sqlite, mysql, postgres, memory]
    pub fn register(&mut self, b: Arc<dyn DbBackend>);
    pub async fn connect(&self, dsn: &str) -> BridgeResult<Arc<dyn DataAccessor>>;
}
```

- **内置后端**（`src/bridge/db_backend.rs`）：`SqliteBackend` / `MySqlBackend` / `PostgresBackend`
  委托现有 `SqlxAccessor::connect`（`src/bridge/accessor_sqlx.rs`）复用一个实现；
  `MemoryBackend` 返回 `InMemoryAccessor`（`src/bridge/db.rs:85`）。
- **`StableState`** 新增 `db_backends: Arc<DbBackendRegistry>`（`src/bridge/mod.rs:68`）。
- **DSN 解析改道**：`oj/src/server_cmd.rs:82-88`、`oj/src/build_cmd.rs:186-189` 的
  `SqlxAccessor::arc(dsn)` 换成 `registry.connect(dsn)`——未知前缀 DSN 现在**明确报错**
  （而非静默回落 sqlite）。
- **自省 op**：新增核心 op `op_db_backends`（列已注册方言），`bootstrap.js` 暴露 `db.backends()`。

### db 如何并入统一 `Plugin` 路径

每个方言后端 crate（`oj-db-sqlite` / `oj-db-mysql` / `oj-db-postgres`）实现 `Plugin`：

```rust
impl Plugin for SqliteDbPlugin {
    fn name(&self) -> &str { "db-sqlite" }
    fn extension(&self) -> deno_core::Extension { deno_core::Extension::default() } // db op 已在核心
    fn register(&self, b: &mut StableStateBuilder) -> Result<()> {
        b.db_backends.register(Arc::new(SqliteBackend));
        Ok(())
    }
}
```

`BridgeBuilder::build()` 先跑 `register` 攒齐 `db_backends`，再解析 DSN（若给定）。

---

## 4. 工作区布局（改造后）

```
only-js/        core: bridge_ext(核心 ops) + Plugin trait + BridgeBuilder
                      + StableState(含 db_backends 字段) + DbBackend/Registry + MemoryBackend
  src/bridge/
    ext.rs            ← 新增：Plugin trait + BridgeBuilder + StableStateBuilder
    db_backend.rs     ← 新增：DbBackend trait + DbBackendRegistry + 内置后端
    db.rs             → 收敛为 DataAccessor/TxSession/InMemoryAccessor（方言无关）
oj-blob/              crate: BlobBackend + LocalBlob + S3Blob + op_blob_* + 自己的 bootstrap 片段
crates/plugins/oj-kv-redis/          crate: RedisKV + op_kv_*(redis 变体) + bootstrap（挂 globalThis.redis）
crates/plugins/oj-es/                crate: EsClient + op_es_* + bootstrap（挂 globalThis.es）
oj-db-sqlite/         crate: SqliteBackend + (Plugin 注册)
crates/plugins/oj-db-mysql/          crate: MySqlBackend + (Plugin 注册，feature-gate sqlx/mysql)
crates/plugins/oj-db-postgres/       crate: PostgresBackend + (Plugin 注册，feature-gate sqlx/postgres)
```

---

## 5. 分阶段落地（每步可编译、全测试绿）

- **阶段 0 — 插桩（零功能迁移）**
  - 新增 `src/bridge/ext.rs`：`Plugin` trait + `BridgeBuilder` + `StableStateBuilder`。
  - `RuntimePool::new(stable, extensions: Vec<Extension>, inspect)`；
    `Bridge::with_dbs_and_loader` 内部走 `BridgeBuilder`（仍接收 `Extras` 以向后兼容）。
  - 新增 `DbBackend` trait + `DbBackendRegistry` + `StableState.db_backends`（默认 `builtin()`）
    + `op_db_backends` 自省 op；`oj`/`server`/`build_cmd` 的 DSN 解析改走注册表
    （默认配置行为不变）。

- **阶段 1 — `oj-es` 插件**：把 `src/bridge/es.rs` 整体迁入 `oj-es` crate
  （ops + `EsClient` + esm 片段）；core 删 `op_es_*`，`bootstrap.js` 的 `globalThis.es` 改占位 stub。
  `oj`/`server` 按配置决定是否 `builder.plugin(Box::new(EsPlugin::from_config(&cfg)))`。
  迁移 `es` 相关测试到"带 EsPlugin 构造"。

- **阶段 2 — `oj-kv-redis` 插件**：`RedisKV` + `op_kv_*(redis 变体)` 迁 `oj-kv-redis`；
  core 保留 `InMemoryKV` 作 `kv.*`。`bootstrap.js` 的 `globalThis.redis` 由插件装配。
  更新 redis 测试用 `RedisPlugin`（env-gated `#[ignore]` 保留）。

- **阶段 3 — `oj-blob` 插件**：`BlobBackend` + `LocalBlob` + `S3Blob` + `op_blob_*` 迁 `oj-blob`。
  core 可保留 `LocalBlob` 作默认（dev/local 开箱即用），`S3Blob` 走 feature/插件。
  `blob_roundtrip_via_extras` 改为用 `BlobPlugin` 构造。

- **阶段 4 — db 按方言拆插件**：内置后端迁为 `oj-db-{sqlite,mysql,postgres}` crate，
  各自实现 `DbBackend`+`Plugin`；`oj` 配置加 `db_plugins: [sqlite, mysql, postgres, memory]`
  控制加载，**并 feature-gate `sqlx` 的 `mysql`/`postgres` 驱动**——至此"只编 sqlite 的二进制"可行；
  `MemoryBackend` 留在 core（测试/dev 默认）。

- **阶段 5 — 配置驱动发现 + 文档**：`oj`/`server` 按配置发现插件清单 → `builder.plugin(...)`；
  更新 `dev-manual.md`/`user-manual.md`（插件清单、feature 开关、`OJ_TEST_*` 与 db 方言测试）。

---

## 6. 风险与注意

- **op 命名全局唯一**：跨 extension 重名会运行时 panic。阶段 1-3 必须保证 core 删干净对应 ops，不残留。
- **`Extras` 向后兼容**：阶段 0 在 `Extras`（`src/bridge/mod.rs:85`）加
  `db_backends: Option<Arc<DbBackendRegistry>>`（None=builtin），保持
  `with_dbs_and_loader` 签名不变，所有现有调用点
  （`server/src/lib.rs` 多个 `with_dbs_and_loader`、`src/main.rs:29`）零改动。
- **db 阶段的 `sqlx` 复用 vs 真独立**：阶段 0-3 可暂沿用 `SqlxAccessor` 的 `Any` 复用，
  先完成"注册表/可装卸"语义；阶段 4 才把 `sqlx` 三个 driver feature 拆开、由各后端 crate
  各自 `connect` 对应类型池——这是阶段 4 的主要工作量。
- **allocator 边界**：方案 B 是编译期链接同一工具链，无 `.so` 边界，`Arc<dyn>` 跨 crate 安全。
  若未来要做运行时 `.so` 动态库（libloading + C-ABI），需注意 `Arc<dyn>` 跨 `.so` 边界
  allocator 不匹配→drop 时 UB、V8 isolate 线程亲和、deno `Extension` 非 `repr(C)` 过不了边界——
  故本轮不采用。
- **测试**：`accessor_sqlx.rs` 直接 `SqlxAccessor::arc` 的用例保留；新增 `DbBackendRegistry`
  测试（accepts 优先级、未知 DSN 报错、`memory` 后端、`sqlite` 经注册表连通）+ `op_db_backends`
  自省测试。`OJ_TEST_REDIS`/`OJ_TEST_S3`/`OJ_TEST_ES` 的 `#[ignore]` 集成测试保留。
