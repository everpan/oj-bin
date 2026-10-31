//! bridge：注入到 deno_core JsRuntime 的 JS SDK 全局对象（JS 全局由 bootstrap.js 装配）。
//! 按绑定类型分文件组织：
//!   - json.rs     —— json.ok/fail/header 统一信封与返回头
//!   - db.rs       —— db.query/exec 与 DB(name) 命名实例（异步 op，支持绑定参数）
//!   - query.rs    —— 安全查询构造器（sea-query + SchemaRegistry 白名单，参数化值）
//!   - fetch       —— fetch 全局 = deno_fetch 扩展（WHATWG，见 ws_client_extensions）
//!   - http.rs     —— http.* 请求上下文（只读，懒加载）
//!   - kv.rs       —— redis 内存 KV 抽象（get/set，无 Redis 时联调用）
//!   - log.rs      —— log.debug/info/warn/error 结构化日志（tracing）
//!   - envelope.rs —— {code,msg,data} 统一信封
//!   - registry.rs —— SchemaRegistry 表/列白名单（SQL 注入根治点）
//!   - runtime.rs  —— RuntimePool：复用 JsRuntime（预热 = 快照等价），V8 代码缓存
//!   - loader.rs   —— HandlerStore：handler 源码加载 + 热重载（FS / 嵌入）
//!   - transpile.rs—— TS→JS 转译（deno_ast strip types）+ mtime 全局缓存
//!   - inspector.rs—— InspectorServer：deno_core 自带 inspector 的 DevTools WS 桥
//!   - bootstrap.js —— JS 侧全局对象装配
//!
//! 状态拆分（revised）：
//! - `StableState`：跨请求不变（kv / dbs / client / registry / loader），`Arc` 共享，创建一次。
//! - `ReqState`：每请求可变（req / 响应捕获 / done），存入 OpState，checkout 时重置。
//!
//! 如此 JsRuntime 可池化复用而不串号请求。

mod accessor_sqlx;
pub mod audit;
pub mod auth;
pub mod blob;
pub mod broker;
mod bus;
pub mod bus_backend;
pub mod cert;
pub mod client_db;
mod crypto;
mod db;
pub mod db_backend;
pub mod db_ddl;
pub mod db_error;
pub mod db_exec;
mod db_explain;
pub mod db_export;
pub mod db_import;
mod db_introspect;
pub mod db_policy;
mod db_query;
pub mod db_quota;
mod db_seq;
pub mod db_session;
mod db_stream;
mod db_tx;
pub mod dialect_caps;
mod embedded_js;
mod envelope;
mod es;
pub mod exec;
pub(crate) mod ffi;
pub mod frame_pool;
pub mod fs;
pub mod generic_axis;
pub mod guard;
pub mod handle_core;
mod http;
pub mod import_scan;
mod inspector;
mod introspect;
mod jsnum;
mod json;
mod kv;
pub mod ldap;
mod loader;
mod log;
pub mod mail;
pub mod mgmt;
mod module_loader;
pub mod module_scan;
pub mod mq;
mod named_registry;
pub mod oidc;
mod placeholder;
pub mod platform;
pub mod plugin_loader;
mod plugins_op;
mod query;
mod registry;
pub mod reload_bridge;
mod runtime;
pub mod secret_axis;
pub mod sql_trace;
pub mod sso;
mod stream;
pub mod task_pool;
pub mod transpile;
mod vars;
mod ws;
mod yaml;

pub use accessor_sqlx::SqlxAccessor;
pub use auth::AuthGuard;
pub use blob::{BlobBackend, BlobServed, LocalBlob, valid_key};
pub use bus::{Bus, BusPayload, EventBroker};
pub use bus_backend::{BusBackend, BusBackendRegistry};
pub use client_db::{ClientDbEntry, ClientDbRegistry};
pub use crypto::JwtCfg;
pub use db::{DataAccessor, Dialect, InMemoryAccessor, Row};
pub use db_backend::{DbBackend, DbBackendRegistry};
pub use embedded_js::patch_fs_loaded_sources;
pub use envelope::{fail, ok, status_code};
pub use es::EsBackend;
pub use http::{RequestInfo, UploadedFile};
pub use kv::{InMemoryKV, KVStore};
pub use ldap::{FfiLdapBackend, LdapBackend, LdapConfig};
pub use loader::HandlerStore;
// resolve_relative / resolve_alias 一并导出：oj build 的导入实化用**同一份探针**，
// 使 dev 与 release 对同一 specifier 命中同一文件（dev 能跑 release 炸的一类缺陷）。
pub use module_loader::{
    LoaderShared, OjModuleLoader, resolve_alias, resolve_relative, strip_verbatim,
    versioned_specifier,
};
// 模块索引：源码树 → 模块根 → 模块名 / URL 前缀。运行期（别名解析）与构建期（导入实化、
// 路由、检查）共用同一份发现结果，避免「dev 认、build 不认」的分叉。
pub use module_scan::{
    ModuleEntry, ModuleIndex, manifest_head, resolve_prefix, resolve_roots, stray_manifests,
    validate_module, validate_prefix, validate_version,
};
pub use mq::MqInstance;
pub use named_registry::NamedRegistry;
pub use plugin_loader::PluginInfo;
/// 构造器 LIMIT 配置（`db_query:` 段；v0.1.20）——config 与装配层都要用。
pub use query::QueryLimits;
pub use registry::{ColumnType, SchemaRegistry};
// boot_runtime 供 oj 的 test 运行时复用（`oj test` 不走 RuntimePool，直接建 JsRuntime）。
pub use runtime::{
    BOOT_TIMEOUT, boot_runtime, heap_create_params, heap_guard_fired, install_heap_limit_callback,
};

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use deno_core::error::CoreError;
use deno_core::{JsRuntime, ModuleSpecifier, OpState, PollEventLoopOptions, op2};

/// 契约实现（DataAccessor/KVStore）的统一错误返回（stdlib，不泄漏 deno 类型）。
pub type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// 模块执行上下文（P2 §5.3）：manifest 的 db/deps 声明 + 模块名 + 版本。
/// 装配期从 manifest.yaml 汇总，按模块目录绝对路径存入 StableState.modules；
/// 派发时 `run_module` 经 api_path 祖先目录命中后写入 `ReqState.module`
/// （+ `module_version`，`http.module` 的数据源）。
#[derive(Clone)]
pub struct ModuleCtx {
    pub name: String,
    /// manifest version：dev = src/<m>/manifest.yaml；release = dist/<m>-<v>/manifest.yaml。
    /// `http.module.version` 的数据源（装配期冻结，运行期不可变）。
    /// JS 可见面只经 `op_http_info` 显式投影——禁对本结构 derive Serialize（防 DTO 腐化）。
    pub version: String,
    /// manifest deps 声明的模块名集合（跨模块表访问守卫的放行依据）。
    pub deps: Arc<std::collections::HashSet<String>>,
    /// manifest db 绑定：Some(name) → 模块的 "default" 库调用重定向到该命名库。
    pub db: Option<String>,
}

/// 多租户 SQL 防护模式（tenant.sql_guard；装配期冻结进 StableState，首 run 前不可变）。
/// 反序列化在 config.rs（接受 bool true/false 与字符串 "off"/"warn"/"deny"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SqlGuard {
    /// 不启用（默认）。
    #[default]
    Off,
    /// 仅告警（软过渡：注入照做、缺失/逃逸口不拒绝，eprintln 告警）。
    Warn,
    /// 缺租户条件即拒绝（fail-closed）。
    Deny,
}

/// StableState：跨请求共享、创建后不可变（内部句柄均为 Arc）。
pub struct StableState {
    pub kv: Arc<dyn KVStore>,
    pub dbs: HashMap<String, Arc<dyn DataAccessor>>,
    pub registry: Arc<SchemaRegistry>,
    /// oj 模块加载配置（node_modules 回溯上界 + CJS require 的 project_root）。
    /// T9 装配注入；devserver 旧路径不配（裸 specifier / __ojRequire 不可用）。
    pub loader: Option<Arc<module_loader::LoaderShared>>,
    /// 模块索引（`--module-root` 递归发现的结果；None = 今日布局「src 根首层子目录 =
    /// 模块根」）。别名 `#/<name>/…` 首段按 `manifest.name` 在此查锚点。装配期冻结、只读。
    pub module_index: Option<Arc<ModuleIndex>>,
    /// blob 注册表（阶段 0 单后端形态：至多一个 "default"；阶段 1 命名多后端）。
    pub blobs: Arc<blob::BlobRegistry>,
    /// 事件总线（统一契约 `EventBroker`）：进程内 `Bus` 或分布式 Kafka/RabbitMQ；
    /// server 装配共享一个跨连接广播；缺省每 Bridge 自带进程内 Bus。
    pub bus: Arc<dyn bus::EventBroker>,
    /// ES 客户端（OJ-6）：es 配置存在时注入；否则 es.* 报 "es not configured"。
    pub es: Option<Arc<dyn es::EsBackend>>,
    /// 已装配插件自省信息（op_plugins 输出源；装配层注入）。
    pub plugins: Vec<PluginInfo>,
    /// 模块执行上下文（键 = 模块目录绝对路径；P2 §5.3）。空 = 无模块上下文（守卫不设防）。
    pub modules: Arc<HashMap<String, ModuleCtx>>,
    /// 表归属守卫模式（P2 §5.3）：false=warn（默认，日志告警）；true=deny（违规拒绝）。
    pub ownership_deny: bool,
    /// 多租户 SQL 防护模式（tenant.sql_guard；Off=不设防）。
    pub sql_guard: SqlGuard,
    /// `db.asTenant` 开关（tenant.allow_as_tenant，v0.1.20）：默认 false。
    /// 匿名请求显式声明租户身份的逃生口 —— 面向公开面且 id 无从校验，故 fail-closed。
    pub allow_as_tenant: bool,
    /// 运行期默认库重定向（`oj test` 的 db.test；v0.1.20）。仅重定向字面 "default"。
    pub db_override: Option<String>,
    /// 构造器 LIMIT：隐式默认 + 显式硬顶（db_query 段，v0.1.20）。
    pub query_limits: QueryLimits,
    /// SQL 执行追踪配置（dev 日志 + 每请求画像；v0.1.51）。装配期冻结、只读。
    pub sql_trace: sql_trace::SqlTraceConfig,
    /// `db.nextSeq` 的「平台序列表已确保」缓存（键 = 库名；v0.1.24）：DDL 每个库只跑一次。
    /// **必须放在 Bridge 级**（不能是全局 static）——每个 Bridge 对应一份 DB 配置与连接池，
    /// 全局缓存会在「同一进程里多个独立库（含测试的内存库）」之间误判。
    pub seq_ensured: std::sync::Mutex<std::collections::HashSet<String>>,
    /// 命名 MQ 客户端（Kafka(name)/RabbitMQ(name) 数据源；spec 2026-09-07 §4）。
    /// 段未配置 = 空 registry（op_mq_has 恒 false → JS 侧 undefined）。
    pub kafkas: Arc<NamedRegistry<mq::MqInstance>>,
    pub rabbits: Arc<NamedRegistry<mq::MqInstance>>,
    /// 任务上下文标志（评审 M2/N：任务 Bridge 注 Some，HTTP/WS Bridge 注 None）。
    pub tasks_flag: Option<Arc<AtomicBool>>,
    /// 裸 SQL 表名提取 memo（守卫热路径缓存；键 = SQL 原文）。
    pub sql_memo: Mutex<HashMap<String, Arc<Vec<String>>>>,
    /// PR-4：单 isolate 堆限额（字节）；None = 不限额（测试/嵌入式默认）。
    /// 生产装配恒 Some（server.js_heap_limit_bytes，默认 256 MiB）。
    pub js_heap_limit: Option<usize>,
    /// ext_boot 模块 specifier（装配期冻结的 `file://…?v=<mtime>`）；None = 无 boot。
    /// 每个新 JsRuntime 创建后加载执行一次（见 `runtime::RuntimePool::checkout`）。
    pub boot: Option<String>,
    /// jwt 配置（装配层从 config.auth 构建）；None = jwt.* 报 "jwt not configured"。
    pub jwt: Option<Arc<JwtCfg>>,
    /// OIDC 配置态（装配层从 config.oidc 构建）；None = oidc.* 报 "oidc not configured"。
    pub oidc: Option<Arc<oidc::OidcState>>,
    /// HTTP 表单 / CAS SSO 提供者（v0.1.61，`http_sso:` 段）。None =
    /// `sso.providers` 报 "http sso not configured"。
    pub sso: Option<Arc<serde_json::Value>>,
    /// mail 后端（装配层按 smtp: 段 + oj-mail 插件注入）；None = mail.* 报 "mail not configured"。
    pub mail: Option<Arc<dyn mail::MailBackend>>,
    /// ldap 后端（装配层按 ldap: 段 + oj-ldap 插件注入）；None = ldap.* 报 "ldap not configured"。
    pub ldap: Option<Arc<dyn ldap::LdapBackend>>,
    /// db 访问控制策略表（config `db_access:` 段，v0.1.89）：键 = db 别名，
    /// `defaults` 兜底未声明的库。装配期冻结、只读。
    pub db_access: Arc<db_policy::DbAccessSection>,
    /// 动态数据源登记配置段（`db_client:`，L3 v0.1.91）：登记门（谁能建连接）。
    /// 装配期冻结、只读。
    pub db_client: Arc<db_policy::DbClientSection>,
    /// 跨请求事务会话配置段（`db_session:`，L7 v0.1.95）。缺省 enabled=false
    /// （与升级前完全一致：无跨请求事务能力）。装配期冻结、只读。
    pub db_session: Arc<db_policy::DbSessionSection>,
    /// 运行期动态数据源注册表（L3 v0.1.91）：壳 `Arc` 不可变、内部 `Mutex` 变动，
    /// 与 `reload: Arc<OnceLock>` 同哲学，不破 StableState 不可变红线。
    pub client_dbs: Arc<client_db::ClientDbRegistry>,
    /// 跨请求事务会话注册表（L7 v0.1.95，U-4）：壳 `Arc` 不可变、内部 `Mutex` 变动。
    /// **进程级生命周期**：热更沿用同一 `StableState`（`app::reuse_backend` 只
    /// clone `stable`），故会话跨热更存活（这是「跨请求」的应有语义）；配置变更走
    /// `restart_pending`（不与存活会话打架）。
    pub tx_sessions: Arc<db_session::TxSessionRegistry>,
    /// 安全审计配置段（U-9 v0.1.98，`audit:`）：`audit.query` 的读取门在查询时用。
    pub audit_section: Arc<db_policy::AuditSection>,
    /// 安全审计存储（U-9 v0.1.98）：统一事件出口的进程内有界环形缓冲。热更存活
    /// （装配层注入同一 Arc，与 tx_sessions 同款）。
    pub audit: Arc<audit::AuditStore>,
    /// 任务式导出（U-18 v0.1.104）：配置段 + 进程级任务登记表（配额/进度/状态机）。
    /// None 缺省 = 段落缺省值 + 新表（与 exec_sessions 同款生命周期）。
    pub export_section: Arc<db_policy::DbExportSection>,
    pub export_registry: Arc<db_export::ExportRegistry>,
    /// 任务式批量导入（REQ-12 v0.1.115）：配置段 + 进程级任务登记表（配额/进度/状态机）。
    /// None 缺省 = 段落缺省值 + 新表（与 export 同款生命周期，热更存活）。
    pub import_section: Arc<db_policy::DbImportSection>,
    pub import_registry: Arc<db_import::ImportRegistry>,
    /// 执行登记表（U-5 v0.1.96）：`db.cancel(executionId)` 的跨请求路由表。
    /// 进程级生命周期（热更存活），无配置段（取消判权只看归属主体）。
    pub exec_sessions: Arc<db_exec::ExecRegistry>,
    /// 管理面配置段（U-15）+ 装配期收编的泛型轴（**不进** `axis()` 公开面）+ 运行期登记表。
    pub mgmt_section: Arc<db_policy::MgmtSection>,
    pub mgmt_axes: generic_axis::GenericAxisRegistry,
    pub mgmt: Arc<mgmt::MgmtRegistry>,
    /// 运行期凭据密钥（U-10 v0.1.101）：`secret.seal/open` 的装配期数据源。
    /// None = 无密钥装配（op 层 fail-closed）。私钥**永不出 Rust 边界**。
    pub secret_keys: Option<Arc<crate::secret::RuntimeKeys>>,
    /// 运行期动态授权表（U-1 v0.1.93）：壳 `Arc` 不可变、内部 `Mutex` 变动，不破红线。
    /// 仅作用于「动态登记别名」（client_dbs 命中且非 config `db_access.by_db` 声明）：
    /// grant 写入、revoke 移除；静态 config 兜底不受其影响（防运行时提权覆盖 config 的 deny）。
    pub db_access_rt: Arc<Mutex<HashMap<String, db_policy::DbAccess>>>,
    /// 运行期连接工厂（与装配期同集：内置 sqlite/memory + 插件 mysql/postgres）。
    pub db_backend: Arc<db_backend::DbBackendRegistry>,
    /// sqlite 相对路径归一基准（connect DSN 用）；断言期 = config 目录。
    pub config_dir: Arc<std::path::PathBuf>,
    /// 部署期常量（config `vars:` 段，v0.1.25）：`vars.get(name)` 的唯一数据源。
    /// **fail-closed**：只有这里声明的键可读（空表 = 全 null）。装配期冻结、只读。
    pub vars: Arc<HashMap<String, String>>,
    /// 入参契约（v0.1.44）：键 = (api 文件绝对路径, js 方法名)。
    /// None = 校验关闭；Some(空表) = 无 handler 声明契约（当前全默认）。
    pub input_contracts: Option<Arc<std::sync::RwLock<crate::contract::InputContractRegistry>>>,
    /// 本地文件系统授权（config `fs:` 段；v0.1.53）；None = fs.* 报 PermissionDenied。
    pub fs: Option<Arc<fs::FsGrant>>,
    /// 平台特权授权（config `platform:` 段，ojm 批次）；None = 平台 API 恒
    /// "platform not configured"。见 [`platform::PlatformGrant`] 与 [`platform::assert_platform`]。
    pub platform: Option<Arc<platform::PlatformGrant>>,
    /// exec 授权（config `platform.exec:` 段；阶段 3 `exec.run` 消费）；
    /// None = exec 恒 NotCapable。
    pub exec: Option<Arc<platform::ExecGrant>>,
    /// 热更触发槽（ojm PR-3）：`oj serve` 装配期后填的 executor 闭包；槽空 =
    /// 非 serve 形态（`oj test` / 嵌入式）→ op 报 `reload not available`。
    /// 槽指针构造期冻结、内容后填（不触 `Arc::get_mut` 红线）；桥池多实例
    /// 共享同一槽（同一 `Arc<OnceLock>`），每代热更重建桥后依旧有效。
    pub reload: Arc<reload_bridge::ReloadSlot>,
    /// 泛型轴注册表（插件自报的非类型化轴；bootstrap `axis(name)` 的数据源）。
    /// 空表 = 无泛型轴，`axis(name).op()` 报 unknown axis 并列可用轴名。
    pub generic_axes: generic_axis::GenericAxisRegistry,
    /// ojInfo() 数据源（与 `oj info` CLI 同源的 OjInfo 经 serde_json::to_value 固化；
    /// v0.1.54）。装配期已脱敏（config 只含段名/键名，值一律不出）——**注入即公开**，
    /// op 层不得附加任何值。
    pub oj_info: Arc<serde_json::Value>,
}

/// bridge 可选能力注入（构造期一次）。
#[derive(Default)]
pub struct Extras {
    /// 模块索引（`--module-root` 递归发现结果）；缺省 None = 今日布局（别名按目录名解析）。
    pub module_index: Option<Arc<ModuleIndex>>,
    /// None = 零后端（blob.* 报 notConfigured）。
    pub blobs: Option<Arc<blob::BlobRegistry>>,
    /// Some = 共享总线（server 跨连接广播）；None = 每 Bridge 自带新 Bus（进程内）。
    pub bus: Option<Arc<dyn bus::EventBroker>>,
    /// Some = ES 客户端；None = es.* 未配置报错。
    pub es: Option<Arc<dyn es::EsBackend>>,
    /// 已装配插件自省信息（op_plugins 数据源；装配层注入）。
    pub plugins: Vec<PluginInfo>,
    /// 模块执行上下文（§5.3；缺省空表 = 守卫不设防）。
    pub modules: Arc<HashMap<String, ModuleCtx>>,
    /// 表归属守卫 deny 模式（缺省 false = warn）。
    pub ownership_deny: bool,
    /// 多租户 SQL 防护模式（缺省 Off = 不设防）。
    pub sql_guard: SqlGuard,
    /// `db.asTenant` 开关（缺省 false = 关闭）。
    pub allow_as_tenant: bool,
    /// 默认库重定向（`oj test` 注入；None = 用 default）。
    pub db_override: Option<String>,
    /// 构造器 LIMIT（缺省 = 内置默认 100 / 硬顶 1000）。
    pub query_limits: QueryLimits,
    /// SQL 执行追踪配置（缺省关；app 装配按 dev/release + config 决定）。
    pub sql_trace: sql_trace::SqlTraceConfig,
    /// ext_boot 模块 specifier（装配期冻结的 `file://…?v=<mtime>`）；None = 无 boot。
    pub boot: Option<String>,
    /// jwt 配置（装配层从 config.auth 构建）；None = jwt.* 报 "jwt not configured"。
    pub jwt: Option<Arc<JwtCfg>>,
    /// OIDC 配置态（装配层从 config.oidc 构建）；None = oidc.* 报 "oidc not configured"。
    pub oidc: Option<Arc<oidc::OidcState>>,
    /// HTTP 表单 / CAS SSO 提供者（v0.1.61，`http_sso:` 段）。None =
    /// `sso.providers` 报 "http sso not configured"。
    pub sso: Option<Arc<serde_json::Value>>,
    /// mail 后端（装配层构造 `mail::FfiMailBackend`）；None = mail.* 未配置报错。
    /// 注入时构造期一并挂 `deliver("mail.result")` 路由（见 `mail::install_mail_deliver`）。
    pub mail: Option<Arc<dyn mail::MailBackend>>,
    /// ldap 后端（装配层构造 `ldap::FfiLdapBackend`）；None = ldap.* 未配置报错。
    pub ldap: Option<Arc<dyn ldap::LdapBackend>>,
    /// 命名 MQ 客户端（None = 空 registry，Kafka/RabbitMQ(name) → undefined）。
    pub kafkas: Option<Arc<NamedRegistry<mq::MqInstance>>>,
    pub rabbits: Option<Arc<NamedRegistry<mq::MqInstance>>>,
    /// 任务上下文标志（Some 仅注入任务 Bridge，评审 M2）。
    pub tasks_flag: Option<Arc<AtomicBool>>,
    /// db 访问控制策略表（`db_access:` 段）；None = 空表（全库不设防，与升级前一致）。
    pub db_access: Option<Arc<db_policy::DbAccessSection>>,
    /// 动态数据源登记配置段（`db_client:`，L3 v0.1.91）。None = 默认（enabled=false）。
    pub db_client: Option<Arc<db_policy::DbClientSection>>,
    /// 跨请求事务会话配置段（`db_session:`，L7 v0.1.95）。None = 默认（enabled=false）。
    pub db_session: Option<Arc<db_policy::DbSessionSection>>,
    /// 跨请求事务会话注册表（装配层构造后注入，以便热更前后同源）。
    /// None = 按 `db_session` 配置现造一个（`oj test` / 嵌入式路径）。
    pub tx_sessions: Option<Arc<db_session::TxSessionRegistry>>,
    /// 安全审计段与存储（装配层构造后注入，以便热更前后同源）。None = 缺省段 + 新存储。
    pub audit_section: Option<Arc<db_policy::AuditSection>>,
    pub audit: Option<Arc<audit::AuditStore>>,
    /// 执行登记表（装配层构造后注入，以便热更前后同源）。None = 进程内新表。
    pub exec_sessions: Option<Arc<db_exec::ExecRegistry>>,
    /// 任务式导出段 + 登记表（U-18 v0.1.104，`db_export:`）。None = 段落缺省 + 新表。
    pub export_section: Option<Arc<db_policy::DbExportSection>>,
    pub export_registry: Option<Arc<db_export::ExportRegistry>>,
    /// 任务式批量导入段 + 登记表（REQ-12 v0.1.115，`db_import:`）。None = 段落缺省 + 新表。
    pub import_section: Option<Arc<db_policy::DbImportSection>>,
    pub import_registry: Option<Arc<db_import::ImportRegistry>>,
    /// 管理面（U-15）：段 + 收编轴 + 登记表。None = 缺省。
    pub mgmt_section: Option<Arc<db_policy::MgmtSection>>,
    pub mgmt_axes: Option<generic_axis::GenericAxisRegistry>,
    pub mgmt: Option<Arc<mgmt::MgmtRegistry>>,
    /// 运行期凭据密钥（U-10 v0.1.101，`secret.seal/open`）。None = 装配无密钥，
    /// 两个 op fail-closed（报 not configured）。
    pub secret_keys: Option<Arc<crate::secret::RuntimeKeys>>,
    /// 运行期动态数据源注册表（L3 v0.1.91）。None = 空 registry。
    pub client_dbs: Option<Arc<client_db::ClientDbRegistry>>,
    /// 运行期动态授权表（U-1 v0.1.93）。None = 空表（全库按 config 静态策略）。
    pub db_access_rt: Option<Arc<Mutex<HashMap<String, db_policy::DbAccess>>>>,
    /// 运行期连接工厂（含插件 mysql/postgres）。None = 内置 builtin（sqlite/memory）。
    pub db_backend: Option<Arc<db_backend::DbBackendRegistry>>,
    /// sqlite 相对路径归一基准（connect 用）。None = 当前目录。
    pub config_dir: Option<Arc<std::path::PathBuf>>,
    /// 部署期常量（config `vars:` 段，v0.1.25）；缺省空表 = `vars.get` 恒 null。
    pub vars: Arc<HashMap<String, String>>,
    /// PR-4 单 isolate 堆限额（字节）；None = 不限额（测试/嵌入式默认）。
    pub js_heap_limit: Option<usize>,
    /// 入参契约注册表（v0.1.44）；None = 不校验（缺省；Extras 是 Default 的，
    /// 故未显式注入的 Bridge 一律不设防，行为与升级前一致）。
    pub input_contracts: Option<Arc<std::sync::RwLock<crate::contract::InputContractRegistry>>>,
    /// 本地文件系统授权（config `fs:` 段；v0.1.53）；None = fs.* 双轴 deny
    /// （PermissionDenied）。只读语义见 [`fs::FsGrant`]。
    pub fs: Option<Arc<fs::FsGrant>>,
    /// 平台特权授权（config `platform:` 段，ojm 批次）；None = assert_platform 恒
    /// "platform not configured"。见 [`platform::PlatformGrant`]。
    pub platform: Option<Arc<platform::PlatformGrant>>,
    /// exec 授权（config `platform.exec:` 段；阶段 3 消费）；None = exec 恒 NotCapable。
    pub exec: Option<Arc<platform::ExecGrant>>,
    /// 热更触发槽（ojm PR-3）：缺省空槽 = `server.reloadConfig` 报
    /// `reload not available`（Extras 是 Default 的，`oj test` / 嵌入式零接线
    /// 即 fail-closed）。`oj serve` 装配层传入共享槽 Arc 并在 ReloadState
    /// 就绪后 `set`（槽位后填，照搬 `ReloadState::set_executor` 解环惯例）。
    pub reload: Arc<reload_bridge::ReloadSlot>,
    /// 泛型轴注册表（None = 空 registry，`axis(name)` 一律报 unknown axis——与
    /// vars 同哲学：未注入 = 空表而非 None）。
    pub generic_axes: Option<generic_axis::GenericAxisRegistry>,
    /// ojInfo() 数据源（None = 空对象 {}，与 vars 同哲学：未注入 = 空表而非 None；
    /// 值在装配层已脱敏，此处只接固化 JSON）。
    pub oj_info: Option<Arc<serde_json::Value>>,
}

/// ReqState：每请求可变状态（存在 OpState 中，checkout 时整体重置）。
/// 不 derive Clone：含活跃事务（ActiveTx 不可复制，clone 掉事务句柄就是漏回滚）。
#[derive(Default)]
pub struct ReqState {
    pub req: RequestInfo,
    pub response: Option<Vec<u8>>,
    pub status: u16,
    /// `json.raw` 置位（v0.1.110）：body 是裸 JSON、无 `{code,msg,data}` 信封——
    /// MCP/JSON-RPC 的结果映射据此豁免信封检测（raw 业务数据恰好带
    /// code/msg/data 键时不得误判成错误信封）。
    pub raw: bool,
    /// 响应头（有序、可重名——Set-Cookie 合法重复；写回时按名去重语义见 server capture_response）。
    pub headers: Vec<(String, String)>,
    pub done: bool,
    /// 活跃事务（db.tx；每请求至多一个，reset 时丢弃 = drop 自带回滚）。
    pub tx: Option<Arc<db::ActiveTx>>,
    /// 已绑定的**跨请求事务会话** id（L7 v0.1.95；`db.session.attach` 写入）。
    /// 只存 id、句柄留在 `StableState.tx_sessions`——会话的生命周期与请求无关，
    /// 请求结束不得回滚它（这正是跨请求事务存在的理由）。
    pub session: Option<String>,
    /// WS 帧循环专用：ws.send 收集（处理器结束后按序写出，先于信封响应）。
    pub ws_sends: Vec<WsSend>,
    /// WS 帧循环专用：ws.close 置位 → 本帧结束后关闭连接。
    pub ws_close: bool,
    /// WS 帧收尾（帧池 spec 2026-09-09）：dispatcher `finally` 把 `__sess` 快照交还
    /// （sess.state 外置 Rust 会话表的回传通道；None = 本帧未触发 finally 兜底）。
    pub ws_sess: Option<serde_json::Value>,
    /// 本请求所属模块名（§5.3 执行上下文；run_module 按目录命中注入；None = 无上下文）。
    pub module: Option<String>,
    /// 本请求所属模块版本（与 module 同点注入，唯一写点在 checkout_reset；
    /// `http.module.version` 的数据源；None = 无上下文）。
    pub module_version: Option<String>,
    /// db.asSystem() 逃生口：本请求以系统身份绕过租户防护（reset 即失效；
    /// sql_guard 活跃时调用处记审计日志）。默认 false。
    pub system: bool,
    /// 流式响应（v0.1.35）：`json.stream()`/`json.sse()` 开流时写入器 `tx` 与配对接收端 `rx`。
    /// `rx` 在 `read_capture` 时移入 `Capture.stream`，与 isolate 生命周期解耦；isolate
    /// 归还池不依赖流是否已写完（数据通道归 `Capture` 所有，由 server 排空）。
    /// 用 `RefCell` 包裹以便 `read_capture` 在「OpState 已被共享借用」的帧池路径下仍可取走
    /// （baseline 的 `read_capture` 走共享借用，帧池 worker 持有该借用跨调用）。
    pub stream_tx: std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedSender<bytes::Bytes>>>,
    pub stream_rx: std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>>>,
    /// SSE 心跳停止信号（仅 `json.sse()` 开流时存在）；`op_stream_end`/`read_capture` 触发，
    /// 让心跳任务（持有 `stream_tx` 克隆）干净退出并关闭流。
    pub stream_heartbeat_stop: std::cell::RefCell<Option<std::sync::Arc<tokio::sync::Notify>>>,
    /// PR-2 Phase A：`db.stream` 每请求流注册表（keyed by stream_id）。pump 持有 Arc 克隆推数据，
    /// `op_db_stream_next` 拉取；`reset` 换全新 Arc，旧 Arc 由仍存活的 pump 持有至排空，互不串。
    pub(crate) db_streams: std::sync::Arc<db::DbStreamRegistry>,
    /// SQL 执行追踪画像（v0.1.51）：本请求累计的 SQL 事件，dev 信封 `_sql` 与
    /// `db.sqlProfile()` 的数据源；`reset` 时清空。
    pub sql_profile: sql_trace::SqlProfile,
    /// http 快照 memo 的失效代数（perf）：`reset` 及请求中途改 `req` 的 op
    /// （op_db_as_tenant）自增。bootstrap 的 http Proxy 据此判断 memo 是否过期，
    /// 让 `http.*` 属性访问从「每次全量快照」降为「一次 smi fast op + memo 命中」。
    pub epoch: u32,
    /// 模块执行 driver 的异常回传（perf F2）：`__ojRun`（bootstrap）catch 后经
    /// op_driver_error 存入；run_side_driver 据此映射 RunError::Core。语义等价于
    /// 旧 side-module 路径的 mod_evaluate 错误（500 + 消息），但免每请求 module 装载。
    pub driver_error: Option<String>,
}

impl ReqState {
    /// 重置为初始态，写入新请求的上下文。
    pub fn reset(&mut self, req: RequestInfo) {
        self.req = req;
        self.epoch = self.epoch.wrapping_add(1);
        self.response = None;
        self.raw = false;
        self.status = 200;
        self.headers.clear();
        self.done = false;
        self.tx = None;
        self.session = None;
        self.ws_sends.clear();
        self.ws_close = false;
        self.ws_sess = None;
        self.module = None;
        self.module_version = None;
        self.system = false;
        self.stream_tx.borrow_mut().take();
        self.stream_rx.borrow_mut().take();
        self.stream_heartbeat_stop.borrow_mut().take();
        self.db_streams = std::sync::Arc::new(db::DbStreamRegistry::default());
        self.sql_profile = sql_trace::SqlProfile::default();
        self.driver_error = None;
    }
}

deno_core::extension!(
    bridge_ext,
    ops = [
        op_finish,
        op_driver_error,
        json::op_json_ok,
        json::op_json_fail,
        json::op_json_header,
        json::op_json_raw,
        json::op_json_redirect,
        stream::op_json_stream_open,
        stream::op_stream_write,
        stream::op_stream_write_bytes,
        stream::op_stream_end,
        http::op_http_info,
        http::op_http_epoch,
        http::op_http_file,
        http::op_http_body_bytes,
        kv::op_kv_get,
        kv::op_kv_set,
        kv::op_kv_del,
        kv::op_kv_expire,
        kv::op_kv_incr,
        db::op_db_has,
        db::op_db_allowed,
        db::op_db_as_system,
        db::op_db_as_tenant,
        db_query::op_db_query,
        db_query::op_db_exec,
        db_query::op_db_query_begin,
        db_query::op_db_exec_begin,
        db_query::op_db_call_poll,
        db_query::op_db_query_take,
        db_query::op_db_exec_take,
        db_ddl::op_db_ddl_diff,
        db_explain::op_db_explain,
        db_seq::op_db_next_seq,
        db::op_db_sql_profile,
        db_tx::op_db_tx_begin,
        db_tx::op_db_tx_commit,
        db_tx::op_db_tx_rollback,
        db_stream::op_db_stream_open,
        db_stream::op_db_stream_next,
        db_stream::op_db_stream_info,
        db_export::op_db_export,
        db_export::op_db_export_status,
        db_export::op_db_export_cancel,
        db_import::op_db_import,
        db_import::op_db_import_status,
        db_import::op_db_import_cancel,
        db_stream::op_db_stream_close,
        db_stream::op_db_stream_abort,
        query::op_db_query_build,
        query::op_db_query_sql,
        db::op_db_dialect,
        db_introspect::op_db_list_tables,
        db_introspect::op_db_list_schemas,
        db_introspect::op_db_list_databases,
        db_introspect::op_db_list_columns,
        db_introspect::op_db_list_indexes,
        db_introspect::op_db_list_foreign_keys,
        db_introspect::op_db_catalog,
        client_db::op_db_client_register,
        client_db::op_db_client_test,
        client_db::op_db_client_delete,
        client_db::op_db_client_re_register,
        client_db::op_db_client_list,
        client_db::op_db_client_has,
        client_db::op_db_access_grant,
        client_db::op_db_access_revoke,
        client_db::op_db_epoch,
        db_exec::op_db_cancel,
        audit::op_audit_query,
        mgmt::op_mgmt_register,
        mgmt::op_mgmt_delete,
        mgmt::op_mgmt_list,
        mgmt::op_mgmt_has,
        mgmt::op_mgmt_call,
        secret_axis::op_secret_seal,
        secret_axis::op_secret_open,
        db_session::op_db_session_begin,
        db_session::op_db_session_attach,
        db_session::op_db_session_detach,
        db_session::op_db_session_query,
        db_session::op_db_session_exec,
        db_session::op_db_session_commit,
        db_session::op_db_session_rollback,
        db_session::op_db_session_close,
        db_session::op_db_session_touch,
        db_session::op_db_session_close_all,
        db_session::op_db_session_list,
        db_session::op_db_session_info,
        db_session::op_db_session_stats,
        blob::op_blob_put,
        blob::op_blob_get,
        blob::op_blob_del,
        blob::op_blob_url,
        blob::op_blob_upload_url,
        blob::op_blob_content_type,
        blob::op_blob_copy,
        blob::op_blob_move,
        blob::op_blob_read_range,
        bus::op_bus_publish,
        bus::op_bus_publish_bin,
        bus::op_bus_subscribe,
        bus::op_bus_kind,
        es::op_es_search,
        es::op_es_index,
        es::op_es_del,
        mail::op_mail_send,
        mail::op_mail_send_sync,
        mail::op_mail_enqueue,
        mail::op_mail_send_raw,
        mail::op_mail_result,
        mail::op_mail_profiles,
        plugins_op::op_plugins,
        plugins_op::op_oj_info,
        vars::op_vars_get,
        log::op_log,
        log::op_log_enabled,
        module_loader::op_resolve_cjs,
        oidc::op_oidc_sign,
        oidc::op_oidc_verify,
        oidc::op_oidc_info,
        sso::op_sso_providers,
        ws::op_ws_send,
        ws::op_ws_send_bin,
        cert::op_cert_gen,
        cert::op_cert_renew,
        crypto::op_jwt_sign,
        crypto::op_jwt_verify,
        crypto::op_jwt_durations,
        crypto::op_bcrypt_hash,
        crypto::op_bcrypt_verify,
        crypto::op_sha256_hex,
        crypto::op_sha256_bytes,
        crypto::op_md5_hex,
        crypto::op_md5_bytes,
        crypto::op_ed25519_verify,
        crypto::op_random_hex,
        crypto::op_crypto_random,
        crypto::op_aes_gcm_encrypt,
        crypto::op_aes_gcm_decrypt,
        ws::op_ws_frame_close,
        ws::op_ws_sess_set,
        ws::op_ws_join,
        ws::op_ws_leave,
        ws::op_ws_broadcast,
        ws::op_ws_broadcast_bin,
        ws::op_ws_room_size,
        mq::op_mq_has,
        mq::op_mq_call,
        mq::op_tasks_stopping,
        mq::op_tasks_sleep,
        ldap::op_ldap_call,
        generic_axis::op_axis_call,
        fs::op_fs_resolve,
        exec::op_exec_run,
        exec::op_exec_spawn,
        exec::op_exec_read,
        exec::op_exec_kill,
        exec::op_exec_exit,
        yaml::op_yaml_parse,
        yaml::op_yaml_stringify,
        reload_bridge::op_server_reload_config,
    ],
    esm_entry_point = "ext:bridge_ext/bootstrap.js",
    options = { stable: Arc<StableState> },
    state = |state, options| {
        state.put(options.stable.clone());
        state.put(ReqState::default());
        // deno_websocket 的 op_ws_check_permission_and_cancel_handle 要求 OpState
        // 里有 PermissionsContainer——出站 WS 与 fetch 同一信任边界。
        // 放在 bridge_ext 的 state 闭包：HTTP 池 / 任务 / `oj test` 三路径统一生效。
        // fs 轴（v0.1.53）：net 等轴恒放行（出站行为不变），read/write 收窄到
        // config `fs:` 段的 jail（未配置 = 双 deny，PermissionDenied）。
        state.put(fs::container_for(options.stable.fs.as_deref()));
    },
);

/// bridge_ext 的 ESM 源（编译期内嵌）。
///
/// **不**用 `esm = [dir "src/bridge", "bootstrap.js"]`：deno_core 0.411 的 `dir` 形式会
/// 把 `concat!(CARGO_MANIFEST_DIR, ...)` 的**构建机绝对路径**编进二进制，且产出
/// `LoadedFromFsDuringSnapshot`；未启用 startup snapshot 时运行期按该路径读盘，在非
/// 构建机上必然 ENOENT（"Failed to initialize a JsRuntime"）。内嵌后与机器无关。
const BRIDGE_ESM: &[deno_core::ExtensionFileSource] = &[deno_core::ExtensionFileSource::new(
    "ext:bridge_ext/bootstrap.js",
    deno_core::ascii_str_include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/bridge/bootstrap.js"
    )),
)];

/// 构造 `bridge_ext`（bootstrap.js 编进二进制）。所有运行入口都应经此函数取扩展。
pub fn bridge_ext_init(stable: Arc<StableState>) -> deno_core::Extension {
    let mut ext = bridge_ext::init(stable);
    ext.esm_files = std::borrow::Cow::Borrowed(BRIDGE_ESM);
    ext
}

/// 出站网络扩展面（v0.1.8，spec 2026-09-08）：deno_fetch 提供 WHATWG `fetch`
/// 全局（bootstrap.js 从 ext:deno_fetch/26_fetch.js 导出挂载），deno_websocket
/// 提供 WHATWG `WebSocket`；两者 JS 经 core.loadExtScript 依赖 deno_web /
/// deno_webidl / deno_net 的扩展 JS，故五个扩展一并注册、顺序即依赖序。
/// https fetch 与 wss 握手共用 FetchOptions 的根证书（op_ws_create 从 OpState
/// 读取）——webpki-roots（Mozilla 根集）编译进二进制，零系统依赖。
pub fn ws_client_extensions() -> Vec<deno_core::Extension> {
    // 全图 rustls 只启用 aws_lc_rs（deno_tls、hyper-rustls 的 aws-lc-rs、reqwest 的
    // __rustls-aws-lc-rs），**无 ring**；但 provider 是进程级单例，显式钉死 aws_lc_rs
    // 才能消除「谁先建 ClientConfig 谁定 provider」的竞态（default 未装时 rustls 拒绝
    // 自动判定，ClientConfig::builder() 直接 panic）。oj-mail 插件同样装 aws_lc_rs，
    // 两处落在同一实例上。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let roots: Arc<dyn deno_tls::RootCertStoreProvider> =
        Arc::new(StaticRoots(deno_tls::create_default_root_cert_store()));
    vec![
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::init(
            Arc::new(deno_web::BlobStore::default()),
            None,  // maybe_location
            false, // enable_css_parser_features
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        deno_fetch::deno_fetch::init(deno_fetch::Options {
            root_cert_store_provider: Some(roots),
            ..Default::default()
        }),
        deno_net::deno_net::init(None, None),
        deno_websocket::deno_websocket::init(),
        // fs 轴（v0.1.53）：deno_fs 依赖 deno_io 的 read/write 原语（30_fs.js 经
        // loadExtScript 引 ext:deno_io/12_io.js），两扩展一并注册、顺序即依赖序。
        // FileSystemRc 在无 sync feature 时是 Rc（非 Arc），故 Rc::new(RealFs)。
        deno_io::deno_io::init(Some(deno_io::Stdio::default())),
        deno_fs::deno_fs::init(std::rc::Rc::new(deno_fs::RealFs)),
    ]
}

/// webpki-roots 根集的静态 provider（trait 只要求返回同一份引用）。
struct StaticRoots(deno_tls::rustls::RootCertStore);

impl deno_tls::RootCertStoreProvider for StaticRoots {
    fn get_or_try_init(&self) -> Result<&deno_tls::rustls::RootCertStore, deno_error::JsErrorBox> {
        Ok(&self.0)
    }
}

/// 任务驱动出口（spec §6）：Stopped = flag 置位后自然收场；Crashed = 顶层抛错/加载失败
/// （监督重启信号）；Killed = grace 到期强杀（terminate + event loop 兜底，SIGSEGV 纪律）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskExit {
    Stopped,
    Crashed(String),
    Killed,
}

impl Bridge {
    /// 任务常驻驱动（spec §6，评审 F3+F5）：任务文件**一律按 ESM side-module 加载**
    ///（不走 looks_cjs 启发式——无 import/export 的任务文件、release minify 后无空格的
    /// import 都会被它误伤），先 mod_evaluate 再驱动 event loop；不武装 handler 超时。
    /// flag 置位 → 任务循环检测退出（TLA 自然完成）；grace 到期仍不退 → 看门狗线程
    /// 跨线程 terminate_execution（紧 JS 循环会饿死本线程的协作式 select，只能靠它）
    /// → 再兜一轮 event loop → 丢弃 runtime（不归还池）。
    /// CJS 写法（require/module.exports）在 ESM 语义下自然 ReferenceError → Crashed。
    pub async fn run_task(
        &self,
        path: &std::path::Path,
        flag: Arc<AtomicBool>,
        grace: std::time::Duration,
    ) -> TaskExit {
        let src = match transpile::cached_transpile(path) {
            Ok(s) => s,
            Err(e) => return TaskExit::Crashed(format!("task compile: {e}")),
        };
        // 入口不经 module loader（评审 F3：TS 转译会吞掉空 `export {}`，loader 的
        // looks_cjs 会把入口误包成 CJS 绞杀 TLA）——直接以转译源 + 任务文件自身的
        // versioned URL 走 side-module：TLA 保真，相对导入以任务目录解析，共享库照常。
        let spec = match module_loader::versioned_specifier(path) {
            Ok(s) => s,
            Err(e) => return TaskExit::Crashed(format!("task spec: {e}")),
        };
        let mut rt = match self.checkout_reset(RequestInfo::default(), None).await {
            Ok(rt) => rt,
            Err(e) => return TaskExit::Crashed(e.to_string()),
        };
        let id = match rt
            .load_side_es_module_from_code(&spec, format!("{src}\n"))
            .await
        {
            Ok(id) => id,
            Err(e) => return TaskExit::Crashed(e.to_string()),
        };
        // 先武装看门狗再 mod_evaluate：TLA 紧循环（如 `while(true){ await poll() }`
        // 且 op 同步就绪）会把 mod_evaluate 的初始 microtask checkpoint 饿死到永不
        // 返回——宿主线程连 select 的协作分支都轮不到，停机 flag 只能由看门狗代盯，
        // grace 到点由它跨线程 terminate（SIGSEGV 纪律：terminate 后本线程兜一轮
        // event loop 再丢弃，绝不归还池）。
        let handle = rt.v8_isolate().thread_safe_handle();
        self.kill.arm_on_flag(handle, flag, grace);
        let eval = rt.mod_evaluate(id);
        // terminate 落在 isolate「未执行 JS」的窗口会被 V8 锁存，TLA promise 被
        // 悬空——eval 可能永不 settle。故除自然驱动外，还需盯 fired：一旦看门狗
        // 熔断即放弃等待，按 Killed 收场。
        let natural = tokio::select! {
            r = async {
                let looped = rt.run_event_loop(PollEventLoopOptions::default()).await;
                let evaled = eval.await;
                looped.and(evaled)
            } => Some(r),
            _ = async {
                while !self.kill.fired() {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            } => None,
        };
        let fired = self.kill.disarm();
        match natural {
            // 熔断后的兜底轮加超时：被终止的 isolate 不保证 event loop 还能返回；
            // 超时即放弃（runtime 反正要丢弃，绝不归还池——SIGSEGV 纪律）。
            _ if fired => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    rt.run_event_loop(PollEventLoopOptions::default()),
                )
                .await;
                TaskExit::Killed
            }
            Some(Err(e)) => {
                let _ = rt.run_event_loop(PollEventLoopOptions::default()).await;
                TaskExit::Crashed(e.to_string())
            }
            Some(Ok(())) => {
                let _ = rt.run_event_loop(PollEventLoopOptions::default()).await;
                TaskExit::Stopped
            }
            None => unreachable!("fired implies the fired-watch arm above"),
        }
    }
}

/// finish()：标记会话完成。
#[op2(fast)]
fn op_finish(state: &mut OpState) {
    state.borrow_mut::<ReqState>().done = true;
}

/// 模块 driver 的异常回传（perf F2）：bootstrap 的 `__ojRun` catch 后存入 ReqState，
/// run_side_driver 据此映射 RunError::Core（isolate 丢弃语义与旧 module 路径一致）。
#[op2(fast)]
fn op_driver_error(state: &mut OpState, #[string] msg: &str) {
    state.borrow_mut::<ReqState>().driver_error = Some(msg.to_string());
}

/// 响应捕获（json.ok/fail 写入，server 层读取后写回 HTTP 响应）。
///
/// 流式响应（v0.1.35）：若 handler 调用 `json.stream()`/`json.sse()`，`stream` 为 Some，
/// `server` 用 `axum::body::Body::from_stream` 写出，`body` 字段被忽略（流式路径绕过信封）。
/// 非流式：走 `body`（与历史行为一致，所有存量调用点无需改动）。
#[derive(Default, Debug)]
pub struct Capture {
    pub status: u16,
    /// 响应头（有序、可重名；Set-Cookie 依赖多值同存，json.header 追加写入）。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub stream: Option<tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>>,
    /// `json.raw` 产物（无统一信封；MCP/JSON-RPC 信封检测豁免依据，ReqState.raw 镜像）。
    pub raw: bool,
}

impl Capture {
    /// 按名取**最后一个**值（与旧 HashMap 覆盖写语义一致：同名后写覆盖先写的读取结果）。
    /// 大小写敏感（HTTP 头名语义本大小写不敏感，旧实现即敏感，保持不变）。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Bridge：持有 runtime 池、handler 仓库，并执行 handler 脚本。
/// （StableState 由 pool 构造期捕获，经 OpState 注入；此处不另持 Arc。）
pub struct Bridge {
    pool: runtime::RuntimePool,
    handlers: HandlerStore,
    /// 是否启用 inspector（透传至 runtime 工厂）。
    inspect: bool,
    /// 超时熔断（每 Bridge 一个看门狗线程）。
    kill: Arc<runtime::KillSwitch>,
}

impl Bridge {
    /// 构造 Bridge（依赖倒置：传入接口而非具体实现）。
    /// 传入的 db 注册为 dbs["default"]，等价于 DB("default")，无需额外配置。
    pub fn new(db: Arc<dyn DataAccessor>, kv: Arc<dyn KVStore>) -> Self {
        Self::with_opts(db, kv, SchemaRegistry::new(), false)
    }

    /// 含 schema 注册表与 inspector 开关的构造（单 db，注册为 "default"）。
    pub fn with_opts(
        db: Arc<dyn DataAccessor>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
    ) -> Self {
        Self::with_dbs(
            HashMap::from([("default".to_string(), db)]),
            kv,
            registry,
            inspect,
        )
    }

    /// 全量命名 DB 构造期注入（DBAccessors + default 回落）。
    /// 须在构造期给定全量：StableState 一经 runtime 池共享即不可变。
    pub fn with_dbs(
        dbs: HashMap<String, Arc<dyn DataAccessor>>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
    ) -> Self {
        Self::with_dbs_and_loader(dbs, kv, registry, inspect, None, Extras::default())
    }

    /// 全量命名 DB + 模块加载器构造（oj server 专用路径，T10/T11 装配消费）。
    /// extras：可选能力（blob/es）单一扩展点。
    pub fn with_dbs_and_loader(
        mut dbs: HashMap<String, Arc<dyn DataAccessor>>,
        kv: Arc<dyn KVStore>,
        registry: SchemaRegistry,
        inspect: bool,
        loader: Option<Arc<module_loader::LoaderShared>>,
        extras: Extras,
    ) -> Self {
        // 防御：无 "default" 键时取任一实例补位（JS 侧 db = DB("default")）。
        if !dbs.contains_key("default")
            && let Some(first) = dbs.values().next().cloned()
        {
            dbs.insert("default".to_string(), first);
        }
        // 跨请求事务会话：配置与注册表分开接（装配层可注入已存在的注册表，
        // 使热更前后同源——会话是进程级资源，不能因一次 reload 就重开表）。
        let db_session_cfg = extras
            .db_session
            .clone()
            .unwrap_or_else(|| Arc::new(db_policy::DbSessionSection::default()));
        let audit_section = extras.audit_section.clone().unwrap_or_default();
        let audit_store = extras
            .audit
            .clone()
            .unwrap_or_else(|| Arc::new(audit::AuditStore::new(&audit_section)));
        let tx_sessions = extras.tx_sessions.clone().unwrap_or_else(|| {
            Arc::new(db_session::TxSessionRegistry::new(
                (*db_session_cfg).clone(),
                audit_store.clone(),
            ))
        });
        let exec_sessions = extras.exec_sessions.clone().unwrap_or_default();
        let export_section = extras.export_section.clone().unwrap_or_default();
        let export_registry = extras
            .export_registry
            .clone()
            .unwrap_or_else(|| Arc::new(db_export::ExportRegistry::new(&export_section)));
        let import_section = extras.import_section.clone().unwrap_or_default();
        let import_registry = extras
            .import_registry
            .clone()
            .unwrap_or_else(|| Arc::new(db_import::ImportRegistry::new(&import_section)));
        let mgmt_section = extras
            .mgmt_section
            .clone()
            .unwrap_or_else(|| Arc::new(db_policy::MgmtSection::default()));
        let mgmt_axes = extras
            .mgmt_axes
            .clone()
            .unwrap_or_else(|| Arc::new(HashMap::new()));
        let mgmt = extras.mgmt.clone().unwrap_or_else(|| {
            Arc::new(mgmt::MgmtRegistry::new(
                mgmt_section.max_entries,
                mgmt_section.max_per_owner,
            ))
        });
        let secret_keys = extras.secret_keys.clone();
        let stable = Arc::new(StableState {
            kv,
            dbs,
            registry: Arc::new(registry),
            loader,
            module_index: extras.module_index,
            seq_ensured: std::sync::Mutex::new(std::collections::HashSet::new()),
            blobs: extras
                .blobs
                .unwrap_or_else(|| Arc::new(blob::BlobRegistry::new())),
            bus: extras.bus.unwrap_or_else(|| Arc::new(bus::Bus::new())),
            es: extras.es,
            plugins: extras.plugins,
            modules: extras.modules,
            ownership_deny: extras.ownership_deny,
            sql_guard: extras.sql_guard,
            allow_as_tenant: extras.allow_as_tenant,
            db_override: extras.db_override,
            query_limits: extras.query_limits,
            sql_trace: extras.sql_trace,
            boot: extras.boot,
            input_contracts: extras.input_contracts,
            jwt: extras.jwt,
            oidc: extras.oidc,
            sso: extras.sso,
            kafkas: extras
                .kafkas
                .unwrap_or_else(|| Arc::new(NamedRegistry::new())),
            rabbits: extras
                .rabbits
                .unwrap_or_else(|| Arc::new(NamedRegistry::new())),
            tasks_flag: extras.tasks_flag,
            sql_memo: Mutex::new(HashMap::new()),
            mail: extras.mail,
            ldap: extras.ldap,
            db_access: extras.db_access.unwrap_or_default(),
            db_client: extras.db_client.unwrap_or_default(),
            db_session: db_session_cfg,
            client_dbs: extras
                .client_dbs
                .unwrap_or_else(|| Arc::new(client_db::ClientDbRegistry::new(16))),
            tx_sessions,
            audit_section,
            audit: audit_store,
            exec_sessions,
            export_section,
            export_registry,
            import_section,
            import_registry,
            mgmt_section,
            mgmt_axes,
            mgmt,
            secret_keys,
            db_access_rt: extras
                .db_access_rt
                .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new()))),
            db_backend: extras
                .db_backend
                .unwrap_or_else(|| Arc::new(db_backend::DbBackendRegistry::builtin())),
            config_dir: extras
                .config_dir
                .unwrap_or_else(|| Arc::new(std::path::PathBuf::from("."))),
            vars: extras.vars,
            js_heap_limit: extras.js_heap_limit,
            fs: extras.fs,
            platform: extras.platform,
            exec: extras.exec,
            reload: extras.reload,
            generic_axes: extras.generic_axes.unwrap_or_default(),
            oj_info: extras
                .oj_info
                .unwrap_or_else(|| Arc::new(serde_json::json!({}))),
        });
        // mail 结果回调（HostContext.deliver，无状态 extern "C"）经进程级弱引用路由到本后端：
        // 存结果 + 本地扇出。未配置 mail 时不挂（上送被明确丢弃并告警）。
        if let Some(m) = &stable.mail {
            mail::install_mail_deliver(m);
        }
        // KillSwitch 先于池构造：池在 boot 期要 arm 它（TLA 死循环的唯一兜底）。
        let kill = runtime::KillSwitch::spawn();
        let pool = runtime::RuntimePool::new(stable.clone(), inspect, kill.clone());
        Self {
            pool,
            handlers: HandlerStore::from_env(),
            inspect,
            kill,
        }
    }

    /// 替换 handler 仓库（生产用嵌入 map，开发用 FS 热重载）。
    pub fn set_handlers(&mut self, handlers: HandlerStore) {
        self.handlers = handlers;
    }

    /// StableState 句柄（测试内省用；生产装配侧持 Backend::stable 单源）。
    #[cfg(test)]
    pub(crate) fn stable_for_test(&self) -> Arc<StableState> {
        self.pool.stable().clone()
    }

    /// 取 inspector 句柄（若以 inspect=true 构造且已有 checkout 的 runtime 持有时可用）。
    /// 此处提供便捷构造 inspector server 的能力：需先 checkout 一个 runtime 并取其 inspector。
    pub fn inspect(&self) -> bool {
        self.inspect
    }

    /// 执行 handler 源码并驱动 event loop 至所有 Promise 落定。
    /// 从池借出 runtime，重置 per-request 状态，执行，归还。
    pub async fn run(&self, source: &str) -> Result<Capture, CoreError> {
        self.run_with(source, RequestInfo::default()).await
    }

    /// 带请求上下文执行。
    pub async fn run_with(&self, source: &str, req: RequestInfo) -> Result<Capture, CoreError> {
        // boot 失败（含超时）→ CoreError：run_with 无超时语义，不引入 RunError。
        let mut rt = match self.checkout_reset(req, None).await {
            Ok(rt) => rt,
            Err(e) => return Err(CoreError::from(std::io::Error::other(e.to_string()))),
        };
        let result = runtime::run_to_completion(&mut rt, "handler.js", source.to_string()).await;
        // 仅成功执行（已轮询 event loop）的 runtime 才归还；失败的可能 isolate 损坏，直接丢弃。
        match result {
            Ok(()) => {
                let cap = Self::read_capture(&rt);
                Self::finalize_tx(&rt).await;
                self.pool.checkin(rt);
                Ok(cap)
            }
            Err(e) => Err(e),
        }
    }

    // ----- 执行管道复用辅助 -----

    /// 借出 runtime 并重置 per-request 状态（不武装看门狗）。
    /// boot 失败 → `RunError`（绝不 panic：调用方可能在 actor 请求线程上，panic 会
    /// 永久杀死该 worker）。
    async fn checkout_reset(
        &self,
        req: RequestInfo,
        module: Option<&ModuleCtx>,
    ) -> Result<JsRuntime, RunError> {
        let rt = self.pool.checkout().await?;
        let stable = self.pool.stable().clone();
        {
            let op_state = runtime::op_state(&rt);
            let mut g = op_state.borrow_mut();
            {
                let rs = g.borrow_mut::<ReqState>();
                rs.reset(req);
                // http.module {name, version} 与 Rust 侧授信面（归属守卫/bound_db/
                // 平台特权换装）同源于 ModuleCtx——唯一写点，两者恒同生同灭。
                rs.module = module.map(|c| c.name.clone());
                rs.module_version = module.map(|c| c.version.clone());
                debug_assert_eq!(rs.module.is_some(), rs.module_version.is_some());
            }
            // fs 轴第二道防线按模块特权级换装（ojm 批次，设计稿 §1.6）：特权模块
            // （ReqState.module ∈ platform.modules）的 read/write 授权面 =
            // fs.root ∪ trusted_paths；非特权 = 既有 fs.root 单集合（行为不变）。
            // 门面裁决在 fs::op_fs_resolve（特权判定 + trusted 集合放行），此处保证
            // deno op 自身的权限检查与之同口径——trusted 路径的可及性缺这步会被
            // deno_permissions 拒掉。池中归还的 runtime 仍带上一请求的容器；所有
            // 借出路径（HTTP/run_with/任务/WS/内省，含 inspector 挂载前借出）都先经
            // 本函数重置，特权容器不跨请求生效。
            g.put(fs::container_for_module(
                stable.fs.as_deref(),
                stable.platform.as_deref(),
                module.map(|c| c.name.as_str()),
            ));
            // 资源回收门禁（评审 CRITICAL，v0.1.78）：借出重置时兜底清空 ResourceTable，
            // 防御 runtime 被异常路径丢弃未走 checkin 的情形；覆盖 ExecHandleResource::close
            // 同步 kill_tree，杜绝跨请求/租户泄漏子进程/句柄 + next_rid 回绕 panic。
            {
                let rids: Vec<deno_core::ResourceId> =
                    g.resource_table.names().map(|(rid, _)| rid).collect();
                for rid in rids {
                    if let Ok(resource) = g.resource_table.take_any(rid) {
                        resource.close();
                    }
                }
            }
        }
        Ok(rt)
    }

    /// 借出 runtime、重置状态并武装看门狗（KillSwitch）。
    /// 用 v8::IsolateHandle（Send+Sync 跨线程句柄）而非裸指针：OwnedIsolate 包装地址 ≠ 真实
    /// isolate 指针，手转裸指针再跨线程 terminate 会解引用垃圾地址（SIGSEGV）。
    async fn checkout_armed(
        &self,
        req: RequestInfo,
        timeout: std::time::Duration,
        module: Option<&ModuleCtx>,
    ) -> Result<JsRuntime, RunError> {
        let mut rt = self.checkout_reset(req, module).await?;
        let handle = rt.v8_isolate().thread_safe_handle();
        self.kill.arm(handle, timeout);
        Ok(rt)
    }

    /// 启动期预热：借出一个 runtime（从而跑完 ext_boot）并归还，失败返回 `Err`。
    ///
    /// 用途：把 boot 的错误前移到装配期。**必须显式调用** —— 运行期首次 checkout 才 boot，
    /// 若只依赖启动期内省的间接失败，线程 panic 会被 `routes::bridge_introspector` 的
    /// `join()` 吞成路由 failure，而路由 failure 在装配层只 warn 不致命（路由全空、服务照常监听）。
    pub async fn prewarm(&self) -> Result<(), RunError> {
        let rt = self.checkout_reset(RequestInfo::default(), None).await?;
        self.pool.checkin(rt);
        Ok(())
    }

    /// 取 inspector 句柄（`inspect=true` 构造时有效；否则 None）：借出一个 runtime（已完成
    /// boot）取句柄后立即归还。
    ///
    /// 借出**必须**走 `checkout_reset`（非裸 `pool.checkout`）：池中归还的 runtime 仍带
    /// 上一请求的 OpState，若那是特权请求，残留的特权 PermissionsContainer 会被挂上的
    /// 调试器求值触达。checkout_reset 以 `module=None` 重置，容器即非特权基线形态。
    pub async fn inspector(&self) -> Option<Rc<deno_core::JsRuntimeInspector>> {
        if !self.inspect {
            return None;
        }
        let rt = self
            .checkout_reset(RequestInfo::default(), None)
            .await
            .ok()?;
        let insp = rt.inspector();
        self.pool.checkin(rt);
        Some(insp)
    }

    /// 请求收尾：未完结的活跃事务保底回滚（checkin 前调用；错误吞掉只告警，
    /// sqlx Transaction drop 亦会释放连接）。JS 侧正常路径已显式 commit/rollback。
    async fn finalize_tx(rt: &JsRuntime) {
        let t = {
            let op_state = runtime::op_state(rt);
            let mut g = op_state.borrow_mut();
            g.borrow_mut::<ReqState>().tx.take()
        };
        if let Some(t) = t {
            match t.session.lock().await.rollback().await {
                Ok(()) => eprintln!(
                    "warn: open transaction on db '{}' rolled back at request end",
                    t.db
                ),
                Err(e) => eprintln!(
                    "warn: open tx on db '{}' rollback failed at request end: {e}",
                    t.db
                ),
            }
        }
    }

    /// 读取当前 runtime 的 per-request 响应捕获。
    fn read_capture(rt: &JsRuntime) -> Capture {
        let op_state = runtime::op_state(rt);
        // 与 baseline 一致走「共享借用」：帧池 worker（frame_pool.rs）会持有 OpState 的共享
        // 借用跨调用进入此处，故此处不得用 borrow_mut；流式字段用 ReqState 内的 RefCell 取走。
        let g = op_state.borrow();
        let rs = g.borrow::<ReqState>();
        // 流式：移出接收端；丢弃写入端（或触发心跳停止）以在 handler 返回后关闭流。
        // 若 handler 未显式 op_stream_end，此处丢弃 tx 同样关流；心跳任务经 Notify 退出。
        let (tx, rx, stop) = (
            rs.stream_tx.borrow_mut().take(),
            rs.stream_rx.borrow_mut().take(),
            rs.stream_heartbeat_stop.borrow_mut().take(),
        );
        if let Some(s) = stop {
            s.notify_one();
        }
        drop(tx);
        Capture {
            status: rs.status,
            headers: rs.headers.clone(),
            body: rs.response.clone().unwrap_or_default(),
            stream: rx,
            raw: rs.raw,
        }
    }

    /// 按名执行已加载的 handler（热重载走 HandlerStore）。
    pub async fn run_named(&self, name: &str) -> Result<Capture, CoreError> {
        let src = self.handlers.get(name).ok_or_else(|| {
            CoreError::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("handler '{name}' not found"),
            ))
        })?;
        self.run(&src).await
    }

    /// 带超时熔断的执行：到期经 V8 terminate_execution 终止（跨线程安全），
    /// 该 runtime 不归还池，返回 `RunError::Timeout`（408 语义）。
    pub async fn run_with_timeout(
        &self,
        source: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<Capture, RunError> {
        self.run_ws(source, req, timeout).await.map(|o| o.capture)
    }

    /// WS 帧执行：同 run_with_timeout，额外带出 ws.send 收集与 ws.close 置位。
    pub async fn run_ws(
        &self,
        source: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<WsOutcome, RunError> {
        let mut rt = self.checkout_armed(req, timeout, None).await?;
        let result = runtime::run_to_completion(&mut rt, "handler.js", source.to_string()).await;
        if self.kill.disarm() {
            // runtime 已被 terminate，不可复用，直接丢弃（不 checkin）。
            return Err(RunError::Timeout);
        }
        // PR-4：堆限额触发（isolate 已被 terminate；checkin 会丢弃，不回池）。
        if runtime::heap_guard_fired(&rt) {
            return Err(RunError::Oom {
                limit: self.pool.stable().js_heap_limit.unwrap_or(0),
            });
        }
        result.map_err(RunError::Core)?;
        let (sends, close) = {
            let op_state = runtime::op_state(&rt);
            let g = op_state.borrow();
            let rs = g.borrow::<ReqState>();
            (rs.ws_sends.clone(), rs.ws_close)
        };
        let capture = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        self.pool.checkin(rt);
        Ok(WsOutcome {
            capture,
            sends,
            close,
        })
    }

    /// WS 会话建立：借出 runtime（不还池，交给 `WsSession` 持有）、注入连接期
    /// driver——import ws.ts（default 导出钩子对象）→ 装 `__ws_hooks`/`__ws_call`/
    /// `sess` 代理 → 经 json.ok 回报钩子清单。四钩子全缺 → Err（一刀切契约：失败要显式）。
    /// 每帧调用契约见 frame_pool 的 `ws_event`；`__ws_call` 内建 try/catch → error()
    /// 兜底，`finally` 把 `__sess` 快照经 `Deno.core.ops.op_ws_sess_set` 交还
    /// （sess.state 外置；driver 是 file: side-module，deno_core 0.411 禁止其
    /// `import "ext:core/ops"`，故走 Deno.core.ops 直呼）。
    pub async fn ws_connect(&self, ws_file: &std::path::Path) -> Result<WsSession, RunError> {
        let spec = module_loader::versioned_specifier(ws_file)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let code = format!(
            "const h = (await import(\"{spec}\")).default ?? {{}};\n\
             const fns = {{}};\n\
             for (const k of [\"connection\", \"message\", \"close\", \"error\"])\n\
               fns[k] = typeof h[k] === \"function\" ? h[k] : null;\n\
             globalThis.__ws_hooks = fns;\n\
             globalThis.__sess = null;\n\
             globalThis.sess = {{\n\
               get id() {{ return globalThis.__ws_conn; }},\n\
               get state() {{ return globalThis.__sess ?? (globalThis.__sess = {{}}); }},\n\
               set state(v) {{ globalThis.__sess = v; }},\n\
             }};\n\
             globalThis.__ws_call = async (conn, ev) => {{\n\
               const fn = globalThis.__ws_hooks[ev];\n\
               if (!fn) return;\n\
               try {{ await fn(); }} catch (e) {{\n\
                 const onErr = globalThis.__ws_hooks.error;\n\
                 if (onErr && ev !== \"error\") await onErr(e); else throw e;\n\
               }} finally {{\n\
                 try {{ Deno.core.ops.op_ws_sess_set(globalThis.__sess === undefined ? null : globalThis.__sess); }} catch {{}}\n\
               }}\n\
             }};\n\
             json.ok({{ connection: !!fns.connection, message: !!fns.message,\n\
                        close: !!fns.close, error: !!fns.error }});\n"
        );
        let driver_spec = deno_core::ModuleSpecifier::parse("file:///oj/ws_connect.js")
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e.to_string()))))?;
        let mut rt = self
            .checkout_armed(RequestInfo::default(), WS_CONNECT_TIMEOUT, None)
            .await?;
        // 顺序同 run_side_driver（0.410 签名）：mod_evaluate 先启动、驱动 event loop、
        // 再 await 求值 future 取 TLA/import 错误。
        let result: Result<(), CoreError> = async {
            let id = rt.load_side_es_module_from_code(&driver_spec, code).await?;
            let eval = rt.mod_evaluate(id);
            rt.run_event_loop(deno_core::PollEventLoopOptions::default())
                .await?;
            eval.await?;
            Ok(())
        }
        .await;
        if self.kill.disarm() {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Timeout); // runtime 丢弃（会话建立失败，无从交接）
        }
        if let Err(e) = result {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Core(e)); // 文件缺失/转译/求值错误原样带出
        }
        let cap = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        let d = &v["data"];
        let has_hook = ["connection", "message", "close", "error"]
            .iter()
            .any(|k| d[k].as_bool() == Some(true));
        if !has_hook {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Core(CoreError::from(std::io::Error::other(
                format!(
                    "ws handler '{}' must export at least one of \
                     connection/message/close/error (via default export)",
                    ws_file.display()
                ),
            ))));
        }
        Ok(WsSession {
            rt,
            kill: Arc::clone(&self.kill),
        })
    }

    /// 池化任务会话建立（PRD v2 §6.3）：借出 runtime（不还池，交给 TaskPool Worker 持有），
    /// driver import 任务模块 → 装配 `__task_hooks`/`__task_call` → **执行 setup** →
    /// 经 json.ok 回报三钩子存在性。钩子为**命名导出**（setup/loop_body/teardown），
    /// 模块作用域即任务 ctx（FR-EP-003/004）。管道与 ws_connect 同构（装配期一次性）。
    pub async fn task_connect(&self, task_file: &std::path::Path) -> Result<TaskSession, RunError> {
        let spec = module_loader::versioned_specifier(task_file)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let code = format!(
            "const m = await import(\"{spec}\");\n\
             const fns = {{}};\n\
             for (const k of [\"setup\", \"loop_body\", \"teardown\"])\n\
               fns[k] = typeof m[k] === \"function\" ? m[k] : null;\n\
             globalThis.__task_hooks = fns;\n\
             globalThis.__task_call = async (name) => {{\n\
               const fn = globalThis.__task_hooks[name];\n\
               if (!fn) throw new Error(\"task hook '\" + name + \"' not exported\");\n\
               await fn();\n\
             }};\n\
             if (fns.setup) await fns.setup();\n\
             json.ok({{ setup: !!fns.setup, loop_body: !!fns.loop_body, teardown: !!fns.teardown }});\n"
        );
        let driver_spec = deno_core::ModuleSpecifier::parse("file:///oj/task_connect.js")
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e.to_string()))))?;
        let mut rt = self
            .checkout_armed(RequestInfo::default(), WS_CONNECT_TIMEOUT, None)
            .await?;
        let result: Result<(), CoreError> = async {
            let id = rt.load_side_es_module_from_code(&driver_spec, code).await?;
            let eval = rt.mod_evaluate(id);
            rt.run_event_loop(deno_core::PollEventLoopOptions::default())
                .await?;
            eval.await?;
            Ok(())
        }
        .await;
        if self.kill.disarm() {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Timeout);
        }
        if let Err(e) = result {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Core(e));
        }
        let cap = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        if v["data"]["loop_body"].as_bool() != Some(true) {
            let _ = rt
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await;
            return Err(RunError::Core(CoreError::from(std::io::Error::other(
                format!(
                    "task '{}' must export loop_body (named export) to run in TaskPool",
                    task_file.display()
                ),
            ))));
        }
        Ok(TaskSession {
            rt,
            kill: Arc::clone(&self.kill),
        })
    }

    /// 判别任务文件模式（PRD v2 FR-LT-004）：存在 `loop_body` 命名导出 → 池化模式；
    /// 否则 → 存量 TLA 监督模式。装配期一次性探测，复用 introspect 管道。
    pub async fn probe_task_loop(&self, task_file: &std::path::Path) -> Result<bool, RunError> {
        let spec = module_loader::versioned_specifier(task_file)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let body = format!(
            "const m = await import(\"{spec}\");\n\
             json.ok({{ loop: typeof m.loop_body === \"function\" }});\n"
        );
        let code = driver_script(&body);
        let cap = self
            .run_side_driver(RequestInfo::default(), code, INTROSPECT_TIMEOUT, None)
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        Ok(v["data"]["loop"].as_bool() == Some(true))
    }

    /// ESM 模式执行：TLA driver 模块 import api 模块，沿 **handler 链**逐段计算式
    /// 取值并调用末段函数（v0.1.64 嵌套 handler 组；顶层动词链即 `"get"`，行为不变）。
    /// KillSwitch/ReqState 复用 run_ws 的熔断与捕获路径；被熔断的 runtime 同样不归还池。
    pub async fn run_module(
        &self,
        api_path: &std::path::Path,
        handler: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<Capture, RunError> {
        self.run_module_validated(api_path, handler, req, timeout, true)
            .await
    }

    /// 与 `run_module` 相同，但可显式关闭入参契约校验。
    ///
    /// **仅**内部合成派发（`serve::dispatch_meta_handler`）用 `validate=false`：
    /// 那个请求是框架自己造的（`query={"path":…}`、空 body），不是外部输入；
    /// 让它的契约校验失败只会静默降级页面 meta，属于误伤。
    /// 外部请求**一律** `validate=true`（缺省即如此）。
    pub async fn run_module_validated(
        &self,
        api_path: &std::path::Path,
        handler: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
        validate: bool,
    ) -> Result<Capture, RunError> {
        let spec = self
            .driver_spec(api_path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        // TLA driver：import 命中 V8 模块缓存（?v= 不变时零转译零重编译）；
        // 段缺失 → 404，末段非函数 / default 缺失 → 405（信封映射同码 HTTP）。
        // 链/段/msg 一律 JSON 编码嵌入（合法 JS 字面量，杜绝引号/换行注入）。
        // 经常驻 `__ojRun`（bootstrap，strict direct eval）执行——不再每请求装载
        // side-module driver（perf F2：省编译/装载 + module map 无界增长）。
        let handler_lit = serde_json::to_string(handler).unwrap_or_else(|_| "\"\"".into());
        let msg_lit = serde_json::to_string(&format!(
            "handler '{handler}' not exported by {}",
            api_path.display()
        ))
        .unwrap_or_else(|_| "\"handler not exported\"".into());
        let body = format!(
            "const m = await import(\"{spec}\");\n\
             const SEGS = {handler_lit}.split(\".\");\n\
             const root = m.default;\n\
             if (!root || (typeof root !== \"object\" && typeof root !== \"function\"))\n\
               json.fail(405, {msg_lit});\n\
             else {{\n\
               let cur = root;\n\
               for (let i = 0; i < SEGS.length; i++) {{\n\
                 const s = SEGS[i];\n\
                 if (s === \"\" || s.indexOf(\"/\") >= 0 ||\n\
                     s === \"__proto__\" || s === \"constructor\" || s === \"prototype\")\n\
                   return json.fail(404, \"bad handler segment\");\n\
                 if ((typeof cur !== \"object\" && typeof cur !== \"function\") ||\n\
                     !Object.prototype.hasOwnProperty.call(cur, s))\n\
                   return json.fail(404, \"no handler\");\n\
                 cur = cur[s];\n\
               }}\n\
               if (typeof cur !== \"function\") json.fail(405, {msg_lit});\n\
               else await cur();\n\
             }}\n"
        );
        let code = driver_script(&body);
        // §5.3 执行上下文：api_path 祖先目录命中模块目录 → 本请求归该模块
        // （归属守卫 / bound_db 重定向依据；Map 键 = 模块目录绝对路径，装配期注入）。
        // 命中即取整个 ModuleCtx——name 进 Rust 授信面，name+version 经
        // op_http_info 投影为 JS 只读 http.module。
        let stable = self.pool.stable();
        // 入参契约校验（v0.1.44）：**在 V8 之前**拦住不合规输入 → 400。
        // 与「handler 未导出 → 405」同处一个执行切面：两者都是「进 JS 之前」的契约失败。
        // 契约键 = (api 文件, handler 链) —— 嵌套组各 verb 契约互不串门。
        // 注册表为装配后填充的共享 Arc：读锁仅覆盖校验本身（内部无 await，亦不跨 await 持有）。
        let violation = if validate {
            if let Some(reg) = &stable.input_contracts {
                let g = reg.read().unwrap_or_else(|p| p.into_inner());
                g.check_request(api_path, handler, &req).err()
            } else {
                None
            }
        } else {
            None
        };
        if let Some(msg) = violation {
            let (body, status) = envelope::fail(400, &msg, &serde_json::Value::Null);
            let headers = vec![("content-type".to_string(), "application/json".to_string())];
            return Ok(Capture {
                status,
                headers,
                body,
                stream: None,
                raw: false,
            });
        }
        let mut module: Option<&ModuleCtx> = None;
        let mut anc = api_path.parent();
        while let Some(d) = anc {
            if let Some(ctx) = stable.modules.get(&d.to_string_lossy().into_owned()) {
                module = Some(ctx);
                break;
            }
            anc = d.parent();
        }
        self.run_side_driver(req, code, timeout, module).await
    }

    /// 启动期内省：import api 模块，**递归**遍历 default 导出的键（v0.1.64 嵌套 handler 组），
    /// 经 json.ok 信封回传**扁平 leaf 数组** `[{ chain, route, schema }, ...]`：
    /// - `chain`：点分 handler 链（`"cccc.get"`；顶层动词即 `"get"`）；
    /// - `route`：组前缀解析后的**有效路由**（Rust 词汇：`null` = 目录镜像 /
    ///   `"/x"` = 根级 / `"seg/.."` = 相对目录镜像）；
    /// - `schema`：handler 函数上的 `.schema` 元数据（原样透传，未校验）。
    ///
    /// 判别红线（§二.A）：**先按键是否 verb 分派，再以 `typeof` 校验值**——verb 键必须是
    /// 函数（否则报错）；非 verb 键必须是普通对象（函数/数组/基元一律报错，消灭此前的
    /// 静默丢弃）。组级 `.route` 作整组默认前缀，动词自身 `.route` 优先覆盖。
    /// 防护：`seen` 路径 WeakSet（进/出删，拦自引用环且不误伤共享组对象的 DAG）+
    /// 深度上限 4（链长 >4 即失败）。**route 与 schema 同源一趟取**，否则「路由表里有、
    /// 契约表没有」会退化成声明不生效。
    /// 复用 run_module 的 driver/KillSwitch/checkin 管道（其注释同样适用）。
    pub async fn introspect_module(
        &self,
        api_path: &std::path::Path,
    ) -> Result<serde_json::Value, RunError> {
        let spec = self
            .driver_spec(api_path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let body = format!(
            "const m = await import(\"{spec}\");\n\
             const VERBS = new Set([\"get\",\"post\",\"put\",\"del\",\"patch\",\"head\",\"options\"]);\n\
             const out = [];\n\
             const seen = new WeakSet();\n\
             const resolve = (own, rp) =>\n\
               own === null ? rp :\n\
               own.charAt(0) === \"/\" ? own :\n\
               rp ? rp + \"/\" + own : own;\n\
             // 组级 .schema 与组级 .route 同等地位：组 .schema 作整组默认契约，动词自身\n\
             // .schema 在其上合并。入参三通道（params/query/body）与 response 通道\n\
             // 合并规则不同，见下。v0.1.72：出参另可与 .schema **平级**声明 .response，\n\
             // 由 walk 的 gresp 链继承，最终按 动词.response > 组.response > 动词\n\
             // .schema.response > 组.schema.response 写回 schema.response 透传。\n\
             const mergeSchema = (base, over) => {{\n\
               if (over === undefined || over === null) return base || null;\n\
               if (base === undefined || base === null) return over;\n\
               if (typeof base !== \"object\" || typeof over !== \"object\") return over;\n\
               const out = {{}};\n\
               // 入参三通道：通道级合并——required 取并集、properties 动词逐键覆盖\n\
               // （缺省用组值），其余通道级关键字动词优先。空并集 / 空 properties\n\
               // **不写入**：凭空注入空的 required / properties 会给原本没声明\n\
               // 这些键的通道塞进空值，进 openapi 就是噪声。\n\
               for (const ch of [\"params\", \"query\", \"body\"]) {{\n\
                 const b = base[ch], o = over[ch];\n\
                 if (b && o) {{\n\
                   const merged = Object.assign({{}}, b, o);\n\
                   const br = Array.isArray(b.required) ? b.required : [];\n\
                   const or = Array.isArray(o.required) ? o.required : [];\n\
                   const req = Array.from(new Set(br.concat(or)));\n\
                   if (req.length) merged.required = req; else delete merged.required;\n\
                   const props = Object.assign({{}}, b.properties || {{}}, o.properties || {{}});\n\
                   if (Object.keys(props).length) merged.properties = props; else delete merged.properties;\n\
                   out[ch] = merged;\n\
                 }} else if (b) {{ out[ch] = b; }}\n\
                 else if (o) {{ out[ch] = o; }}\n\
               }}\n\
               // response：动词级**整体覆盖**，无则继承组级。不并入参那套\n\
               // （required 并集 / properties 逐键覆盖）——响应是动词各不相同的，\n\
               // 组级声明的必填字段被某动词继承后，文档与下游 client 会声称返回\n\
               // 并不存在的必填项，那是假真源。\n\
               // 判定与平级 .response 链一致（v0.1.74）：null 视为未声明；声明了但非法
               // （如布尔 false，装配期 audit fail-fast）不静默跳过优先级。
               if (over.response !== undefined && over.response !== null) out.response = over.response;\n\
               else if (base.response !== undefined && base.response !== null) out.response = base.response;\n\
               for (const k of Object.keys(over)) if (!(k in out)) out[k] = over[k];\n\
               for (const k of Object.keys(base)) if (!(k in out)) out[k] = base[k];\n\
               return out;\n\
             }};\n\
             const walk = (obj, prefix, rp, gschema, gresp, name, depth) => {{\n\
               const at = prefix.join(\".\") || \"<root>\";\n\
               if (depth > 4) throw new Error(\"handler group nested deeper than 4 levels at \" + at);\n\
               if (seen.has(obj)) throw new Error(\"circular handler group at \" + at);\n\
               seen.add(obj);\n\
               const lraw = obj.route;\n\
               const layer = (typeof lraw === \"string\" && lraw !== \"\")\n\
                 ? resolve(String(lraw), rp)\n\
                 : (rp ? rp + \"/\" + name : name);\n\
               const thisSchema = mergeSchema(gschema, obj.schema);\n\
               // 组级 .response 平级声明（v0.1.72）：组值沿嵌套层层继承，与 gschema 同机制。\n\
               const thisResp = obj.response != null ? obj.response : gresp;\n\
               for (const k of Object.keys(obj)) {{\n\
                 if (k === \"route\" && typeof obj[k] === \"string\") continue;\n\
                 if (k === \"schema\") continue;\n\
                 if (k === \"response\") continue;\n\
                 const v = obj[k];\n\
                 if (VERBS.has(k)) {{\n\
                   if (typeof v !== \"function\")\n\
                     throw new Error(\"verb key '\" + k + \"' at \" + at + \" is \" + (v === null ? \"null\" : typeof v) + \", expected handler function\");\n\
                   const raw = v.route;\n\
                   const own = (raw === undefined || raw === null || raw === \"\") ? null : String(raw);\n\
                   const mschema = mergeSchema(thisSchema, v.schema);\n\
                   // 出参优先级：动词 .response > 继承的组 .response > 合并后的 .schema\n\
                   // .response（动词 > 组，mergeSchema 已保证）。浅拷贝写回——mschema 可\n\
                   // 能 IS thisSchema（v.schema 缺省时共享引用），直接改会污染同组兄弟动词。\n\
                   const resp = v.response != null ? v.response : thisResp;\n\
                   const vschema = resp !== undefined ? Object.assign({{}}, mschema, {{ response: resp }}) : mschema;\n\
                   out.push({{\n\
                     chain: prefix.concat(k).join(\".\"),\n\
                     route: resolve(own, layer) || null,\n\
                     schema: vschema,\n\
                   }});\n\
                 }} else {{\n\
                   if (v === null || typeof v !== \"object\" || Array.isArray(v))\n\
                     throw new Error(\"non-verb key '\" + k + \"' at \" + at + \" is \" + (Array.isArray(v) ? \"array\" : (v === null ? \"null\" : typeof v)) + \", expected nested group object\");\n\
                   walk(v, prefix.concat(k), layer, thisSchema, thisResp, k, depth + 1);\n\
                 }}\n\
               }}\n\
               seen.delete(obj);\n\
             }};\n\
             const d = m.default;\n\
             if (d !== undefined && d !== null) walk(d, [], \"\", null, undefined, \"\", 1);\n\
             json.ok(out);\n"
        );
        let code = driver_script(&body);
        let cap = self
            .run_side_driver(RequestInfo::default(), code, INTROSPECT_TIMEOUT, None)
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        Ok(v["data"].clone())
    }

    /// 读模块 default 导出（release 直载 routes.js：一次 import，不逐模块内省）。
    pub async fn read_module_default(
        &self,
        path: &std::path::Path,
    ) -> Result<serde_json::Value, RunError> {
        let spec = module_loader::versioned_specifier(path)
            .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
        let body = format!(
            "const m = await import(\"{spec}\");\n\
             json.ok(m.default === undefined ? null : m.default);\n"
        );
        let code = driver_script(&body);
        let cap = self
            .run_side_driver(RequestInfo::default(), code, INTROSPECT_TIMEOUT, None)
            .await?;
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap_or_default();
        Ok(v["data"].clone())
    }

    /// 请求路径的模块 spec（perf F3）：dev 逐请求 mtime 版本化（热重载）；
    /// release（ts=false，含无 loader）走冻结缓存——首请求一次 stat/canonicalize，
    /// 之后零 syscall。
    fn driver_spec(&self, path: &std::path::Path) -> Result<ModuleSpecifier, String> {
        let ts = self
            .pool
            .stable()
            .loader
            .as_ref()
            .map(|l| l.ts)
            .unwrap_or(false);
        module_loader::driver_spec(path, ts)
    }

    /// 模块 driver 执行（perf F2 重构）：借出 runtime、武装看门狗，经常驻
    /// `__ojRun`（bootstrap，strict direct eval）执行 `code`（已 import 目标模块并
    /// 触发对应 handler/内省），返回响应捕获。不再每请求装载 side-module driver
    /// （省一次 module 编译/实例化 + isolate module map 不再随请求数无界增长）。
    /// run_module / introspect_module / read_module_default / probe_task_loop 共用。
    /// `code` 必须是 `driver_script(&body)` 的产物——裸 body 的顶层 await 在
    /// execute_script（classic script）里是语法错（曾漏包 probe 一处，tasks 探测
    /// 全体误落 TLA 桶，dup 校验失效）。
    /// 超时 → RunError::Timeout（runtime 不归还）；driver 异常 → RunError::Core（同旧）。
    async fn run_side_driver(
        &self,
        req: RequestInfo,
        code: String,
        timeout: std::time::Duration,
        module: Option<&ModuleCtx>,
    ) -> Result<Capture, RunError> {
        let mut rt = self.checkout_armed(req, timeout, module).await?;
        // `__ojRun` catch 后经 op_driver_error 回传；promise 恒 resolve，event loop
        // 排空即完成——错误通道走 ReqState.driver_error（不再有 mod_evaluate future）。
        let result: Result<(), CoreError> = async {
            rt.execute_script("<oj-driver>", code)?;
            rt.run_event_loop(deno_core::PollEventLoopOptions::default())
                .await?;
            Ok(())
        }
        .await;
        // 超时熔断：看门狗已 terminate isolate，runtime 不可复用，直接丢弃（不 checkin）。
        if self.kill.disarm() {
            return Err(RunError::Timeout);
        }
        // PR-4：堆限额触发（isolate 已被 terminate；checkin 会丢弃，不回池）。
        if runtime::heap_guard_fired(&rt) {
            return Err(RunError::Oom {
                limit: self.pool.stable().js_heap_limit.unwrap_or(0),
            });
        }
        // driver 异常（原 mod_evaluate 错误通道）：runtime 不回池（与旧错误路径一致）。
        let driver_err = {
            let op_state = runtime::op_state(&rt);
            let g = op_state.borrow();
            g.borrow::<ReqState>().driver_error.clone()
        };
        if let Some(msg) = driver_err {
            return Err(RunError::Core(CoreError::from(std::io::Error::other(msg))));
        }
        result.map_err(RunError::Core)?;
        let capture = Self::read_capture(&rt);
        Self::finalize_tx(&rt).await;
        self.pool.checkin(rt);
        Ok(capture)
    }
}

/// driver 执行脚本（perf F2）：把框架生成的 body 以 JSON 字面量塞进
/// `globalThis.__ojBody`，调常驻 `__ojRun`（bootstrap；strict direct eval + 异常
/// 经 op_driver_error 回传）。每请求只编译这段两行小 script，不再装载 side module。
fn driver_script(body: &str) -> String {
    let body_lit = serde_json::to_string(body).unwrap_or_else(|_| "\"\"".into());
    format!("globalThis.__ojBody = {body_lit};\n__ojRun();\n")
}

/// 内省超时：坏模块顶层死循环不挂死启动。
/// ponytail: 常量而非配置；真有 >2s 的合法顶层模块再加 server.introspect_timeout。
pub const INTROSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// WS 会话连接阶段超时（模块加载 + 首次求值；与内省同量级）。
pub const WS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// WS 帧执行结果：HTTP 捕获 + ws.send 集合 + ws.close 置位。
#[derive(Debug, Default)]
pub struct WsOutcome {
    pub capture: Capture,
    pub sends: Vec<WsSend>,
    pub close: bool,
}

/// 一条待写出帧（v0.1.16 二进制支持）：ws.send 文本 / 二进制与 bus 广播帧共用同一
/// 管道（ws_sends → WsOutcome → server resp 通道 → Writer），帧类型在此定型：
/// Text → WebSocket text 帧；Binary → WebSocket binary 帧（opcode 0x2）。
#[derive(Clone, Debug, PartialEq)]
pub enum WsSend {
    Text(String),
    Binary(Vec<u8>),
}

/// 池化任务驻留会话（PRD v2 §6.2/6.3）：模块已加载、setup 已执行，模块作用域 = 任务 ctx。
/// `!Send`：钉在 TaskPool Worker 线程的 current_thread runtime 上；**永不还池**。
pub struct TaskSession {
    pub(crate) rt: JsRuntime,
    pub(crate) kill: Arc<runtime::KillSwitch>,
}

impl std::fmt::Debug for TaskSession {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("TaskSession").finish_non_exhaustive()
    }
}

impl TaskSession {
    /// 触发一个任务钩子（loop_body / teardown）。重置 per-call ReqState、武装看门狗、
    /// 调 `__task_call(hook)`、排空 event loop。超时 → runtime 已被 terminate（毒化），
    /// 调用方必须丢弃会话（teardown 不承诺，Rust Drop 兜底——FR-LT-005）。
    pub async fn fire(&mut self, hook: &str, timeout: std::time::Duration) -> Result<(), RunError> {
        {
            let op_state = runtime::op_state(&self.rt);
            let mut g = op_state.borrow_mut();
            g.borrow_mut::<ReqState>().reset(RequestInfo::default());
        }
        let handle = self.rt.v8_isolate().thread_safe_handle();
        self.kill.arm(handle, timeout);
        let hook_lit = serde_json::to_string(hook).unwrap_or_else(|_| "\"\"".into());
        let code = format!("globalThis.__task_call({hook_lit});");
        let result = match self.rt.execute_script("task_event.js", code) {
            Ok(_) => {
                self.rt
                    .run_event_loop(deno_core::PollEventLoopOptions::default())
                    .await
            }
            Err(e) => Err(CoreError::from(e)),
        };
        if self.kill.disarm() {
            return Err(RunError::Timeout);
        }
        result.map_err(RunError::Core)?;
        Self::finalize_tx_pub(&self.rt).await;
        Ok(())
    }

    async fn finalize_tx_pub(rt: &JsRuntime) {
        Bridge::finalize_tx(rt).await;
    }
}

/// WS 驻留会话：每连接独占的 runtime（模块已加载、钩子已装配）。
/// `!Send`：整条生命周期钉在 ws-js 线程的 current_thread runtime 上。
/// **永不还池**——模块作用域持有连接状态，复用会串连接；连接结束直接 drop
/// （每帧 drain event loop，正常路径 drop 前无悬挂句柄；超时毒化路径同
/// run_ws：直接丢弃）。帧池模型（spec 2026-09-09）下由 frame_pool 的 Worker 持有。
pub struct WsSession {
    pub(crate) rt: JsRuntime,
    pub(crate) kill: Arc<runtime::KillSwitch>,
}

// JsRuntime 非 Debug；测试 unwrap_err 需要 Debug（仅输出形态，不含 runtime 内部）。
impl std::fmt::Debug for WsSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsSession").finish_non_exhaustive()
    }
}

impl WsSession {
    /// 触发一个生命周期事件（connection/message/close/error）。逐帧执行逻辑统一在
    /// frame_pool 的 `ws_event`（conn=0、无外置 state 的每连接独占形态）；
    /// server 的 v0.1.9 帧循环仍在用，Task 6 帧池接线后随本路径退役。
    /// 超时 → runtime 已被 terminate（毒化），调用方必须丢弃会话断连。
    pub async fn fire(
        &mut self,
        ev: &str,
        req: RequestInfo,
        timeout: std::time::Duration,
    ) -> Result<WsOutcome, RunError> {
        frame_pool::ws_event(self, 0, ev, &req, &serde_json::Value::Null, timeout)
            .await
            .map(|(o, _)| o)
    }
}

/// handler 执行错误：区分超时熔断（408）与普通失败（500）。
#[derive(Debug)]
pub enum RunError {
    Timeout,
    /// PR-4：isolate 堆限额触发（超限被 terminate + isolate 丢弃，不回池）。
    Oom {
        limit: usize,
    },
    Core(CoreError),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Timeout => write!(f, "handler execution timed out"),
            RunError::Oom { limit } => write!(
                f,
                "js heap limit exceeded (limit {limit} bytes; server.js_heap_limit_bytes)"
            ),
            RunError::Core(e) => write!(f, "{e}"),
        }
    }
}

/// 启动 DevTools inspector：借一个 runtime 取其 inspector 句柄并起 WS 服务。
/// 仅当 `inspect=true` 构造时有效；addr 形如 `127.0.0.1:9229`。
pub async fn start_inspector(
    bridge: &Bridge,
    addr: std::net::SocketAddr,
) -> tokio::task::JoinHandle<()> {
    match bridge.inspector().await {
        Some(insp) => inspector::spawn(insp, addr),
        None => {
            tracing::warn!(target: "inspector", "inspector not enabled at construction");
            tokio::task::spawn_local(async {})
        }
    }
}

// run_module 系测试持 TRANSPILE_TEST_LOCK 跨 await（current_thread 单线程，安全）。
#[cfg(test)]
#[path = "mod_tests.rs"]
#[allow(clippy::await_holding_lock)]
mod tests;
