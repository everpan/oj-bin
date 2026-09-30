//! oj 配置（cli2.md 预案 schema）：server(host/port/app_path) + db/redis 的 URL 风格 DSN map。
//! 旧三层 env 叠加已删（预案即单文件）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 配置字段反序列化辅助：把「缺失」与「显式 null（如 `redis:` 这样的空键）」都落回 `T::default()`。
///
/// serde 的 `#[serde(default)]` 只覆盖「键不存在」；键存在却为 YAML null 时仍会报
/// `invalid type: unit value, expected a map`。`Option::<T>::deserialize` 对「缺失」与
/// 「null」都解为 `None`，故本函数对二者一视同仁，统一回退默认值——`db:` / `redis:` /
/// `plugins:` 等空键可直接留空，不必写成 `{}`。
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    // `Option::<T>` 对 YAML null 解为 `None`（→ 默认值）；对**真实类型错误**
    // （如 `redis: foo` 标量）仍透传原错，不静默吞掉误配置。`?` 保证错误上浮。
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// 单对象或命名 map 兼容反序列化：旧写法（`es: { endpoint }` / `broker: { kind: kafka }`）
/// 自动包成 `{ default: {...} }`；新写法（`es: { default: {...}, other: {...} }`）原样保留；
/// 键缺失或显式 null → 空 map。用于把单例段平滑升级为命名多源 map（CLI `--es`/`--broker` 选源）。
fn single_or_named_map<'de, D, T>(d: D) -> Result<HashMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum OneOrMap<T> {
        Single(T),
        Map(HashMap<String, T>),
    }
    let opt = Option::<OneOrMap<T>>::deserialize(d)?;
    Ok(match opt {
        None => HashMap::new(),
        Some(OneOrMap::Single(s)) => HashMap::from([("default".to_string(), s)]),
        Some(OneOrMap::Map(m)) => m,
    })
}

/// `es:` 段：单对象 → `{ default }`，命名 map 原样，缺失/null → 空。
fn es_or_default_map<'de, D>(d: D) -> Result<HashMap<String, EsCfg>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    single_or_named_map(d)
}

/// `broker:` 段：单对象 → `{ default }`，命名 map 原样，缺失/null → 空。
fn broker_or_default_map<'de, D>(d: D) -> Result<HashMap<String, BrokerCfg>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    single_or_named_map(d)
}

/// 多静态站点条目（v0.1.27，`server.static_sites`）：前缀→目录映射。
/// `prefix` 规范化见 server_cmd::resolve_app_prefix（首斜杠、无尾斜杠、`/` 唯一）；
/// `path` 相对 config 目录（CLI `--app-path prefix=dir` 给出的已按 CWD 预绝对化）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct StaticSiteConf {
    pub prefix: String,
    pub path: String,
    /// 该站点的自定义响应头（v0.1.30，如 CSP）：覆盖 `server.response_headers`
    /// 里同名的全局头；框架自有头（Content-Type/Content-Length 等）永远优先。
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

/// 路由级 timeout 覆盖（v0.1.30，`server.route_timeouts`）：pattern 段语义与
/// tenant anonymous_paths 相同（字面 / `*` 恰好一段 / `**` 跨段），按声明顺序
/// 首个命中生效；命中请求的 handler 超时用它替换全局 `server.timeout`（408 语义不变）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RouteTimeoutConf {
    pub pattern: String,
    /// 时长字符串（如 "5m"），parse_duration 解析；装配期解析失败 fail-fast。
    pub timeout: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    /// API 基础路由前缀（如 "/v1/api"）；CLI `-b` 显式给出时覆盖。
    /// 旧键名 `base` 仍可解析（serde alias），两键并存 → duplicate field 报错。
    #[serde(alias = "base")]
    pub api_prefix: String,
    /// 静态站点前缀（默认 "/" = 全路径兜底）。非 "/" 时仅该前缀下的 GET/HEAD
    /// 落静态目录（前缀剥除后解析；前缀根 → index.html），前缀外一律 404。
    /// API 路由永远优先于静态兜底。
    pub app_prefix: String,
    /// 静态站点根目录（相对 config 所在目录）；None → 不开静态服务。
    /// CLI `--app-path` 显式给出时覆盖，且按 CWD 解析（server_cmd 预绝对化后写入）。
    pub app_path: Option<String>,
    /// 多静态站点（v0.1.27）：前缀→目录映射列表。与 (app_prefix, app_path) 主站点
    /// 共存；规范化后前缀重复 → 启动 fail-fast。缺省空。
    #[serde(default)]
    pub static_sites: Vec<StaticSiteConf>,
    /// SPA 深链接回落（v0.1.20）：静态未命中 + 无扩展名 + Accept html + 不在 api_prefix
    /// 下 → 送 `<app_path>/index.html`。**默认 false** —— 静默把 404 变 200 会掩盖错配
    /// （拼错的 API 路径、丢掉的静态资源），故 SPA 工程显式开启。
    #[serde(default)]
    pub app_spa_fallback: bool,
    /// 路由感知 meta 目录（v0.1.20）：相对 `app_path` 的子目录名（如 `"__meta"`）。
    /// 送 HTML 前按请求路径查 `<app_path>/<html_meta>/<path>.json`，把 `title`/
    /// `description`/`og:*`/`twitter:*`/`canonical` 注入 `<head>`（值 HTML 转义，
    /// **不注入脚本**）。None = 不注入。该目录对静态服务不可见（命中即 404）。
    #[serde(default)]
    pub html_meta: Option<String>,
    /// 动态 meta 源（v0.1.25）：业务 handler 的**路由路径**（如 `"/v1/api/html-meta"`）。
    /// 送静态 HTML 前内部派发该 GET handler（原请求路径进 `?path=`，见 `server` 手册），
    /// 用其返回的 JSON 注入 `<head>`——与 `html_meta` **同一白名单键与转义**，另可带
    /// `cache_control` 覆盖本响应的缓存头。静态 JSON 打底、动态按 key 覆盖。
    ///
    /// 与 `html_meta` 的分工：静态 JSON 只能覆盖**构建期就知道**的路由（prerender 形态）；
    /// 需要按**数据**（issue 标题 / 分享页正文）注入时用本项。未配置 = 不派发。
    /// 装配期校验：必须在路由表里命中一个 GET 路由（拼错 fail-fast，不静默降级）。
    #[serde(default)]
    pub html_meta_handler: Option<String>,
    /// HTML 响应的 Cache-Control（v0.1.25）：**只管 HTML**（含 SPA 壳）——注入后同一份
    /// 壳可能因路由而异，不能再让中间层盲缓存。js/css/图片等资源不受影响（资源的长缓存
    /// 仍交前置反代，见已知限制）。None = 不加该头（与旧版逐字节一致）；
    /// 动态 handler 返回的 `cache_control` 优先于本项。
    #[serde(default)]
    pub html_cache_control: Option<String>,
    /// 时长字符串（如 "30s"），parse_duration 解析。
    pub timeout: String,
    pub pool_size: u32,
    /// 上传体积上限（字节；超出 413）。axum 层再乘 2 做硬顶。
    pub max_upload_bytes: u64,
    /// blob 直传 PUT 路由（`PUT {base}/blob/{key}`，v0.1.30）的体积上限（字节，
    /// 默认 1 GiB）——与 handler 面的 `max_upload_bytes` 分开：直传不经 JsActor
    /// （无 30s timeout、不占 handler 内存上限）。
    #[serde(default = "default_blob_upload_max_bytes")]
    pub blob_upload_max_bytes: u64,
    /// 自定义响应头（v0.1.30）：全局默认（动态信封/静态站点/blob 响应都补）；
    /// 与框架自有头冲突时框架优先（只补缺，不覆盖）。默认空 = 行为不变。
    #[serde(default)]
    pub response_headers: std::collections::HashMap<String, String>,
    /// 路由级 timeout 覆盖（v0.1.30）：见 RouteTimeoutConf。默认空 = 全局 timeout 一统。
    #[serde(default)]
    pub route_timeouts: Vec<RouteTimeoutConf>,
    /// 日志目录：绝对路径原样；相对 → 相对 config 目录；未配置 → config 目录下的 ./logs。
    /// 不存在则自动创建。每次启动一个新文件 `server-<启动秒>_<pid>.log`，终端输出完整镜像落盘。
    #[serde(default)]
    pub logs_dir: Option<String>,
    /// 单个日志文件大小上限（单位 M；超过滚动为 `base.1.log` 依次后移）。
    /// 小于 100 时按 100 生效（下限钳制在应用侧 logging::init）。
    pub logs_max_m: u64,
    /// 日志文件保留个数（含活动文件，超出删除；最小生效值 2）。
    pub logs_keep_files: u32,
    /// 终端输出开关（**默认 false = 只落盘**，终端保持干净）。true → 额外回写终端
    /// （stdout 与 stderr 一起，因为 tracing 控制台层写的是 stderr）。
    /// CLI `--console-log` 可打开。非 unix 平台无落盘，此时强制保留终端输出。
    pub console_log: bool,
    /// 公钥路径（PEM 格式，用于验证证书签名）
    pub public_key_path: String,
    /// 证书路径（JWS 格式，包含荷载）
    pub certificate_path: String,
    /// 宽限期（天数），证书过期后仍可接受的额外时间
    pub grace_days: Option<u64>,
    /// 启动迁移门禁（§4.6）：auto=启动即 apply 待应用迁移；verify=只校验
    /// （账本落后/存在待应用 → fail-fast）；off=不做迁移。缺省按模式取值：
    /// dev=auto、release=verify（部署 = `oj build && oj migrate && oj server`）。
    #[serde(default)]
    pub migrate_on_start: Option<String>,
    /// 表归属守卫模式（§5.3）：warn（默认，违规仅告警）| deny（违规拒绝执行）。
    /// 非法值装配期 fail-fast。
    #[serde(default)]
    pub ownership_guard: Option<String>,
    /// CORS（v0.1.35）：段存在即启用；缺段 = 不挂 CORS 层（安全默认，与 auth/oidc 一致）。
    /// 具体头在 CorsCfg 内声明；装配期校验 credentials 需显式 origins（见 oj/src/app.rs）。
    #[serde(default)]
    pub cors: Option<CorsCfg>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            // 9778：与 README / sample/config.yaml / devkit 手册一致（此前为 778，
            // 省缺 port 的用户会静默落到与文档不同的端口）。
            port: 9778,
            api_prefix: "/v1/api".into(),
            app_prefix: "/".into(),
            app_path: None,
            static_sites: Vec::new(),
            app_spa_fallback: false,
            html_meta: None,
            html_meta_handler: None,
            html_cache_control: None,
            timeout: "30s".into(),
            pool_size: 4,
            max_upload_bytes: 10 * 1024 * 1024,
            blob_upload_max_bytes: default_blob_upload_max_bytes(),
            response_headers: Default::default(),
            route_timeouts: Vec::new(),
            logs_dir: None,
            logs_max_m: 100,
            logs_keep_files: 10,
            console_log: false,
            public_key_path: "".into(),
            certificate_path: "".into(),
            grace_days: Some(30),
            migrate_on_start: None,
            ownership_guard: None,
            cors: None,
        }
    }
}

/// CORS 配置（v0.1.35，挂 `server.cors`）。段存在即启用，缺段 = 不挂 CORS 层。
/// 各字段存原始字符串，装配期在 `server/src/lib.rs` 解析为 tower-http 类型；
/// `credentials: true` 要求 `origins` 非空（否则装配期 fail-fast，避免 tower-http pan]。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CorsCfg {
    /// 允许源列表；空 = 允许任意源（AllowOrigin::any）。
    pub origins: Vec<String>,
    /// 允许方法（GET/POST/...）；空 = tower-http 默认（GET,HEAD,POST 等）。
    pub methods: Vec<String>,
    /// 允许请求头；空 = 反射请求里的 Access-Control-Request-Headers。
    pub headers: Vec<String>,
    /// 是否允许凭据（cookie/Authorization）；true 时 origins 必须非空。
    pub credentials: bool,
    /// 预检缓存秒数（max-age）；None = 不设。
    pub max_age: Option<u64>,
    /// 暴露给浏览器的响应头（Access-Control-Expose-Headers）；空 = 不额外暴露。
    pub expose: Vec<String>,
}

impl ServerCfg {
    /// 证书必配门禁判据：两个路径都配齐才算就绪（缺任一 → 装配拒绝启动）。
    /// 证书校验无任何开关——config 或 CLI 都无法绕过。
    pub fn cert_paths_configured(&self) -> bool {
        !self.public_key_path.trim().is_empty() && !self.certificate_path.trim().is_empty()
    }
}

/// 对象存储（OJ-5）：driver local|s3；local root 相对 config 目录。
// Serialize：装配层经 cfg JSON 透传给 oj-blob-s3 插件（Task 4.2，spec §3 按值传入）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct BlobCfg {
    pub driver: String,
    pub root: String,
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    /// MinIO 等路径风格访问。
    pub path_style: bool,
}

/// blob 段（spec §2 命名多后端）：平铺字段 = 旧单后端语法糖（等价 backends.default）；
/// `backends` 命名多后端。两者并存且平铺非默认 → 歧义报错（fail fast）。
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct BlobSection {
    pub driver: String,
    pub root: String,
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub path_style: bool,
    /// 命名多后端：`blob.backends.<name>`。
    pub backends: HashMap<String, BlobCfg>,
}

impl Default for BlobSection {
    /// 平铺默认值与 BlobCfg 对齐（driver "local"/root "uploads"），
    /// 否则无法区分「未写平铺」与「写了默认平铺」（entries 歧义判定依赖）。
    fn default() -> Self {
        let d = BlobCfg::default();
        Self {
            driver: d.driver,
            root: d.root,
            endpoint: None,
            bucket: None,
            region: None,
            access_key: None,
            secret_key: None,
            path_style: false,
            backends: HashMap::new(),
        }
    }
}

impl BlobSection {
    /// 归一为命名后端表：backends 非空优先（平铺非默认并存 → Err 歧义）；
    /// 否则平铺字段 = default 单后端（旧格式兼容）。
    pub fn entries(&self) -> Result<HashMap<String, BlobCfg>, String> {
        let d = BlobCfg::default();
        let flat_used = self.driver != d.driver
            || self.root != d.root
            || self.endpoint.is_some()
            || self.bucket.is_some()
            || self.region.is_some()
            || self.access_key.is_some()
            || self.secret_key.is_some()
            || self.path_style;
        if !self.backends.is_empty() {
            if flat_used {
                return Err(
                    "blob: flat fields and backends: are mutually exclusive (use backends.default for the default backend)"
                        .into(),
                );
            }
            return Ok(self.backends.clone());
        }
        Ok(HashMap::from([(
            "default".to_string(),
            BlobCfg {
                driver: self.driver.clone(),
                root: self.root.clone(),
                endpoint: self.endpoint.clone(),
                bucket: self.bucket.clone(),
                region: self.region.clone(),
                access_key: self.access_key.clone(),
                secret_key: self.secret_key.clone(),
                path_style: self.path_style,
            },
        )]))
    }
}

impl Default for BlobCfg {
    fn default() -> Self {
        Self {
            driver: "local".into(),
            root: "uploads".into(),
            endpoint: None,
            bucket: None,
            region: None,
            access_key: None,
            secret_key: None,
            path_style: false,
        }
    }
}

/// ES 客户端（OJ-6）：`es:` 块存在即启用 es.* op；endpoint 尾斜杠由 EsClient.url_for 幂等剪除。
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct EsCfg {
    pub endpoint: String,
}

/// 事件 broker（分布式事件总线）：`broker:` 块存在即按 `kind` 启用对应实现。
/// 缺省（无 `broker:` 段）= 进程内 `Bus`（零配置、保持现状）。
///
/// - `kind`：`"local"`（默认）/ `"kafka"` / `"rabbitmq"`。
/// - kafka：`brokers`（逗号分隔 bootstrap servers，必需）、`group`（消费组，默认 "oj-bus"）、
///   `topic_prefix`（物理 topic 前缀，可选）。
/// - rabbitmq：`url`（amqp URL，或取 `brokers[0]`）、`topic_prefix`（交换名，默认 "oj-bus"）。
// Serialize：装配层经 cfg JSON 透传给 bus 插件（Task 4.3，spec §3 按值传入）。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct BrokerCfg {
    pub kind: String,
    #[serde(default)]
    pub brokers: Vec<String>,
    pub url: Option<String>,
    pub group: Option<String>,
    pub topic_prefix: Option<String>,
}

/// 匿名路径条目（v0.1.23）：字符串简写，或对象形态（带「有意一层」确认）。
///
/// 背景：v0.1.20 起尾 `/*` 收紧为**严格一层**，启动期对含尾 `/*` 的列表打聚合 WARN 提示改
/// `**`。但「尾段恰好一层」本身是四形态中的合法形态——尾段是动态段时只能用 `/*` 表达
/// （改 `**` 反而扩面：`**` 含零层与任意深），这类条目永远无法让 WARN 消音。
///
/// `one_layer: true` 即对该条目的显式确认：声明「这一层是有意的」，退出迁移 WARN。
/// v0.1.23 起 WARN 默认已改为**按影响面**判定（只在改 `**` 会真多命中已注册路由时才告警，
/// 见 `oj::app::warn_legacy_tail_wildcards`），本标记用于「明知影响面仍要严格一层」的人工确认。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnonPath {
    /// 简写：`- /auth/oidc/*`
    Plain(String),
    /// 对象：`- { path: "/auth/oidc/*", one_layer: true }`
    Detailed {
        path: String,
        /// 显式确认尾 `/*` 是**有意的一层**（非待迁移的旧式宽匹配）。
        one_layer: bool,
    },
}

/// 手写 `Deserialize`（不用 `#[serde(untagged)]`），两个理由：
/// ① **未知键必须报错**——`#{serde(untagged)}` 无法 `deny_unknown_fields`，`one_layr: true`
///    这样的笔误会**静默**解析成 `one_layer=false`，用户以为已消音而实际没有，正好违背
///    `validate_anon_paths`「不让配置撒谎」的立论；
/// ② **报错要能定位**——untagged 的报错只给「did not match any variant」并把位置指到
///    序列首元素（下游列表动辄 10+ 条），这里给出「合法形态长什么样 + 哪个键不认识」。
impl<'de> Deserialize<'de> for AnonPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = AnonPath;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("字符串简写 \"/path/*\"，或对象 { path: \"/path/*\", one_layer: true }")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<AnonPath, E> {
                Ok(AnonPath::Plain(v.to_string()))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<AnonPath, E> {
                Ok(AnonPath::Plain(v))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> Result<AnonPath, A::Error> {
                let mut path: Option<String> = None;
                let mut one_layer = false;
                while let Some(key) = m.next_key::<String>()? {
                    match key.as_str() {
                        "path" => path = Some(m.next_value()?),
                        "one_layer" => one_layer = m.next_value()?,
                        other => {
                            return Err(serde::de::Error::custom(format!(
                                "anonymous_paths 条目只认 `path` 与 `one_layer` 两个键，\
                                 收到未知键 `{other}`（拼写错误？）——合法形态：字符串 \
                                 \"/path/*\" 或对象 {{ path: \"/path/*\", one_layer: true }}"
                            )));
                        }
                    }
                }
                let path = path.ok_or_else(|| {
                    serde::de::Error::custom(
                        "anonymous_paths 对象形态缺 `path`（合法形态：{ path: \"/path/*\", \
                         one_layer: true }；只要确认一层时可省 one_layer）",
                    )
                })?;
                Ok(AnonPath::Detailed { path, one_layer })
            }
        }

        d.deserialize_any(V)
    }
}

impl AnonPath {
    /// 条目路径（两种形态同口径）。
    pub fn path(&self) -> &str {
        match self {
            AnonPath::Plain(p) => p,
            AnonPath::Detailed { path, .. } => path,
        }
    }

    /// 是否显式确认为「有意的严格一层」（退出迁移 WARN）。
    pub fn one_layer(&self) -> bool {
        matches!(
            self,
            AnonPath::Detailed {
                one_layer: true,
                ..
            }
        )
    }
}

/// 匿名路径列表 → 路径字符串列表（server `Pipeline` 与 oj-auth 插件 cfg 都只要字符串；
/// 插件侧因此零改动，ABI 不变）。
pub fn anon_paths(list: &[AnonPath]) -> Vec<String> {
    list.iter().map(|p| p.path().to_string()).collect()
}

/// 装配期校验（fail-fast）：`one_layer` 只对尾 `/*` 条目有意义——挂在别的条目上是无效
/// 标记（WARN 本就不会点名它），静默接受等于让配置撒谎。
///
/// 判据用**段级**口径（末段 == `"*"`）而非裸后缀，与匹配层 `server::path_matches` 的
/// 「忽略空段」保持一致：`/idp/*/` 在运行期就是严格一层，标 `one_layer` 合法。
/// （迁移 WARN 的 `is_legacy_prefix_shape` 有意仍用裸后缀——它对齐的是 v0.1.19
/// `strip_suffix("/*")` 的历史口径，两处差异是刻意的。）
pub fn validate_anon_paths(cfg: &Config) -> Result<(), String> {
    let check = |what: &str, list: &[AnonPath]| -> Result<(), String> {
        for p in list {
            let tail_is_star = p
                .path()
                .split('/')
                .rfind(|s| !s.is_empty())
                .is_some_and(|s| s == "*");
            if p.one_layer() && !tail_is_star {
                return Err(format!(
                    "{what}: {:?} 标了 one_layer，但末段不是 \"*\" —— 该标记只用于确认「有意的严格一层」",
                    p.path()
                ));
            }
        }
        Ok(())
    };
    check("tenant.anonymous_paths", &cfg.tenant.anonymous_paths)?;
    if let Some(a) = &cfg.auth {
        check("auth.anonymous_paths", &a.anonymous_paths)?;
    }
    Ok(())
}

/// 多租户注入（OJ-3）：enable 后 handle() 从 header 提取租户 id 注入 http.tenantId。
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TenantCfg {
    pub enable: bool,
    pub header_key: String,
    /// 浏览器跳转腿豁免（去 base 后路径；尾 "/*" 一层通配）——OIDC 回跳带不了自定义头。
    pub anonymous_paths: Vec<AnonPath>,
    /// 多租户 SQL 防护（默认 off=false；true=deny 缺租户条件即拒；"warn"=仅告警软过渡）。
    /// 反序列化在 config.rs（三态共用 bridge::SqlGuard 一个类型）。
    #[serde(default)]
    pub sql_guard: crate::bridge::SqlGuard,
    /// 共享表白名单：schema.yaml 标 `tenant: false` 的表须在此列出才生效（fail-closed；
    /// 空 = 共享表声明被忽略，仍按受租户约束校验 tenant_id 列）。
    #[serde(default)]
    pub shared_allow: Vec<String>,
    /// `db.asTenant` 开关（v0.1.20）：匿名请求（anonymous_paths 命中且无租户头）允许
    /// handler 显式声明本次查询的租户身份。**默认 false** —— id 由 handler 自选、平台
    /// 无从校验，是「授信 handler」而非平台校验，故比 `db.asSystem` 多一道开关。
    #[serde(default)]
    pub allow_as_tenant: bool,
}

impl Default for TenantCfg {
    fn default() -> Self {
        Self {
            enable: false,
            header_key: "X-TENANT-ID".into(),
            anonymous_paths: Vec::new(),
            sql_guard: crate::bridge::SqlGuard::Off,
            shared_allow: Vec::new(),
            allow_as_tenant: false,
        }
    }
}

/// SqlGuard 三态反序列化：bool（true=Deny/false=Off）或字符串 off|warn|deny。
impl<'de> serde::Deserialize<'de> for crate::bridge::SqlGuard {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_yaml::Value::deserialize(d)? {
            serde_yaml::Value::Bool(false) => Ok(crate::bridge::SqlGuard::Off),
            serde_yaml::Value::Bool(true) => Ok(crate::bridge::SqlGuard::Deny),
            serde_yaml::Value::String(s) => match s.as_str() {
                "off" | "false" => Ok(crate::bridge::SqlGuard::Off),
                "warn" => Ok(crate::bridge::SqlGuard::Warn),
                "deny" => Ok(crate::bridge::SqlGuard::Deny),
                other => Err(Error::custom(format!(
                    "tenant.sql_guard: illegal value {other:?} (true|false|warn|deny)"
                ))),
            },
            _ => Err(Error::custom("tenant.sql_guard: expected bool or string")),
        }
    }
}

/// JWT 鉴权（OJ-4）：`auth:` 块存在即启用；jwt_secret 空 = 装配 fail-fast。
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AuthCfg {
    pub jwt_secret: String,
    /// HS256 | HS384 | HS512。
    pub signing_method: String,
    pub access_token_duration: String,
    pub refresh_token_duration: String,
    /// 免鉴权路径（去 base 后）；结尾 "/*" = 一层前缀通配。条目可为字符串简写或
    /// 对象形态（`{ path, one_layer }`，见 `AnonPath`）。
    pub anonymous_paths: Vec<AnonPath>,
    /// cookie 会话形态（oj-4）：原样透传 oj-auth 插件（`{"enabled":true,...}`，
    /// 键契约属插件 schema）；缺省 None = 纯 Bearer。
    #[serde(default)]
    pub cookie: Option<serde_json::Value>,
}

impl Default for AuthCfg {
    fn default() -> Self {
        Self {
            jwt_secret: String::new(),
            signing_method: "HS256".into(),
            access_token_duration: "60s".into(),
            refresh_token_duration: "720h".into(),
            anonymous_paths: Vec::new(),
            cookie: None,
        }
    }
}

/// SMTP 投递（mail 轴，spec 2026-09-15）：段存在即启用 `mail.*` 全局（须配 oj-mail 插件）。
///
/// **一段两用**：整段序列化为 JSON 交给 `oj-mail` 插件建 transport（凭据只走插件、
/// 不落宿主 JS 面）；宿主另经 `MailConfig::from_value` 只吸收每个 profile 的
/// `allowed_from`/`allowed_recipients` 做前置白名单校验。二者读同一段配置
/// （装配层把同一份 JSON 同时喂插件 cfg 与宿主校验面，避免两边分叉）。
///
/// 顶层除全局键（`workers`/`queue_capacity`/`max_attachment_bytes`/
/// `max_total_attachment_bytes`）外**每个键都是一个 profile**（键 = `Mail(key)` /
/// `mail.send` 的 profile 名）；未声明的键即未知 profile → 调用返回 `{code:5}`。
/// **新增全局键必须加进 `SmtpSection` 的显式字段**，否则会被 `flatten` 当 profile 解析。
// Serialize：装配层原样透传给 oj-mail 插件（spec §3 按值传入；空字段省略而非 null
// ——插件侧 `ProfileCfg` 是强类型 `Deserialize`，`null` 会直接报错）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SmtpSection {
    /// worker 线程数（省略 = 宿主默认）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<usize>,
    /// 有界队列容量（省略 = 宿主默认；满即背压，不无界堆积）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_capacity: Option<usize>,
    /// 单附件字节上限（省略 = 宿主默认 `MAIL_MAX_ATTACHMENT_BYTES`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attachment_bytes: Option<usize>,
    /// 单封全部附件合计字节上限（省略 = 宿主默认 `MAIL_MAX_TOTAL_ATTACHMENT_BYTES`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_total_attachment_bytes: Option<usize>,
    /// profile 名 → 配置（`flatten` 收拢其余顶层键；键序不影响语义）。
    /// **注意**：新增全局键必须在此显式声明 —— `flatten` 会把未知顶层键当 profile
    /// 结构体解析，数值键会直接报 `invalid type: integer, expected struct SmtpProfileCfg`。
    #[serde(flatten)]
    pub profiles: HashMap<String, SmtpProfileCfg>,
}

/// 单个 mail profile 的连接与投递配置（**镜像 `oj-mail` 插件 schema**：字段名/取值域一致，
/// 宿主不解释连接字段，只保证类型化解析与「空字段省略」的过线形态）。
/// 校验（`tls: none` 须显式 `allow_none_tls` 等）归插件 init：宿主不做第二套判断。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SmtpProfileCfg {
    /// SMTP 服务器主机名（同时用作 TLS 证书校验域名）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// SMTP 端口。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// 加密模式：`tls`（隐式 TLS）| `starttls`（强制升级）| `none`（明文，须显式许可）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<String>,
    /// 显式允许 `tls: none`（明文）。缺省 = 拒绝（fail-closed）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_none_tls: Option<bool>,
    /// 认证机制：`login` | `xoauth2`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mechanism: Option<String>,
    /// 认证用户名（无认证中继可省略）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 认证口令（`mechanism: login` 用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass: Option<String>,
    /// XOAUTH2 凭据（`mechanism: xoauth2` 用；本版仅静态 `access_token`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xoauth2: Option<SmtpXOAuth2Cfg>,
    /// 单次 SMTP 命令超时（秒；省略 = 插件默认 30）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// 本地落盘目录：给定时**不发网络**，`.eml` 写进该目录（FileTransport：测试/归档通道）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_transport: Option<String>,
    /// 发件人白名单（后缀匹配、大小写不敏感）。**空表 = 拒绝**（fail-closed）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_from: Vec<String>,
    /// 收件人白名单（to/cc/bcc 后缀匹配）。**空表 = 拒绝**（fail-closed）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_recipients: Vec<String>,
}

/// XOAUTH2 凭据（`mechanism: xoauth2`）：本版只用静态 `access_token`；
/// 只给 `refresh_token` 由插件 fail-loud（刷新流程未支持）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SmtpXOAuth2Cfg {
    /// 静态访问令牌（submit 时作为 Bearer 使用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// 刷新令牌（本版不消费，仅为给出明确报错而解析）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// RP 客户端注册：tenant → 外部 IdP（issuer + 凭证）。
#[derive(Debug, Deserialize, Clone)]
pub struct OidcRpCfg {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_oidc_scope")]
    pub scope: String,
}

fn default_oidc_scope() -> String {
    "openid".into()
}

/// OP 侧 client 白名单：redirect_uri 精确串 + 租户绑定。
#[derive(Debug, Deserialize, Clone)]
pub struct OidcClientCfg {
    pub secret: String,
    pub redirect_uris: Vec<String>,
    pub tenant: String,
}

/// OIDC（spec 2026-09-05 §3.1）：段存在即启用；private_key_path 相对 config 目录。
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct OidcSection {
    pub issuer: String,
    pub private_key_path: String,
    pub rp: HashMap<String, OidcRpCfg>,
    pub clients: HashMap<String, OidcClientCfg>,
}

/// 长任务池配置（spec 2026-09-07 §6）。dir 相对 API 目录（dev=src/、release=dist/）。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TasksCfg {
    /// 任务池目录（相对 API 目录）。
    pub dir: String,
    /// 任务数上限（超过 = fail-fast，防误配打满机器）。
    pub max: usize,
    /// 停机宽限秒数：flag 置位后任务有此窗口自然收场，到期看门狗强杀。
    pub stop_grace_secs: u64,
    /// 池化任务执行体池（v0.2 事件驱动 PRD v2 §6.8）。
    pub pool: TaskPoolCfg,
    /// crontab 配置（相对 API 目录；文件缺失 = 无定时任务）。
    pub crontab: String,
    /// 执行事件日志（JSONL；disabled = 不写盘）。
    pub event_log: TaskEventLogCfg,
}

/// 池化任务执行体配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TaskPoolCfg {
    /// Worker 线程数（每 Worker 一个 Bridge，多任务交替执行 loop_body）。
    pub workers: usize,
    /// 单轮 loop_body 看门狗超时（毫秒；0 = 5s 默认在 default()）。
    pub loop_body_timeout_ms: u64,
    /// 轮间节奏（毫秒）：忙扫每轮后 sleep 这么久，防 trivial loop_body 空转独占 Worker。
    /// 0 = 不限制（忙扫结束立即轮转——压测语义，生产会被空转打满 CPU）。
    pub interval_ms: u64,
}

impl Default for TaskPoolCfg {
    fn default() -> Self {
        Self {
            workers: 4,
            loop_body_timeout_ms: 5000,
            interval_ms: 100,
        }
    }
}

/// 任务执行事件日志配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TaskEventLogCfg {
    pub enabled: bool,
    /// 相对进程 CWD（对齐 logs/ 布局）。
    pub path: String,
    /// 单文件滚动上限 MB（0 = 不滚动，ponytail：先一刀切复制数）。
    pub max_mb: u64,
}

impl Default for TaskEventLogCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "logs/task-events.jsonl".into(),
            max_mb: 16,
        }
    }
}

impl Default for TasksCfg {
    fn default() -> Self {
        Self {
            dir: "tasks".into(),
            max: 64,
            stop_grace_secs: 30,
            pool: TaskPoolCfg::default(),
            crontab: "task/crontab.yaml".into(),
            event_log: TaskEventLogCfg::default(),
        }
    }
}

/// 凭据密封（v0.1.33）：config 里 `ENC[...]` 密文值的密钥位置。
///
/// **公钥可随仓库走**（`oj secret seal` 用它加密，见不解密权）；**私钥只在部署机**
/// ——本段只是私钥的**位置**（PEM 内容本身不该进 config，否则等于把钥匙贴在同一张纸上）。
/// 私钥三通道（优先级从高到低）：`OJ_SECRET_KEY`（内联 PEM）> `OJ_SECRET_KEY_FILE`
/// （文件路径）> 本段 `private_key_path`（相对 config 目录）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct SecretsCfg {
    /// 解密私钥 PEM 路径（相对 config 目录；绝对路径原样）。
    #[serde(default)]
    pub private_key_path: Option<String>,
    /// 加密公钥 PEM 路径（`oj secret seal` 未显式给 `-k` 时用；可选）。
    #[serde(default)]
    pub public_key_path: Option<String>,
}

/// WS 运行时配置（spec 2026-09-09 帧池）。段缺省 = 全默认。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WsCfg {
    /// 全局并发连接上限：超限 upgrade 直接 503；0 = 不限制。
    pub max_connections: u64,
    /// 每路由 Worker 数（无状态，可小于并发连接数）。
    pub workers_per_route: usize,
    /// 路由连接归零后 Worker 池保活毫秒数（0 = 立即退役；调大吃暖启动收益）。
    pub idle_linger_ms: u64,
}

impl Default for WsCfg {
    fn default() -> Self {
        Self {
            max_connections: 1000,
            workers_per_route: 2,
            idle_linger_ms: 0,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    /// name → DSN（sqlite://…、mysql://…、postgres://… 可混用；seed 仅 default 为 sqlite 时重放）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub db: HashMap<String, String>,
    /// name → redis URL（v0.1 warn 后用内存 KV）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub redis: HashMap<String, String>,
    pub tenant: TenantCfg,
    /// None = 不启用鉴权（内置 /auth/* 与 Bearer 守卫均不挂）。
    pub auth: Option<AuthCfg>,
    /// None = 不启用 OIDC。
    pub oidc: Option<OidcSection>,
    /// None = 不启用 blob（blob 全局/上传/下载路由均不挂）。
    pub blob: Option<BlobSection>,
    /// None = 不启用 mail（`mail.*` 全局报 "mail not configured"）；段存在即启用。
    /// 段由 `oj-mail` 插件投递、宿主持白名单校验与附件解析（见 [`SmtpSection`]）。
    pub smtp: Option<SmtpSection>,
    /// None = 不启用 ldap（`ldap.*` 全局报 "ldap not configured"）；段存在即启用。
    /// **一段两用**：整段序列化为 JSON 交给 `oj-ldap` 插件（凭据只走插件）；
    /// 宿主用 `bridge::LdapConfig::from_value` 吃同一份 JSON 做白名单校验与实例选单。
    /// 段为**不透明** map（serde_yaml::Value，见 plugins 段同形）——实例字段的白名单
    /// 归 `LdapConfig::from_value` 独家裁决，未知键装配期 fail-fast（不静默丢弃）。
    pub ldap: Option<serde_yaml::Value>,
    /// None = 不启用 ES（es.* op 报 "es not configured"）。
    /// 命名多源 map：键 = profile 名，CLI `--es <profile>` 选哪个作默认（默认 default）。
    /// 旧单对象写法 `es: { endpoint }` 兼容自动包成 `{ default: {...} }`。
    #[serde(default, deserialize_with = "es_or_default_map")]
    pub es: HashMap<String, EsCfg>,
    /// None = 不启用分布式 broker（事件总线退化为进程内 Bus）。
    /// 命名多源 map：键 = profile 名，CLI `--broker <profile>` 选哪个作默认（默认 default）。
    /// 旧单对象写法 `broker: { kind: kafka }` 兼容自动包成 `{ default: {...} }`。
    #[serde(default, deserialize_with = "broker_or_default_map")]
    pub broker: HashMap<String, BrokerCfg>,
    /// 插件声明（spec「plugins: 统一语义」一段三用）：键 = 要加载的插件名（非空即
    /// 严格模式，只装配列出的插件，沿用清单门禁）；值 = 插件 cfg，非空对象原样透传，
    /// 空对象跳过透传回落轴适配器。缺省/空 map = 扫描模式（加载 plugins_dir 全部）。
    /// 旧 list 写法 `plugins: [a, b]` 废弃（解析报错 fail-fast）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub plugins: HashMap<String, serde_json::Value>,
    /// 命名 MQ 实例（spec 2026-09-07 §3）：kafkas.default = { brokers, group } →
    /// Kafka("default")。值 JSON 透传给 mq 插件（kind 由装配层按段来源注入）。
    /// 段缺省 = 不启用（registry 空 → Kafka/RabbitMQ(name) → undefined）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub kafkas: HashMap<String, serde_json::Value>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub rabbits: HashMap<String, serde_json::Value>,
    /// 长任务池（spec 2026-09-07 §6）：目录约定 task_{name}.* / {name}_task.*；
    /// 缺省段 = 默认值（dir "tasks"，目录不存在 = 无任务，不报错）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub tasks: TasksCfg,
    /// WS 运行时（spec 2026-09-09 帧池）：闸门 / Worker 数 / 空闲退役。
    #[serde(default, deserialize_with = "null_as_default")]
    pub ws: WsCfg,
    /// 构造器 LIMIT（v0.1.20）：`default_limit` = 顶层 select 未给 limit 时的隐式值
    /// （默认 100），`max_limit` = 显式 limit 的 clamp 上界（默认 1000，硬顶 100000）。
    /// 两者都在装配期校验（0 / 倒置 / 超硬顶 均 fail-fast）。
    /// **不能塞进 `db:`**——`db` 是 name → DSN 的 map，键即库名。
    #[serde(default, deserialize_with = "null_as_default")]
    pub db_query: crate::bridge::QueryLimits,
    /// 凭据密封（v0.1.33）：`ENC[...]` 密文的密钥位置。段缺省 = 无私钥
    /// （此时配置里出现 `ENC[...]` 会 fail-fast，见 [`SecretsCfg`]）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub secrets: SecretsCfg,
    /// plugins 目录（相对 config_dir；None = 走 OJ_PLUGINS_DIR > <exe>/plugins > <workspace_root>/bin/plugins 后备）。
    pub plugins_dir: Option<PathBuf>,
    /// 部署期常量（v0.1.25）：`name → string`，handler 经 `vars.get(name)` 读。
    ///
    /// **fail-closed**：未在本段声明的键一律读不到（JS 侧恒 `null`）——平台**没有**
    /// 「读任意 OS env / 读任意 config 键」的通道（旧三层 env 叠加已删，单文件 config
    /// 是唯一真相源）。值只能是**标量**：字符串/数字/布尔都按其 YAML 字面量读成串
    /// （`PORT: 3000` → JS 收到 `"3000"`；`FLAG: true` → `"true"`），嵌套 map/list 在
    /// 解析期报错（本段不是塞 JSON 的地方）。
    #[serde(default, deserialize_with = "null_as_default")]
    pub vars: HashMap<String, String>,
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
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // 密封值解密发生在 **Value 层**（`Config::deserialize` 之前）：ldap/plugins/kafkas
    // 是不透明 Value，类型层够不着其中的 `bind_pw`；在树上递归替换则全段覆盖且
    // schema 零改动。配置里没有 `ENC[...]` 时完全不碰密钥路径（旧配置逐字节不变）。
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    if crate::secret::has_sealed(&value) {
        let cfg_path = value
            .get("secrets")
            .and_then(|s| s.get("private_key_path"))
            .and_then(|v| v.as_str());
        let key = crate::secret::load_private_key(cfg_path, dir).map_err(|e| {
            format!(
                "{}: config has ENC[...] sealed values but {e}",
                path.display()
            )
        })?;
        crate::secret::decrypt_tree(&mut value, &key)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // 解密后**回经文本**再反序列化，而不是 `from_value`：serde_yaml 的 `from_str`
        // 会把裸标量按字面量读成字符串（`vars: {PORT: 3000}` → `"3000"`，该段依赖此
        // 行为），而 `from_value` 对 `Value::Number` 直接报 "invalid type: integer"。
        // 走文本才能与未加密路径逐字节同行为。
        let plain = serde_yaml::to_string(&value)
            .map_err(|e| format!("{}: re-serialize after decrypt: {e}", path.display()))?;
        return serde_yaml::from_str(&plain).map_err(|e| format!("parse {}: {e}", path.display()));
    }
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
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return Err(format!("invalid duration unit: {unit}")),
    };
    Ok(std::time::Duration::from_secs_f64(n * mult))
}

/// blob 直传 PUT 默认上限：1 GiB（office/媒体类大文件的直传面，与 handler 10MB 分开）。
fn default_blob_upload_max_bytes() -> u64 {
    1024 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_file() {
        let c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
        assert_eq!((c.server.host.as_str(), c.server.port), ("localhost", 9778));
        assert_eq!(c.server.api_prefix, "/v1/api");
        assert_eq!(c.server.app_prefix, "/");
        assert!(c.server.app_path.is_none());
        assert_eq!(parse_duration(&c.server.timeout).unwrap().as_secs(), 30);
        assert_eq!(c.server.pool_size, 4);
        assert!(c.db.is_empty() && c.redis.is_empty());
    }

    #[test]
    fn ws_section_defaults_and_override() {
        let c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
        assert_eq!(
            (
                c.ws.max_connections,
                c.ws.workers_per_route,
                c.ws.idle_linger_ms
            ),
            (1000, 2, 0)
        );
        let dir = std::env::temp_dir().join(format!("oj-wscfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "ws:\n  max_connections: 5\n  workers_per_route: 3\n  idle_linger_ms: 60000\n",
        )
        .unwrap();
        let c = load_from(&dir, None).unwrap();
        assert_eq!(
            (
                c.ws.max_connections,
                c.ws.workers_per_route,
                c.ws.idle_linger_ms
            ),
            (5, 3, 60000)
        );
    }

    /// `vars:` 段（v0.1.25）：标量按 YAML 字面量读成字符串（数字/布尔不必加引号）；
    /// 嵌套结构解析期报错（值只能是标量）；未声明的键不在表里（fail-closed 的数据面）。
    #[test]
    fn vars_section_reads_scalars_and_rejects_nested() {
        let dir = std::env::temp_dir().join(format!("oj-varcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "vars:\n  WEB_URL: https://plane.example\n  PORT: 3000\n  FLAG: true\n  RETRIES: \"3\"\n",
        )
        .unwrap();
        let c = load_from(&dir, None).unwrap();
        assert_eq!(
            c.vars.get("WEB_URL").map(String::as_str),
            Some("https://plane.example")
        );
        // 标量原样成串（JS 侧读到的就是这里写的那个字面量）
        assert_eq!(c.vars.get("PORT").map(String::as_str), Some("3000"));
        assert_eq!(c.vars.get("FLAG").map(String::as_str), Some("true"));
        assert_eq!(c.vars.get("RETRIES").map(String::as_str), Some("3"));
        assert!(!c.vars.contains_key("DB_PASSWORD"), "未声明键不在表里");
        // 嵌套结构 → 解析期报错（vars 只装标量；不要把 config 段当 JSON 塞进来）
        std::fs::write(dir.join("config.yaml"), "vars:\n  OOPS:\n    a: 1\n").unwrap();
        let e = load_from(&dir, None).unwrap_err();
        assert!(e.contains("invalid type"), "{e}");
        // 缺段 → 空表（`vars.get` 恒 null），不是报错
        std::fs::write(dir.join("config.yaml"), "server:\n  port: 9778\n").unwrap();
        assert!(load_from(&dir, None).unwrap().vars.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// config 里的 `ENC[...]` 在 `load_from` 里就地解密（含不透明段 ldap）：
    /// 解密后 `Config` 拿到的就是明文，下游（插件 cfg 透传 / DSN 拼接）零改动。
    #[test]
    fn sealed_values_are_decrypted_on_load() {
        let (priv_pem, pub_pem) = crate::secret::keygen().unwrap();
        let dir = std::env::temp_dir().join(format!("ojcfgseal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("k.pem"), &priv_pem).unwrap();
        let dsn = crate::secret::seal(&pub_pem, "mysql://root:hunter2@127.0.0.1:3306/app").unwrap();
        let pw = crate::secret::seal(&pub_pem, "bind-secret").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!(
                "secrets:\n  private_key_path: k.pem\n\
                 db:\n  default: \"{dsn}\"\n\
                 ldap:\n  default:\n    url: ldap://h:389\n    bind_pw: \"{pw}\"\n"
            ),
        )
        .unwrap();
        let c = load_from(&dir, None).unwrap();
        assert_eq!(c.db["default"], "mysql://root:hunter2@127.0.0.1:3306/app");
        assert_eq!(
            c.ldap.as_ref().unwrap()["default"]["bind_pw"].as_str(),
            Some("bind-secret")
        );
        // 明文不该出现在文件里（这就是本项要防的事故面）。
        let raw = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
        assert!(!raw.contains("hunter2") && !raw.contains("bind-secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// fail-fast：有 `ENC[...]` 却拿不到私钥 → 拒绝加载。
    /// 静默把密文当明文用（连上一个叫 "ENC[...]" 的密码）是不可接受的失败形态。
    #[test]
    fn sealed_value_without_private_key_fails_fast() {
        let (_, pub_pem) = crate::secret::keygen().unwrap();
        let dir = std::env::temp_dir().join(format!("ojcfgnoseal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let token = crate::secret::seal(&pub_pem, "x").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!("db:\n  default: \"{token}\"\n"),
        )
        .unwrap();
        let e = load_from(&dir, None).unwrap_err();
        assert!(e.contains("sealed"), "{e}");
        assert!(e.contains("OJ_SECRET_KEY"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 密封值只能落在**字符串字段**上：解密结果必然是字符串，落到 `port` 这类
    /// 数值字段会 `invalid type: string` —— 这是限制（不是 bug），写死在用例里。
    #[test]
    fn sealed_value_on_numeric_field_is_a_parse_error() {
        let (priv_pem, pub_pem) = crate::secret::keygen().unwrap();
        let dir = std::env::temp_dir().join(format!("ojcfgnum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("k.pem"), &priv_pem).unwrap();
        let sealed = crate::secret::seal(&pub_pem, "9778").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!("secrets:\n  private_key_path: k.pem\nserver:\n  port: \"{sealed}\"\n"),
        )
        .unwrap();
        let e = load_from(&dir, None).unwrap_err();
        assert!(e.contains("invalid type"), "{e}");
        // 解密确实发生过（报错里是明文 9778 而非密文）。
        assert!(e.contains("9778"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归：加密路径不能改变 YAML 的标量→字符串语义。
    /// `vars: {PORT: 3000}` 一直读成 `"3000"`（见 `vars` 段文档）；若解密后直接
    /// `from_value`，`Value::Number(3000)` 进 `HashMap<String,String>` 会报
    /// "invalid type: integer" —— 故解密后回经文本再解析。
    #[test]
    fn sealed_path_keeps_scalar_to_string_coercion() {
        let (priv_pem, pub_pem) = crate::secret::keygen().unwrap();
        let dir = std::env::temp_dir().join(format!("ojcfgcoerce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("k.pem"), &priv_pem).unwrap();
        let sealed = crate::secret::seal(&pub_pem, "s3cret").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!(
                "secrets:\n  private_key_path: k.pem\n\
                 vars:\n  PORT: 3000\n  FLAG: true\n\
                 auth:\n  jwt_secret: \"{sealed}\"\n"
            ),
        )
        .unwrap();
        let c = load_from(&dir, None).unwrap();
        assert_eq!(c.vars.get("PORT").map(String::as_str), Some("3000"));
        assert_eq!(c.vars.get("FLAG").map(String::as_str), Some("true"));
        assert_eq!(c.auth.as_ref().unwrap().jwt_secret, "s3cret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧配置（无 `ENC[...]`）完全不碰密钥路径：不配私钥也照常加载。
    #[test]
    fn plain_config_loads_without_any_key() {
        let dir = std::env::temp_dir().join(format!("ojcfgplain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "db:\n  default: sqlite://a.db\nauth:\n  jwt_secret: s\n",
        )
        .unwrap();
        let c = load_from(&dir, None).unwrap();
        assert_eq!(c.db["default"], "sqlite://a.db");
        assert!(c.secrets.private_key_path.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空键（`redis:` / `db:` / `plugins:` / `kafkas:` 等写成了 YAML null）应落回默认值，
    /// 而非报 `invalid type: unit value, expected a map`。缺失键、显式 null、正常值三者统一。
    #[test]
    fn empty_keys_are_null_tolerant() {
        let dir = std::env::temp_dir().join(format!("ojcfgempty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            concat!(
                "redis:\n", // 空键 → null → 空 map
                "db:\n",
                "plugins:\n",
                "kafkas:\n",
                "rabbits:\n",
                "vars:\n",
                "secrets:\n", // 段空键 → SecretsCfg 默认值
                "tasks:\n",   // 段空键 → TasksCfg 默认值
                "auth:\n  jwt_secret: s\n",
            ),
        )
        .unwrap();
        let c = load_from(&dir, None).unwrap();
        assert!(c.redis.is_empty());
        assert!(c.db.is_empty());
        assert!(c.plugins.is_empty());
        assert!(c.kafkas.is_empty());
        assert!(c.rabbits.is_empty());
        assert!(c.vars.is_empty());
        assert!(c.secrets.private_key_path.is_none());
        // 真实类型错误仍要上浮（不静默吞掉误配置）：redis 写成标量
        std::fs::write(
            dir.join("config.yaml"),
            "redis: not-a-map\nauth:\n  jwt_secret: s\n",
        )
        .unwrap();
        let e = load_from(&dir, None).unwrap_err();
        assert!(e.contains("invalid type"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
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
        // `base:` 为旧键名（serde alias）——本用例兼测旧配置兼容；新键 `api_prefix`
        // 与旧键并存时 serde 报 duplicate field（防两处配置漂移）。
        std::fs::write(dir.join("cfg.yaml"), concat!(
            "server:\n  host: 0.0.0.0\n  port: 9000\n  base: /xapi\n  app_prefix: /site\n  app_path: public\n  timeout: 5s\n  pool_size: 2\n",
            "db:\n  default: sqlite://db.sqlite\n",
            "redis:\n  default: redis://127.0.0.1:6379/1\n",
        )).unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.server.host, "0.0.0.0");
        assert_eq!(c.server.api_prefix, "/xapi");
        assert_eq!(c.server.app_prefix, "/site");
        assert_eq!(c.server.app_path.as_deref(), Some("public"));
        assert_eq!(c.db["default"], "sqlite://db.sqlite");
        assert_eq!(c.redis.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_sites_parse_and_default() {
        // 缺省为空表。
        assert!(ServerCfg::default().static_sites.is_empty());
        // round-trip：前缀→目录两条（含 "/" 兜底站点）。
        let dir = std::env::temp_dir().join(format!("ojcfgsites-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "server:\n  static_sites:\n    - { prefix: /docs, path: d1 }\n    - { prefix: /, path: d2 }\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(
            c.server.static_sites,
            vec![
                StaticSiteConf {
                    prefix: "/docs".into(),
                    path: "d1".into(),
                    headers: Default::default(),
                },
                StaticSiteConf {
                    prefix: "/".into(),
                    path: "d2".into(),
                    headers: Default::default(),
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_sites_headers_and_new_server_keys_parse() {
        let dir = std::env::temp_dir().join(format!("ojcfgkeys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "server:\n  static_sites:\n    - { prefix: /docs, path: d1, headers: { csp: 'default-src self' } }\n  blob_upload_max_bytes: 104857600\n  response_headers: { x-frame-options: DENY, referrer-policy: no-referrer }\n  route_timeouts:\n    - { pattern: /v1/api/convert/**, timeout: 5m }\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.server.blob_upload_max_bytes, 104857600);
        assert_eq!(
            c.server
                .response_headers
                .get("x-frame-options")
                .map(String::as_str),
            Some("DENY")
        );
        assert_eq!(
            c.server.static_sites[0]
                .headers
                .get("csp")
                .map(String::as_str),
            Some("default-src self")
        );
        assert_eq!(c.server.route_timeouts.len(), 1);
        assert_eq!(c.server.route_timeouts[0].pattern, "/v1/api/convert/**");
        assert_eq!(c.server.route_timeouts[0].timeout, "5m");
        // 缺省：1 GiB / 空表。
        assert_eq!(
            ServerCfg::default().blob_upload_max_bytes,
            1024 * 1024 * 1024
        );
        assert!(ServerCfg::default().response_headers.is_empty());
        assert!(ServerCfg::default().route_timeouts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tenant_cfg_defaults_and_parse() {
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(!c.tenant.enable && c.tenant.header_key == "X-TENANT-ID");
        let dir = std::env::temp_dir().join(format!("ojcfgt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "tenant:\n  enable: true\n  header_key: X-ACCT\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert!(c.tenant.enable && c.tenant.header_key == "X-ACCT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tenant_sql_guard_parse() {
        let c: Config =
            serde_yaml::from_str("tenant:\n  enable: true\n  sql_guard: true\n").unwrap();
        assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Deny);
        let c: Config = serde_yaml::from_str("tenant:\n  sql_guard: warn\n").unwrap();
        assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Warn);
        let c: Config = serde_yaml::from_str("tenant:\n  sql_guard: deny\n").unwrap();
        assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Deny);
        let c: Config = serde_yaml::from_str("tenant:\n  sql_guard: off\n").unwrap();
        assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Off);
        let c: Config = serde_yaml::from_str("tenant: {}\n").unwrap();
        assert_eq!(c.tenant.sql_guard, crate::bridge::SqlGuard::Off);
        assert_eq!(c.tenant.shared_allow, Vec::<String>::new());
        let c: Config =
            serde_yaml::from_str("tenant:\n  sql_guard: true\n  shared_allow: [dict, geo]\n")
                .unwrap();
        assert_eq!(
            c.tenant.shared_allow,
            vec!["dict".to_string(), "geo".to_string()]
        );
        assert!(serde_yaml::from_str::<Config>("tenant:\n  sql_guard: bogus\n").is_err());
    }

    /// 证书强制必配：未配置 public_key_path / certificate_path → `cert_paths_configured`
    /// 为 false（装配层据此拒绝启动，任何方式都无法绕过——config 无开关、CLI 无逃生口）。
    #[test]
    fn certificate_mandatory_no_escape_hatch() {
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(!c.server.cert_paths_configured());
        // 显式在 config 里写 require_cert: false 也不再生效（字段已删除，YAML 忽略）：
        let dir = std::env::temp_dir().join(format!("ojcfgc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "server:\n  require_cert: false\n  public_key_path: \"\"\n  certificate_path: \"\"\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert!(
            !c.server.cert_paths_configured(),
            "no config can disable the requirement"
        );
        // 配齐两个路径才算就绪；缺任一不算。
        let mut c2 = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert!(!c2.server.cert_paths_configured());
        c2.server.public_key_path = "k.pem".into();
        assert!(
            !c2.server.cert_paths_configured(),
            "one path alone is not enough"
        );
        c2.server.certificate_path = "c.jws".into();
        assert!(c2.server.cert_paths_configured());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasks_section_parse() {
        // tasks: 段（spec §6）：dir / max / stop_grace_secs；缺省 = 内置默认。
        let dir = std::env::temp_dir().join(format!("ojcfgtask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            concat!(
                "tasks:\n",
                "  dir: workers\n",
                "  max: 8\n",
                "  stop_grace_secs: 5\n",
            ),
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.tasks.dir, "workers");
        assert_eq!(c.tasks.max, 8);
        assert_eq!(c.tasks.stop_grace_secs, 5);
        let _ = std::fs::remove_dir_all(&dir);

        // 缺省：默认值（dir "tasks" / max 64 / grace 30s）。
        let dir2 = std::env::temp_dir().join(format!("ojcfgtask2-{}", std::process::id()));
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("cfg.yaml"), "server: {}\n").unwrap();
        let c2 = load_from(&dir2, Some("cfg.yaml")).unwrap();
        assert_eq!(c2.tasks.dir, "tasks");
        assert_eq!(c2.tasks.max, 64);
        assert_eq!(c2.tasks.stop_grace_secs, 30);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn mq_named_sections_parse() {
        // kafkas:/rabbits: 命名段（spec 2026-09-07 §3）：值 JSON 透传；缺省段 = 空 map。
        let dir = std::env::temp_dir().join(format!("ojcfgmq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            concat!(
                "kafkas:\n",
                "  default:\n",
                "    brokers: [b1:9092, b2:9092]\n",
                "    group: g1\n",
                "rabbits:\n",
                "  default:\n",
                "    url: amqp://x:5672\n",
            ),
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.kafkas["default"]["brokers"].as_array().unwrap().len(), 2);
        assert_eq!(c.kafkas["default"]["group"].as_str(), Some("g1"));
        assert_eq!(c.rabbits["default"]["url"].as_str(), Some("amqp://x:5672"));
        let _ = std::fs::remove_dir_all(&dir);

        // 缺省：两段均为空 map（段存在即启用哲学的反面——不写不启用）。
        let dir2 = std::env::temp_dir().join(format!("ojcfgmq2-{}", std::process::id()));
        std::fs::create_dir_all(&dir2).unwrap();
        std::fs::write(dir2.join("cfg.yaml"), "server: {}\n").unwrap();
        let c2 = load_from(&dir2, Some("cfg.yaml")).unwrap();
        assert!(c2.kafkas.is_empty() && c2.rabbits.is_empty());
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn blob_backends_named_sections_parse() {
        let dir = std::env::temp_dir().join(format!("ojcfgbb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            concat!(
                "blob:\n",
                "  backends:\n",
                "    default:\n",
                "      driver: local\n",
                "      root: uploads\n",
                "    img:\n",
                "      driver: s3\n",
                "      bucket: b\n",
                "      region: r\n",
            ),
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let entries = c.blob.expect("some").entries().unwrap();
        assert!(entries.contains_key("default") && entries.contains_key("img"));
        assert_eq!(entries["img"].bucket.as_deref(), Some("b"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blob_flat_and_backends_coexist_is_ambiguous_error() {
        let dir = std::env::temp_dir().join(format!("ojcfgab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "blob:\n  driver: s3\n  bucket: b\n  region: r\n  backends:\n    default:\n      driver: local\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let e = c.blob.expect("some").entries().err().unwrap_or_default();
        assert!(e.contains("mutually exclusive"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blob_flat_legacy_maps_to_default_entry() {
        let dir = std::env::temp_dir().join(format!("ojcfglg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "blob:\n  driver: local\n  root: up2\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let entries = c.blob.expect("some").entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries["default"].root, "up2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blob_cfg_defaults_and_s3_parse() {
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.blob.is_none());
        assert_eq!(ServerCfg::default().max_upload_bytes, 10 * 1024 * 1024);
        assert_eq!(ServerCfg::default().logs_max_m, 100);
        assert_eq!(ServerCfg::default().logs_keep_files, 10);
        let dir = std::env::temp_dir().join(format!("ojcfgbl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            concat!(
                "blob:\n",
                "  driver: s3\n",
                "  endpoint: http://127.0.0.1:9000\n",
                "  bucket: app\n",
                "  region: us-east-1\n",
                "  access_key: minioadmin\n",
                "  secret_key: minioadmin\n",
                "  path_style: true\n",
                "server:\n  max_upload_bytes: 2048\n",
            ),
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let b = c.blob.expect("some");
        assert_eq!(b.driver, "s3");
        assert_eq!(b.bucket.as_deref(), Some("app"));
        assert!(b.path_style);
        assert_eq!(c.server.max_upload_bytes, 2048);
        // 省缺字段走默认（直接断 Default；YAML 裸 `blob:` 是 null → None）
        let d = BlobCfg::default();
        assert_eq!((d.driver.as_str(), d.root.as_str()), ("local", "uploads"));
        assert!(!d.path_style && d.bucket.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn es_cfg_defaults_and_parse() {
        // 未配置 → 空 map（es.* 报 "es not configured"）
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.es.is_empty());
        // 旧单对象写法 `es: { endpoint }` → 兼容包成 { default: { endpoint } }
        let dir = std::env::temp_dir().join(format!("ojcfge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "es:\n  endpoint: http://127.0.0.1:9200/\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let e = c.es.get("default").expect("default profile");
        assert_eq!(e.endpoint, "http://127.0.0.1:9200/");
        // 命名多源 map 原样保留
        std::fs::write(
            dir.join("cfg.yaml"),
            "es:\n  default: { endpoint: http://a:9200 }\n  analytics: { endpoint: http://b:9200 }\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.es.get("default").unwrap().endpoint, "http://a:9200");
        assert_eq!(c.es.get("analytics").unwrap().endpoint, "http://b:9200");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn es_cfg_null_and_scalar_rejected() {
        // 显式 null → 空 map（与缺失等价，向后兼容「写了 es: 但没填值」）。
        let dir = std::env::temp_dir().join(format!("ojcfgn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cfg.yaml"), "es:\n  null\n").unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert!(c.es.is_empty(), "es: null must parse to empty map");
        // 标量（非对象） → 反序列化失败（不静默吞）。
        std::fs::write(dir.join("cfg.yaml"), "es: \"a-string\"\n").unwrap();
        assert!(
            load_from(&dir, Some("cfg.yaml")).is_err(),
            "es as scalar must be rejected"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_cfg_defaults_and_none() {
        // auth 未配置 → None
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.auth.is_none());
        // auth: 存在但字段全省缺 → 各默认值
        let dir = std::env::temp_dir().join(format!("ojcfga-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cfg.yaml"), "auth:\n  jwt_secret: s3cret\n").unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let a = c.auth.expect("some");
        assert_eq!(a.jwt_secret, "s3cret");
        assert_eq!(a.signing_method, "HS256");
        assert_eq!(a.access_token_duration, "60s");
        assert_eq!(a.refresh_token_duration, "720h");
        assert!(a.anonymous_paths.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oidc_section_and_tenant_anonymous_paths_parse() {
        let c: Config = serde_yaml::from_str(
            "oidc:\n\
             \x20 issuer: \"https://idp.example\"\n\
             \x20 private_key_path: \"config/oidc.pem\"\n\
             \x20 rp:\n\
             \x20   acme:\n\
             \x20     issuer: \"https://acme.example\"\n\
             \x20     client_id: \"cid\"\n\
             \x20     client_secret: \"sec\"\n\
             \x20 clients:\n\
             \x20   web:\n\
             \x20     secret: \"s2\"\n\
             \x20     redirect_uris: [\"http://x/cb\"]\n\
             \x20     tenant: \"acme\"\n\
             tenant:\n\
             \x20 enable: true\n\
             \x20 anonymous_paths: [\"/oidc/*\"]\n",
        )
        .unwrap();
        let o = c.oidc.as_ref().unwrap();
        assert_eq!(o.issuer, "https://idp.example");
        assert_eq!(o.rp["acme"].client_id, "cid");
        assert_eq!(o.rp["acme"].scope, "openid"); // 缺省 scope
        assert_eq!(o.clients["web"].redirect_uris[0], "http://x/cb");
        assert_eq!(
            c.tenant.anonymous_paths,
            vec![AnonPath::Plain("/oidc/*".into())]
        );
    }

    #[test]
    fn anon_paths_accepts_string_and_object_forms() {
        // v0.1.23：条目可为字符串简写或对象形态（one_layer 确认）。
        let c: Config = serde_yaml::from_str(
            "auth:\n\
             \x20 jwt_secret: \"s\"\n\
             \x20 anonymous_paths:\n\
             \x20   - /auth/login\n\
             \x20   - path: \"/auth/oidc/*\"\n\
             \x20     one_layer: true\n\
             \x20   - path: \"/public/export/*\"\n",
        )
        .unwrap();
        let a = c.auth.as_ref().unwrap();
        assert_eq!(a.anonymous_paths.len(), 3);
        assert_eq!(a.anonymous_paths[0], AnonPath::Plain("/auth/login".into()));
        assert!(!a.anonymous_paths[0].one_layer());
        assert_eq!(a.anonymous_paths[1].path(), "/auth/oidc/*");
        assert!(a.anonymous_paths[1].one_layer());
        // 对象形态缺省 one_layer → false（等同未确认）。
        assert!(!a.anonymous_paths[2].one_layer());
        // 消费侧归一：两种形态都给同一份字符串列表（插件 cfg / Pipeline 用）。
        assert_eq!(
            anon_paths(&a.anonymous_paths),
            vec!["/auth/login", "/auth/oidc/*", "/public/export/*"]
        );
        assert!(validate_anon_paths(&c).is_ok());
    }

    #[test]
    fn one_layer_on_non_tail_wildcard_fails_fast() {
        // 标在无尾 "/*" 的条目上是无效标记（WARN 本就不会点名它）→ 装配期报错。
        let c: Config = serde_yaml::from_str(
            "tenant:\n\
             \x20 enable: true\n\
             \x20 anonymous_paths:\n\
             \x20   - { path: \"/health\", one_layer: true }\n",
        )
        .unwrap();
        let err = validate_anon_paths(&c).unwrap_err();
        assert!(err.contains("/health"), "{err}");
        assert!(err.contains("one_layer"), "{err}");
    }

    #[test]
    fn one_layer_tail_check_is_segment_wise_like_the_matcher() {
        // 评审 P2-2：校验用段级口径（末段 == "*"）与匹配层「忽略空段」对齐——
        // `/x/*/` 在运行期就是严格一层，标 one_layer 合法（此前裸后缀口径会误拒）。
        let c: Config = serde_yaml::from_str(
            "tenant:\n\
             \x20 enable: true\n\
             \x20 anonymous_paths:\n\
             \x20   - { path: \"/idp/*/\", one_layer: true }\n\
             \x20   - { path: \"/a//*\", one_layer: true }\n",
        )
        .unwrap();
        assert!(validate_anon_paths(&c).is_ok());
        // 末段不是 "*" 的仍然拒。
        let bad: Config = serde_yaml::from_str(
            "auth:\n\
             \x20 jwt_secret: \"s\"\n\
             \x20 anonymous_paths:\n\
             \x20   - { path: \"/a/**\", one_layer: true }\n",
        )
        .unwrap();
        assert!(validate_anon_paths(&bad).unwrap_err().contains("/a/**"));
    }

    #[test]
    fn anon_path_object_unknown_key_and_missing_path_fail_loud() {
        // 评审 P2-4：untagged 会把 `one_layr` 静默吃成 one_layer=false（用户以为已消音）——
        // 手写 Deserialize 后未知键直接报错，并给出合法形态。
        let err = serde_yaml::from_str::<Config>(
            "auth:\n\
             \x20 jwt_secret: \"s\"\n\
             \x20 anonymous_paths:\n\
             \x20   - { path: \"/idp/*\", one_layr: true }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("one_layr"), "{err}");
        assert!(err.contains("one_layer"), "{err}");

        // 对象形态缺 path → 报错（untagged 时代会退化成模糊的 did-not-match-any-variant）。
        let err = serde_yaml::from_str::<Config>(
            "tenant:\n\
             \x20 enable: true\n\
             \x20 anonymous_paths:\n\
             \x20   - { one_layer: true }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("path"), "{err}");

        // 值类型写错（one_layer 给字符串）也要有人话报错。
        let err = serde_yaml::from_str::<Config>(
            "tenant:\n\
             \x20 enable: true\n\
             \x20 anonymous_paths:\n\
             \x20   - { path: \"/idp/*\", one_layer: \"yes\" }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("one_layer") || err.contains("boolean"),
            "{err}"
        );
    }

    #[test]
    fn oidc_section_absent_is_none_and_tenant_anon_defaults_empty() {
        let c: Config = serde_yaml::from_str("tenant:\n  enable: true\n").unwrap();
        assert!(c.oidc.is_none());
        assert!(c.tenant.anonymous_paths.is_empty());
    }

    #[test]
    fn duration_hours_and_days() {
        assert_eq!(parse_duration("720h").unwrap().as_secs(), 2_592_000);
        assert_eq!(parse_duration("2d").unwrap().as_secs(), 172_800);
    }

    #[test]
    fn bad_yaml_errors() {
        let dir = std::env::temp_dir().join(format!("ojcfgbad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cfg.yaml"), "server: [broken").unwrap();
        assert!(load_from(&dir, Some("cfg.yaml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `smtp:` 段（mail 轴，spec 2026-09-15）：**一段两用**——整段序列化交给 `oj-mail`
    /// 插件（含凭据）建 transport；宿主另只吸收非密钥面（`MailConfig::from_value`）。
    /// 断言重点是「过线的 JSON 形态」：空字段必须**不出现**（`null` 会让插件的强类型
    /// `Deserialize` 直接报错），凭据/白名单/并发参数原样带过去。
    #[test]
    fn smtp_section_parses_profiles_and_serializes_for_plugin() {
        let dir = std::env::temp_dir().join(format!("ojcfgsmtp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            concat!(
                "smtp:\n",
                "  workers: 2\n",
                "  queue_capacity: 8\n",
                "  default:\n",
                "    host: smtp.example.com\n",
                "    port: 465\n",
                "    tls: tls\n",
                "    mechanism: login\n",
                "    user: api@x.com\n",
                "    pass: SECRET\n",
                "    timeout: 30\n",
                "    allowed_from: [noreply@x.com]\n",
                "    allowed_recipients: [\"@x.com\", \"@partner.com\"]\n",
                "  oauth:\n",
                "    host: smtp.other.com\n",
                "    port: 587\n",
                "    tls: starttls\n",
                "    mechanism: xoauth2\n",
                "    user: api@y.com\n",
                "    xoauth2: { access_token: \"ya29.TOKEN\" }\n",
                "    allowed_from: [alert@y.com]\n",
                "    allowed_recipients: [\"@y.com\"]\n",
                "  mock:\n",
                "    host: localhost\n",
                "    port: 25\n",
                "    tls: none\n",
                "    allow_none_tls: true\n",
                "    mechanism: login\n",
                "    file_transport: /tmp/oj-mail-eml\n",
                "    allowed_from: [noreply@x.com]\n",
                "    allowed_recipients: [\"@x.com\"]\n",
            ),
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let s = c.smtp.expect("smtp 段存在");
        assert_eq!((s.workers, s.queue_capacity), (Some(2), Some(8)));
        assert_eq!(s.profiles.len(), 3);
        let d = &s.profiles["default"];
        assert_eq!(d.host.as_deref(), Some("smtp.example.com"));
        assert_eq!(d.port, Some(465));
        assert_eq!(d.tls.as_deref(), Some("tls"));
        assert_eq!(d.allow_none_tls, None); // 未写 = 默认拒绝（插件侧 fail-closed）
        assert_eq!(d.mechanism.as_deref(), Some("login"));
        assert_eq!(d.user.as_deref(), Some("api@x.com"));
        assert_eq!(d.pass.as_deref(), Some("SECRET"));
        assert_eq!(d.timeout, Some(30));
        assert_eq!(d.allowed_from, vec!["noreply@x.com".to_string()]);
        assert_eq!(
            d.allowed_recipients,
            vec!["@x.com".to_string(), "@partner.com".to_string()]
        );
        assert!(d.xoauth2.is_none() && d.file_transport.is_none());
        let o = &s.profiles["oauth"];
        assert_eq!(
            o.xoauth2.as_ref().and_then(|x| x.access_token.as_deref()),
            Some("ya29.TOKEN")
        );
        assert!(o.xoauth2.as_ref().unwrap().refresh_token.is_none());
        let m = &s.profiles["mock"];
        assert_eq!(m.file_transport.as_deref(), Some("/tmp/oj-mail-eml"));
        assert_eq!(m.allow_none_tls, Some(true));

        // 过线 JSON（= 给插件的 cfg）：键名与插件 schema 一致（workers/queue_capacity +
        // 每个 profile 一个键），空字段省略而非 null。
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["workers"], 2);
        assert_eq!(j["queue_capacity"], 8);
        assert_eq!(j["default"]["host"], "smtp.example.com");
        assert_eq!(j["default"]["pass"], "SECRET");
        assert_eq!(j["default"]["timeout"], 30);
        assert_eq!(j["mock"]["file_transport"], "/tmp/oj-mail-eml");
        assert_eq!(j["mock"]["allow_none_tls"], true);
        assert!(j["mock"].get("user").is_none(), "{j}");
        assert!(j["mock"].get("pass").is_none(), "{j}");
        assert!(j["default"].get("allow_none_tls").is_none(), "{j}");
        assert!(j["default"].get("file_transport").is_none(), "{j}");
        assert!(j["oauth"]["xoauth2"].get("refresh_token").is_none(), "{j}");
        // `allowed_*` 属宿主校验面（插件忽略未知字段），但**必须在同一份过线 JSON 里**
        // ——宿主与插件的配置面同源（`MailConfig::from_value` 吃同一份），否则两边分叉。
        assert_eq!(j["default"]["allowed_from"][0], "noreply@x.com");
        assert_eq!(j["default"]["allowed_recipients"][1], "@partner.com");

        // 形态错误在**装配期**暴露（不静默变成「无白名单」）：port 非数字 / 段非映射。
        std::fs::write(
            dir.join("bad.yaml"),
            "smtp:\n  default:\n    host: h\n    port: not-a-number\n",
        )
        .unwrap();
        assert!(load_from(&dir, Some("bad.yaml")).is_err());
        assert!(serde_yaml::from_str::<Config>("smtp: [1, 2]\n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// smtp 段缺省 → None（`mail.*` 报未配置）；空段 `smtp: {}` → Some（零 profile，
    /// 装配层按「空 cfg」处理为未配置）。
    #[test]
    fn smtp_section_absent_is_none_and_empty_is_some() {
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.smtp.is_none());
        let c: Config = serde_yaml::from_str("smtp: {}\n").unwrap();
        let s = c.smtp.expect("段存在");
        assert!(s.profiles.is_empty());
        assert_eq!(serde_json::to_value(&s).unwrap().to_string(), "{}");
    }

    #[test]
    fn broker_cfg_defaults_and_parse() {
        // 未配置 → 空 map（退化为进程内 Bus）
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.broker.is_empty());
        // 旧单对象写法 `broker: { kind: kafka }` → 兼容包成 { default: { kind: kafka } }
        let dir = std::env::temp_dir().join(format!("ojcfgbr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "broker:\n  kind: kafka\n  brokers: [127.0.0.1:9092, k2:9092]\n  topic_prefix: ev\n  group: g1\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let b = c.broker.get("default").expect("default profile");
        assert_eq!(b.kind, "kafka");
        assert_eq!(b.brokers, vec!["127.0.0.1:9092", "k2:9092"]);
        assert_eq!(b.topic_prefix.as_deref(), Some("ev"));
        assert_eq!(b.group.as_deref(), Some("g1"));
        assert!(b.url.is_none());
        // 命名多源 map 原样保留
        std::fs::write(
            dir.join("cfg.yaml"),
            "broker:\n  default: { kind: local }\n  prod: { kind: kafka, brokers: [k:9092] }\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.broker.get("default").unwrap().kind, "local");
        assert_eq!(c.broker.get("prod").unwrap().kind, "kafka");
        // 空段缺省
        let d = BrokerCfg::default();
        assert_eq!(d.kind, "");
        assert!(
            d.brokers.is_empty()
                && d.url.is_none()
                && d.group.is_none()
                && d.topic_prefix.is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
