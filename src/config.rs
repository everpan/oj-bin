//! oj 配置（cli2.md 预案 schema）：server(host/port/root) + db/redis 的 URL 风格 DSN map。
//! 旧三层 env 叠加已删（预案即单文件）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    /// API 基础路由前缀（如 "/v1/api"）；CLI `-b` 显式给出时覆盖。
    pub base: String,
    /// 静态站点根目录（相对 config 所在目录）；None → 不开静态服务。
    pub root: Option<String>,
    /// 时长字符串（如 "30s"），parse_duration 解析。
    pub timeout: String,
    pub pool_size: u32,
    /// 上传体积上限（字节；超出 413）。axum 层再乘 2 做硬顶。
    pub max_upload_bytes: u64,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 778,
            base: "/v1/api".into(),
            root: None,
            timeout: "30s".into(),
            pool_size: 4,
            max_upload_bytes: 10 * 1024 * 1024,
        }
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
}

impl Default for TenantCfg {
    fn default() -> Self {
        Self { enable: false, header_key: "X-TENANT-ID".into() }
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
    /// 用户表名（login 查询用；标识符白名单校验）。
    pub user_table: String,
}

impl Default for AuthCfg {
    fn default() -> Self {
        Self {
            jwt_secret: String::new(),
            signing_method: "HS256".into(),
            access_token_duration: "60s".into(),
            refresh_token_duration: "720h".into(),
            anonymous_paths: Vec::new(),
            user_table: "users".into(),
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
    /// None = 不启用 blob（blob 全局/上传/下载路由均不挂）。
    pub blob: Option<BlobSection>,
    /// None = 不启用 ES（es.* op 报 "es not configured"）。
    pub es: Option<EsCfg>,
    /// None = 不启用分布式 broker（事件总线退化为进程内 Bus）。
    pub broker: Option<BrokerCfg>,
    /// plugins 清单（显式给出 → 严格按清单装配，缺文件/版本不符 fail fast；
    /// None = 缺省扫描 plugins_dir 全部加载）。
    pub plugins: Option<Vec<String>>,
    /// plugins 目录（相对 config_dir；None = 走 OJ_PLUGINS_DIR > <exe>/plugins > workspace 后备）。
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
        assert_eq!((c.server.host.as_str(), c.server.port), ("localhost", 778));
        assert_eq!(c.server.base, "/v1/api");
        assert!(c.server.root.is_none());
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
            "server:\n  host: 0.0.0.0\n  port: 9000\n  base: /xapi\n  root: public\n  timeout: 5s\n  pool_size: 2\n",
            "db:\n  default: sqlite://db.sqlite\n",
            "redis:\n  default: redis://127.0.0.1:6379/1\n",
        )).unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert_eq!(c.server.host, "0.0.0.0");
        assert_eq!(c.server.base, "/xapi");
        assert_eq!(c.server.root.as_deref(), Some("public"));
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
        std::fs::write(dir.join("cfg.yaml"), "tenant:\n  enable: true\n  header_key: X-ACCT\n").unwrap();
        let c = load_from(&dir, Some("cfg.yaml")).unwrap();
        assert!(c.tenant.enable && c.tenant.header_key == "X-ACCT");
        let _ = std::fs::remove_dir_all(&dir);
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
        std::fs::write(dir.join("cfg.yaml"), "blob:\n  driver: local\n  root: up2\n").unwrap();
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
        std::fs::write(dir.join("cfg.yaml"), "es:\n  endpoint: http://127.0.0.1:9200/\n").unwrap();
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
        assert_eq!(a.user_table, "users");
        assert!(a.anonymous_paths.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(d.brokers.is_empty() && d.url.is_none() && d.group.is_none() && d.topic_prefix.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
