# 插件系统设计：JS 绑定层全量插件化（启动期配置装配）

> 状态：已评审待落地。本文是 `docs/plugin-architecture.md`（下称"原案"）的评审修订版；
> 原案的阶段 0-5、风险清单、`Plugin`/`DbBackend` trait 定义继续有效，本文只记录
> 评审后的**决策定稿**与**对原案的增量**。冲突处以本文为准。

## 决策定稿（评审结论）

| 议题 | 结论 |
|---|---|
| 装配方式 | **启动期配置装配**：编译期同工具链链接（零 unsafe），`plugins` 清单在启动期决定加载哪些插件。**不做**运行时 `.so`/热插拔——理由见原案 §1.3（op 表在 `JsRuntime::new` 冻结）与 §6（`Arc<dyn>` 跨 `.so` allocator UB、`Extension` 非 `repr(C)`、V8 线程亲和） |
| 插件化范围 | **所有绑定到 JS 的模块均可插件化** |
| core 边界 | **实用 core**：core 保留 `finish/json/http/log/module_loader/ws/fetch`（请求生命周期刚需 + 无外部依赖的小件）；接外部资源的 `kv/db/blob/bus/es` 全部走插件 |
| 装配配置 | **`plugins` 清单**为唯一入口：`plugins = ["kv-redis", "db-sqlite", "blob-s3", "bus-kafka", "es"]`；缺省 = 当前全量内置清单（老配置零迁移、行为不变）；插件参数（s3 bucket、kafka brokers 等）不搬家，各插件 `from_config(&cfg)` 自取 |
| db 多方言 | 已覆盖：`DbBackendRegistry` 按 DSN 前缀认领，多方言并存（`DB("pg")`/`DB("mysql")` 各自解析） |
| blob 多后端 | **命名多后端**，镜像 `DB(name)`（详见 §2） |

## 1. 分层架构

```
Layer 3  装配层    oj / server：读 plugins 清单 → PluginRegistry 按名取 → builder.plugin(...)
Layer 2  插件层    注册表轴插件：oj-db-{sqlite,mysql,postgres}  → DbBackendRegistry
                              oj-blob-{local,s3}               → BlobBackendRegistry   ← 新增轴
                              oj-bus-{kafka,rabbitmq}          → BusBackendRegistry    ← 新增轴
                   能力插件：    oj-es（自带 op_es_* + esm）
                              oj-kv-redis（自带 op_kv_* redis 变体 + esm）
Layer 1  框架层    core 新增 ext.rs：Plugin trait + BridgeBuilder + StableStateBuilder
                   + PluginRegistry（按名查找，启动期解析清单，未知名 → 启动即报错）
Layer 0  核心层    JsRuntime 壳 + finish/json/http/log/module_loader/ws/fetch ops
                   + 三轴的后端无关 ops：op_db_* / op_blob_* / op_bus_* 全部留 core
                   + bootstrap.js 骨架（缺席命名空间装 notConfigured 占位 stub）
```

统一范式：**core 持有全部既有 op（后端无关），插件只贡献"工厂注册"或"新 JS 命名空间"**。
能力插件（es/kv-redis）是例外：它们引入新命名空间，自带 ops，迁出时 core 必须删净对应 ops
（op 名全局唯一，重名运行时 panic）。

## 2. blob 命名多后端（对原案阶段 3 的修订）

- `StableState.blob: Arc<dyn BlobBackend>` 改为 `blob_backends: Arc<BlobBackendRegistry>`，
  注册表与 `DbBackendRegistry` 同构（`accepts`/`connect`/首个命中胜出）；
- `op_blob_*` 增加 name 参数，从注册表解析后端；`LocalBlob` 为内置 `default`，
  S3/minio 由 `oj-blob-s3` 插件注册；
- bootstrap.js：`globalThis.blob = blob("default")`（旧代码零改动），新增
  `blob(name)` 工厂——与 `DB(name)` 完全对称；
- 配置：`[blob.backends.local] path=...`、`[blob.backends.s3] bucket=...`；
  配置声明了名字但对应插件未装配 → 启动期报错（fail fast）；
  JS 侧 `blob("未配置名")` → 首次调用期明确报错。

## 3. 运行侧装配流程（启动期一次性）

```
oj.toml: plugins = ["kv-redis", "db-sqlite", "db-postgres", "blob-local", "blob-s3", "bus-kafka", "es"]
   │
   ▼  PluginRegistry::builtin() 按名过滤；查无此名/重复 → 启动即报错
   ▼  逐个 plugin.register(&mut StableStateBuilder)
      攒齐 kv/blob_backends/db_backends/bus_backends 各注册表
   ▼  extensions = [bridge_ext::init(stable), ...plugins.map(extension())]
   ▼  RuntimePool::new(stable, extensions, inspect)   ← 此后 op 表冻结，不可热插
```

扩展加载顺序固定 core 在前、插件在后；core 的占位 stub 先装、插件 esm 后覆盖。

## 4. 健壮性

- 未知插件名、重复注册、未知 DSN/broker/blob 名 → 全部启动期明确报错，无静默回落；
- op 命名全局唯一：能力插件迁出时 core 删净，迁移测试兜底；
- 缺席能力的 JS 调用报 `"... not configured"`（保留现有友好语义）；
- 零 unsafe、无 `.so` 边界：`Arc<dyn>` 跨 crate 安全。

## 5. 灵活性

- 编译期：feature-gate sqlx mysql/postgres 驱动后，"只编 sqlite 的二进制"可行；
- 启动期：同一二进制靠 `plugins` 清单换装配；
- 扩展期：第三方 crate 实现 `Plugin` + `PluginRegistry::register` 即接入，不动 core。

## 6. 落地顺序（原案阶段 0-5 + 两处修订）

原案阶段 0/1/2/4/5 不变，修订两处：

- **阶段 2.5（新增）— `oj-bus-*` 插件**：`bus.rs` 的 broker 抽象提为 `BusBackend`
  + `BusBackendRegistry`（镜像 db 轴），Kafka/RabbitMQ 绑定迁 `oj-bus-kafka`/`oj-bus-rabbitmq`，
  内存 broker 留 core 作默认；`op_bus_kind` 改从注册表自省。
- **阶段 3（修订）— blob 注册表轴**：先建 `BlobBackendRegistry`（local 内置 default），
  S3 迁 `oj-blob-s3` 插件；bootstrap.js 的 `blob(name)` 工厂在此阶段落地。

每阶段可编译、全测试绿；`OJ_TEST_*` env-gated 集成测试保留。
