use super::*;

/// http_sso 校验矩阵（v0.1.61）：kind 白名单 / form 必 success_when / cas2 必两 URL /
/// name 禁 `:` / 缺省拒 http，allow_http 放行。
#[test]
fn validate_http_sso_matrix() {
    let mut cfg = Config::default();
    assert!(validate_http_sso(&cfg).is_ok(), "段缺省 = 不校验");
    let mut p = HttpSsoProvider {
        kind: "form".into(),
        login_url: "https://sso.corp.com/login".into(),
        ..Default::default()
    };
    let mut m = std::collections::HashMap::new();
    m.insert("corp".to_string(), p.clone());
    cfg.http_sso = Some(m);
    assert!(
        validate_http_sso(&cfg).is_err(),
        "form 缺 success_when 必须拒绝"
    );
    p.success_when
        .insert("message".to_string(), serde_json::json!(""));
    let mut m2 = std::collections::HashMap::new();
    m2.insert("corp".to_string(), p.clone());
    cfg.http_sso = Some(m2);
    assert!(validate_http_sso(&cfg).is_ok());
    let mut m3 = std::collections::HashMap::new();
    m3.insert("ba:d".to_string(), p.clone());
    cfg.http_sso = Some(m3);
    assert!(validate_http_sso(&cfg).is_err(), "name 含 `:` 必须拒绝");
    let p2 = HttpSsoProvider {
        kind: "cas2".into(),
        login_url: "http://sso.corp.com/login".into(), // http 且未 allow_http
        ..Default::default()
    };
    let mut m4 = std::collections::HashMap::new();
    m4.insert("corp".to_string(), p2.clone());
    cfg.http_sso = Some(m4);
    assert!(validate_http_sso(&cfg).is_err(), "非 https 缺省拒绝");
    let p3 = HttpSsoProvider {
        allow_http: true,
        ..p2
    };
    let mut m5 = std::collections::HashMap::new();
    m5.insert("corp".to_string(), p3.clone());
    cfg.http_sso = Some(m5);
    assert!(
        validate_http_sso(&cfg).is_err(),
        "cas2 缺两 URL 必须拒绝（https 放行后仍要报）"
    );
    let p4 = HttpSsoProvider {
        cas_validate_url: "https://sso.corp.com/serviceValidate".into(),
        callback_url: "https://app.corp.com/v1/api/sso/cas/callback/".into(),
        ..p3.clone()
    };
    let mut m6 = std::collections::HashMap::new();
    m6.insert("corp".to_string(), p4);
    cfg.http_sso = Some(m6);
    assert!(
        validate_http_sso(&cfg).is_ok(),
        "allow_http + cas2 两 URL 齐全 → 放行"
    );
}

/// PR-4：堆限额默认 256 MiB，且 < 32 MiB（含 0）装配校验 fail-fast。
#[test]
fn js_heap_limit_default_and_validate() {
    let c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
    assert_eq!(c.server.js_heap_limit_bytes, 256 * 1024 * 1024);
    assert!(validate_server_limits(&c).is_ok(), "默认值必须过校验");
    for bad in [0u64, 1, 1024, 32 * 1024 * 1024 - 1] {
        let mut c2 = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
        c2.server.js_heap_limit_bytes = bad;
        let e = validate_server_limits(&c2).expect_err("低于下限必须拒绝");
        assert!(e.contains("below the minimum"), "{e}");
    }
}

#[test]
fn load_with_extra_top_is_full_mapping() {
    let dir = std::env::temp_dir();
    let p = dir.join("oj-test-extra-config.yaml");
    std::fs::write(
        &p,
        r#"
server: { host: "127.0.0.1", port: 9778 }
vars: { PORT: 3000 }
cache:
  backend: memory
  ttl: 60
"#,
    )
    .unwrap();
    let loaded = load_with_extra(&p, &dir).expect("load_with_extra");
    // vars 数字标量仍按字面读成串（既有宽松行为不可回归）：
    assert_eq!(
        loaded.config.vars.get("PORT").map(String::as_str),
        Some("3000")
    );
    // top = 全量顶层 mapping（spec 裁定：已知 + 未知段都在查找面，
    // 插件 config key 可撞宿主已知段名）：
    assert!(loaded.top.get("cache").is_some());
    assert!(loaded.top.get("server").is_some());
    assert!(loaded.top.get("vars").is_some());
    std::fs::remove_file(&p).ok();
}

#[test]
fn known_top_level_keys_covers_all_typed_fields() {
    // 对账单测（评审 H1/S3）：每个已知键都能被 Config 解析接受，
    // 且列表不含重复。新增 Config 字段必须同步此表。
    let keys = known_top_level_keys();
    assert_eq!(
        keys.len(),
        keys.iter().collect::<std::collections::HashSet<_>>().len()
    );
    for k in keys {
        // `db_access` / `db_client` / `db_session` / `audit` 是 mapping 类型（不接受 null），
        // 其余已知段用 `null` 探测即可（取默认值且不落入 extra）。
        let yaml = if *k == "db_access"
            || *k == "db_client"
            || *k == "db_session"
            || *k == "audit"
            || *k == "mgmt"
        {
            format!("{k}: {{}}\n")
        } else {
            format!("{k}: null\n")
        };
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let extra = split_extra(&v);
        assert!(extra.get(*k).is_none(), "known key '{k}' leaked into extra");
    }
}

/// PR-9：require_signed_claim 装配校验矩阵。
#[test]
fn tenant_binding_validation_matrix() {
    let mut c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
    // 关闭 → 恒过
    assert!(validate_tenant_binding(&c, false).is_ok());
    // 开启但无 auth / 未 enable → 各自拒绝（文案点名）
    c.tenant.require_signed_claim = true;
    let e = validate_tenant_binding(&c, true).unwrap_err();
    assert!(e.contains("tenant.enable=true"), "{e}");
    c.tenant.enable = true;
    let e = validate_tenant_binding(&c, false).unwrap_err();
    assert!(e.contains("needs an auth guard"), "{e}");
    // 齐备 → 过
    assert!(validate_tenant_binding(&c, true).is_ok());
}

#[test]
fn defaults_when_no_file() {
    let c = load_from(std::path::Path::new("/nonexistent-dir"), None).unwrap();
    assert_eq!((c.server.host.as_str(), c.server.port), ("localhost", 9778));
    assert!(c.mounts.is_empty());
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

/// `platform:` 段（ojm 地基）：三键形状解析 + 未知键拒绝 + 缺省全空（fail-closed）。
#[test]
fn platform_section_parse_and_deny_unknown() {
    let dir = std::env::temp_dir().join(format!("oj-platcfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        "platform:\n  \
             modules: [\"ojm-installer\"]\n  \
             root: ./platform-modules\n  \
             trusted_paths: [\"./config.yaml\", \"./dist\"]\n  \
             exec:\n    \
             ojm-installer:\n      \
             allow:\n        - \"tar -xzf ${path:archive} -C ${path:dest}\"\n      \
             env: [\"OJ_SECRET_KEY\"]\n",
    )
    .unwrap();
    let c = load_from(&dir, None).unwrap();
    let p = c.platform.expect("platform section parsed");
    assert_eq!(p.modules, vec!["ojm-installer".to_string()]);
    assert_eq!(p.root.as_deref(), Some("./platform-modules"));
    assert_eq!(
        p.trusted_paths,
        vec!["./config.yaml".to_string(), "./dist".to_string()]
    );
    let allow = p.exec.get("ojm-installer").expect("exec entry");
    assert_eq!(allow.allow.len(), 1);
    assert!(allow.allow[0].starts_with("tar -xzf "));
    assert_eq!(allow.env, vec!["OJ_SECRET_KEY".to_string()]);

    // 未知键拒绝（deny_unknown_fields）：段级与 exec 条目级都点名。
    std::fs::write(dir.join("config.yaml"), "platform:\n  unknown: 1\n").unwrap();
    let e = load_from(&dir, None).unwrap_err();
    assert!(e.contains("unknown"), "{e}");
    std::fs::write(
        dir.join("config.yaml"),
        "platform:\n  exec:\n    m:\n      allow: [\"x\"]\n      extra: 1\n",
    )
    .unwrap();
    let e = load_from(&dir, None).unwrap_err();
    assert!(e.contains("extra"), "{e}");

    // 空映射 = 合法且全空（缺省省略 = 全 deny 的 fail-closed 面）；
    // 显式 null（`platform:` 空键）对齐 `auth:`/`fs:` 惯例解为 None（未启用）。
    std::fs::write(dir.join("config.yaml"), "platform: {}\n").unwrap();
    let c = load_from(&dir, None).unwrap();
    let p = c.platform.expect("empty mapping is Some");
    assert!(p.modules.is_empty() && p.trusted_paths.is_empty() && p.exec.is_empty());
    std::fs::write(dir.join("config.yaml"), "platform:\n").unwrap();
    let c = load_from(&dir, None).unwrap();
    assert!(c.platform.is_none());
    // 段缺省 = None。
    std::fs::write(dir.join("config.yaml"), "server: { host: h }\n").unwrap();
    let c = load_from(&dir, None).unwrap();
    assert!(c.platform.is_none());
    std::fs::remove_dir_all(&dir).ok();
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

/// `db_access:` 段（v0.1.89）：与 `db:` 分离，缺席/空键落默认（off 档 = 零行为变化），
/// 但**写错必须 fail-loud**（非法档位 / 未知键 / 越界 max_rows 都不静默吞）。
#[test]
fn db_access_section_parses_and_rejects_bad_values() {
    let dir = std::env::temp_dir().join(format!("ojcfgdba-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |s: &str| {
        std::fs::write(dir.join("config.yaml"), s).unwrap();
        load_from(&dir, None)
    };
    // 缺席 / 空键 → 默认（off）
    let c = write("db:\n  default: sqlite://a.db\nauth:\n  jwt_secret: s\n").unwrap();
    assert_eq!(
        c.db_access.defaults.enforce,
        crate::bridge::db_policy::Enforce::Off
    );
    assert!(c.db_access.by_db.is_empty());
    let c = write("db_access:\ndb:\n  default: sqlite://a.db\nauth:\n  jwt_secret: s\n").unwrap();
    assert!(c.db_access.by_db.is_empty());
    // 正常解析 + 逐库条目
    let c = write(concat!(
        "db:\n  default: sqlite://a.db\n  analytics: sqlite://b.db\n",
        "db_access:\n",
        "  defaults:\n    enforce: warn\n",
        "  analytics:\n    read_only: true\n    max_rows: 50\n",
        "    allow:\n      roles: [admin]\n",
        "auth:\n  jwt_secret: s\n",
    ))
    .unwrap();
    assert_eq!(
        c.db_access.defaults.enforce,
        crate::bridge::db_policy::Enforce::Warn
    );
    let a = c.db_access.policy_for("analytics");
    assert!(a.read_only && a.max_rows == Some(50));
    assert_eq!(a.allow.roles.as_deref(), Some(&["admin".to_string()][..]));
    // 未声明的库走 defaults
    assert_eq!(
        c.db_access.policy_for("default").enforce,
        crate::bridge::db_policy::Enforce::Warn
    );
    // 非法档位 fail-loud
    let e =
        write("db_access:\n  defaults:\n    enforce: maybe\nauth:\n  jwt_secret: s\n").unwrap_err();
    assert!(e.contains("enforce"), "{e}");
    // 未知键 fail-loud
    let e = write("db_access:\n  analytics:\n    nope: 1\nauth:\n  jwt_secret: s\n").unwrap_err();
    assert!(e.contains("nope"), "{e}");
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
    std::fs::write(
        dir.join("cfg.yaml"),
        concat!(
            "server:\n  host: 0.0.0.0\n  port: 9000\n  timeout: 5s\n  pool_size: 2\n",
            "mounts:\n  - { prefix: /xapi, api: src }\n",
            "db:\n  default: sqlite://db.sqlite\n",
            "redis:\n  default: redis://127.0.0.1:6379/1\n",
        ),
    )
    .unwrap();
    let c = load_from(&dir, Some("cfg.yaml")).unwrap();
    assert_eq!(c.server.host, "0.0.0.0");
    assert_eq!(c.mounts[0].prefix, "/xapi");
    assert_eq!(c.mounts[0].api.as_deref(), Some("src"));
    assert_eq!(c.db["default"], "sqlite://db.sqlite");
    assert_eq!(c.redis.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mounts_parse_and_default() {
    // 缺省为空表。
    let c = load_from(std::path::Path::new("/nonexistent"), None).unwrap();
    assert!(c.mounts.is_empty());
    // round-trip：api + web + 配对 + headers（BTreeMap）+ spa。
    let dir = std::env::temp_dir().join(format!("ojcfgsites-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("cfg.yaml"),
        concat!(
            "mounts:\n",
            "  - { prefix: /v1/api, api: src }\n",
            "  - prefix: /docs\n",
            "    web: d1\n",
            "    spa: true\n",
            "    headers: { b: 2, a: 1 }\n",
        ),
    )
    .unwrap();
    let c = load_from(&dir, Some("cfg.yaml")).unwrap();
    assert_eq!(c.mounts.len(), 2);
    assert_eq!(c.mounts[0].prefix, "/v1/api");
    assert_eq!(c.mounts[0].api.as_deref(), Some("src"));
    assert!(c.mounts[0].web.is_none() && c.mounts[0].spa.is_none());
    assert_eq!(c.mounts[1].web.as_deref(), Some("d1"));
    assert_eq!(c.mounts[1].spa, Some(true));
    // BTreeMap 输出定序（键序与书写序无关）。
    assert_eq!(
        c.mounts[1].headers.keys().collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    // 根 `/` 前缀合法（规则 4 的解析归装配期，这里只钉解析不拒）。
    std::fs::write(
        dir.join("cfg.yaml"),
        "mounts:\n  - { prefix: /, web: d2 }\n",
    )
    .unwrap();
    assert_eq!(
        load_from(&dir, Some("cfg.yaml")).unwrap().mounts[0].prefix,
        "/"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 规则 1/2 校验矩阵：空串、api/web 恰好其一、同类重复、api+web 配对、spa 约束。
#[test]
fn mounts_validation_matrix() {
    let m = |prefix: &str, api: Option<&str>, web: Option<&str>, spa: Option<bool>| MountConf {
        prefix: prefix.into(),
        api: api.map(Into::into),
        web: web.map(Into::into),
        spa,
        headers: Default::default(),

        ..Default::default()
    };
    // api/web 双写 → Err；双缺 → Err。
    assert!(validate_mounts(&[m("/p", Some("a"), Some("w"), None)]).is_err());
    assert!(validate_mounts(&[m("/p", None, None, None)]).is_err());
    // 值空串 → Err（规则 1）。
    for bad in [
        m("", Some("a"), None, None),
        m("/p", Some(""), None, None),
        m("/p", None, Some("  "), None),
    ] {
        let e = validate_mounts(&[bad]).unwrap_err();
        assert!(e.contains("must not be empty"), "{e}");
    }
    // api+web 同 prefix 配对 → Ok（api 优先）。
    assert!(
        validate_mounts(&[
            m("/p", Some("a"), None, None),
            m("/p", None, Some("w"), None)
        ])
        .is_ok()
    );
    // 同类重复 → Err（含尾斜杠归一口径）。
    assert!(
        validate_mounts(&[
            m("/docs", None, Some("a"), None),
            m("/docs/", None, Some("b"), None)
        ])
        .is_err()
    );
    assert!(
        validate_mounts(&[
            m("/a", Some("x"), None, None),
            m("/a", Some("y"), None, None)
        ])
        .is_err()
    );
    // spa: true 同 prefix 有 api → Err；配对无 api / 无 spa → Ok。
    assert!(
        validate_mounts(&[
            m("/p", Some("a"), None, None),
            m("/p", None, Some("w"), Some(true))
        ])
        .is_err()
    );
    assert!(validate_mounts(&[m("/p", None, Some("w"), Some(true))]).is_ok());
    // spa 标在 api 挂载上 → 无意义标记，Err（不让配置撒谎）。
    assert!(validate_mounts(&[m("/p", Some("a"), None, Some(true))]).is_err());
    // 嵌套（/v1 web spa + /v1/api api）合法：最长前缀命中使回落吞不掉 api 路径。
    assert!(
        validate_mounts(&[
            m("/v1", None, Some("w"), Some(true)),
            m("/v1/api", Some("a"), None, None)
        ])
        .is_ok()
    );
    // 根 `/` api 挂载合法（规则 4 允许，此处钉不误拒）。
    assert!(validate_mounts(&[m("/", Some("a"), None, None)]).is_ok());
}

/// 规则 3：五旧键任一出现 → 报错 + 迁移 YAML（spa 全 web 条目 / headers 保留 /
/// api_prefix 只给提示 / `base` alias 同捕获）。
#[test]
fn legacy_server_keys_error_with_migration_yaml() {
    let dir = std::env::temp_dir().join(format!("ojcfglegacy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = concat!(
        "server:\n",
        "  app_prefix: /site\n",
        "  app_path: public\n",
        "  app_spa_fallback: true\n",
        "  static_sites:\n",
        "    - { prefix: /docs, path: d1, headers: { csp: 'default-src self' } }\n",
        "  api_prefix: /v1/api\n",
    );
    std::fs::write(dir.join("cfg.yaml"), cfg).unwrap();
    let e = load_from(&dir, Some("cfg.yaml")).unwrap_err();
    // 逐键两列 + 等价块 + 文档指引。
    assert!(e.contains("removed `server.*` site keys"), "{e}");
    assert!(
        e.contains("server.app_path + server.app_prefix \"/site\""),
        "{e}"
    );
    assert!(e.contains("server.static_sites[0]"), "{e}");
    assert!(e.contains("server.api_prefix \"/v1/api\""), "{e}");
    // F1：spa 加到每一个 web 条目（主站点 + static_sites）——旧版是全局开关
    //（只数 mounts 块内 4 空格缩进的条目行，per-key 说明行里也提到 spa）。
    assert_eq!(e.matches("\n    spa: true").count(), 2, "{e}");
    assert!(
        e.contains("- prefix: \"/site\"") && e.contains("web: \"public\""),
        "{e}"
    );
    assert!(
        e.contains("web: \"d1\"") && e.contains("csp: default-src self"),
        "{e}"
    );
    assert!(e.contains("docs/user-manual.md"), "{e}");
    // `base` alias 同捕获（api_prefix 的旧键名）。
    std::fs::write(dir.join("cfg.yaml"), "server:\n  base: /xapi\n").unwrap();
    let e = load_from(&dir, Some("cfg.yaml")).unwrap_err();
    assert!(e.contains("api_prefix") && e.contains("/xapi"), "{e}");
    // 干净配置不受影响。
    std::fs::write(
        dir.join("cfg.yaml"),
        "server:\n  port: 1\nmounts:\n  - { prefix: /, web: d }\n",
    )
    .unwrap();
    assert!(load_from(&dir, Some("cfg.yaml")).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mounts_headers_and_new_server_keys_parse() {
    let dir = std::env::temp_dir().join(format!("ojcfgkeys-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("cfg.yaml"),
            "mounts:\n  - { prefix: /docs, web: d1, headers: { csp: 'default-src self' } }\nserver:\n  blob_upload_max_bytes: 104857600\n  response_headers: { x-frame-options: DENY, referrer-policy: no-referrer }\n  route_timeouts:\n    - { pattern: /v1/api/convert/**, timeout: 5m }\n",
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
        c.mounts[0].headers.get("csp").map(String::as_str),
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
    // MCP/RPC 缺省：不挂端点（R8 独立门，均默认关）。
    assert!(ServerCfg::default().mcp.is_none());
    assert!(!ServerCfg::default().rpc.enabled);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mcp_and_rpc_sections_parse() {
    // Given 完整 server.mcp 段 + server.rpc.enabled
    // When 解析
    // Then 字段全落；缺省段不拦解析
    let dir = std::env::temp_dir().join(format!("ojcfgmcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("cfg.yaml"),
            "mounts:\n  - { prefix: /, web: d }\nserver:\n  mcp:\n    enabled: true\n    modules: [user, wiki]\n    allow_anonymous: true\n  rpc:\n    enabled: true\n",
        )
        .unwrap();
    let c = load_from(&dir, Some("cfg.yaml")).unwrap();
    let mcp = c.server.mcp.expect("mcp 段应解析出");
    assert!(mcp.enabled);
    assert_eq!(
        mcp.modules,
        Some(vec!["user".to_string(), "wiki".to_string()])
    );
    assert!(mcp.allow_anonymous);
    assert!(c.server.rpc.enabled);
    // Given 只写 enabled（modules/allow_anonymous 缺省）
    std::fs::write(
        dir.join("cfg2.yaml"),
        "mounts:\n  - { prefix: /, web: d }\nserver:\n  mcp:\n    enabled: true\n",
    )
    .unwrap();
    let c2 = load_from(&dir, Some("cfg2.yaml")).unwrap();
    let mcp2 = c2.server.mcp.expect("mcp 段应解析出");
    assert!(mcp2.enabled);
    assert_eq!(mcp2.modules, None);
    assert!(!mcp2.allow_anonymous);
    assert!(!c2.server.rpc.enabled, "rpc 缺省 false");
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
    let c: Config = serde_yaml::from_str("tenant:\n  enable: true\n  sql_guard: true\n").unwrap();
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
        serde_yaml::from_str("tenant:\n  sql_guard: true\n  shared_allow: [dict, geo]\n").unwrap();
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
        d.brokers.is_empty() && d.url.is_none() && d.group.is_none() && d.topic_prefix.is_none()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn find_path_explicit_and_search() {
    // 显式 -c：存在 → Some（绝对/相对均可），缺失 → Err。
    let dir = std::env::temp_dir().join(format!("oj-findpath-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("c.yaml");
    std::fs::write(
        &cfg,
        "server: { port: 1 }
",
    )
    .unwrap();
    assert_eq!(
        find_path(Some(cfg.to_str().unwrap())).unwrap(),
        Some(cfg.clone())
    );
    assert!(find_path(Some(dir.join("none.yaml").to_str().unwrap())).is_err());
    // 未找到：返回值合法（None 或某处命中——不假定测试机 HOME 状态）。
    let _ = find_path(None).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
