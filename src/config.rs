//! oj 配置（cli2.md 预案 schema）：server(host/port/app_path) + db/redis 的 URL 风格 DSN map。
//! 旧三层 env 叠加已删（预案即单文件）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// 时长字符串（如 "30s"），parse_duration 解析。
    pub timeout: String,
    pub pool_size: u32,
    /// 上传体积上限（字节；超出 413）。axum 层再乘 2 做硬顶。
    pub max_upload_bytes: u64,
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
            timeout: "30s".into(),
            pool_size: 4,
            max_upload_bytes: 10 * 1024 * 1024,
            logs_dir: None,
            logs_max_m: 100,
            logs_keep_files: 10,
            console_log: false,
            public_key_path: "".into(),
            certificate_path: "".into(),
            grace_days: Some(30),
            migrate_on_start: None,
            ownership_guard: None,
        }
    }
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
#[serde(default)]
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
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct BrokerCfg {
    pub kind: String,
    #[serde(default)]
    pub brokers: Vec<String>,
    pub url: Option<String>,
    pub group: Option<String>,
    pub topic_prefix: Option<String>,
}

/// 多租户注入（OJ-3）：enable 后 handle() 从 header 提取租户 id 注入 http.tenantId。
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TenantCfg {
    pub enable: bool,
    pub header_key: String,
    /// 浏览器跳转腿豁免（去 base 后路径；尾 "/*" 一层通配）——OIDC 回跳带不了自定义头。
    pub anonymous_paths: Vec<String>,
}

impl Default for TenantCfg {
    fn default() -> Self {
        Self {
            enable: false,
            header_key: "X-TENANT-ID".into(),
            anonymous_paths: Vec::new(),
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
    /// 免鉴权路径（去 base 后）；结尾 "/*" = 一层前缀通配。
    pub anonymous_paths: Vec<String>,
}

impl Default for AuthCfg {
    fn default() -> Self {
        Self {
            jwt_secret: String::new(),
            signing_method: "HS256".into(),
            access_token_duration: "60s".into(),
            refresh_token_duration: "720h".into(),
            anonymous_paths: Vec::new(),
        }
    }
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
}

impl Default for TasksCfg {
    fn default() -> Self {
        Self {
            dir: "tasks".into(),
            max: 64,
            stop_grace_secs: 30,
        }
    }
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
    pub db: HashMap<String, String>,
    /// name → redis URL（v0.1 warn 后用内存 KV）。
    pub redis: HashMap<String, String>,
    pub tenant: TenantCfg,
    /// None = 不启用鉴权（内置 /auth/* 与 Bearer 守卫均不挂）。
    pub auth: Option<AuthCfg>,
    /// None = 不启用 OIDC。
    pub oidc: Option<OidcSection>,
    /// None = 不启用 blob（blob 全局/上传/下载路由均不挂）。
    pub blob: Option<BlobSection>,
    /// None = 不启用 ES（es.* op 报 "es not configured"）。
    pub es: Option<EsCfg>,
    /// None = 不启用分布式 broker（事件总线退化为进程内 Bus）。
    pub broker: Option<BrokerCfg>,
    /// 插件声明（spec「plugins: 统一语义」一段三用）：键 = 要加载的插件名（非空即
    /// 严格模式，只装配列出的插件，沿用清单门禁）；值 = 插件 cfg，非空对象原样透传，
    /// 空对象跳过透传回落轴适配器。缺省/空 map = 扫描模式（加载 plugins_dir 全部）。
    /// 旧 list 写法 `plugins: [a, b]` 废弃（解析报错 fail-fast）。
    pub plugins: HashMap<String, serde_json::Value>,
    /// 命名 MQ 实例（spec 2026-09-07 §3）：kafkas.default = { brokers, group } →
    /// Kafka("default")。值 JSON 透传给 mq 插件（kind 由装配层按段来源注入）。
    /// 段缺省 = 不启用（registry 空 → Kafka/RabbitMQ(name) → undefined）。
    #[serde(default)]
    pub kafkas: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub rabbits: HashMap<String, serde_json::Value>,
    /// 长任务池（spec 2026-09-07 §6）：目录约定 task_{name}.* / {name}_task.*；
    /// 缺省段 = 默认值（dir "tasks"，目录不存在 = 无任务，不报错）。
    #[serde(default)]
    pub tasks: TasksCfg,
    /// WS 运行时（spec 2026-09-09 帧池）：闸门 / Worker 数 / 空闲退役。
    #[serde(default)]
    pub ws: WsCfg,
    /// plugins 目录（相对 config_dir；None = 走 OJ_PLUGINS_DIR > <exe>/plugins > <workspace_root>/bin/plugins 后备）。
    pub plugins_dir: Option<PathBuf>,
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
        // 未配置 → None（es.* 报 "es not configured"）
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.es.is_none());
        // es: 段存在 → Some(endpoint)；endpoint 原样保留（尾斜杠由 EsClient.url_for 剪除）
        let dir = std::env::temp_dir().join(format!("ojcfge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "es:\n  endpoint: http://127.0.0.1:9200/\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let e = c.es.expect("some");
        assert_eq!(e.endpoint, "http://127.0.0.1:9200/");
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
        assert_eq!(c.tenant.anonymous_paths, vec!["/oidc/*".to_string()]);
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

    #[test]
    fn broker_cfg_defaults_and_parse() {
        // 未配置 → None（退化为进程内 Bus）
        let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
        assert!(c.broker.is_none());
        // broker: 段存在 → Some(kind/brokers/...)；缺省 brokers 为空、prefix None
        let dir = std::env::temp_dir().join(format!("ojcfgbr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cfg.yaml"),
            "broker:\n  kind: kafka\n  brokers: [127.0.0.1:9092, k2:9092]\n  topic_prefix: ev\n  group: g1\n",
        )
        .unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        let b = c.broker.expect("some");
        assert_eq!(b.kind, "kafka");
        assert_eq!(b.brokers, vec!["127.0.0.1:9092", "k2:9092"]);
        assert_eq!(b.topic_prefix.as_deref(), Some("ev"));
        assert_eq!(b.group.as_deref(), Some("g1"));
        assert!(b.url.is_none());
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
