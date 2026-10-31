use super::*;

// ---- MCP 装配防线（spec 2026-10-30 R7/R9） ----

// Given mcp.enabled=true 且未装配 auth 插件且 allow_anonymous 缺省（false）
// When validate_mcp_gate
// Then Err 且文案含 "server.mcp" 与 "auth"
#[test]
fn mcp_without_auth_plugin_fails_fast() {
    let mcp = config::McpCfg {
        enabled: true,
        ..Default::default()
    };
    let err = validate_mcp_gate(&mcp, "mcp", false).unwrap_err();
    assert!(err.contains("server.mcp") && err.contains("auth"), "{err}");
    // Given 同上但 allow_anonymous=true，Then 装配通过（显式逃生门）。
    let mcp_anon = config::McpCfg {
        enabled: true,
        allow_anonymous: true,
        ..Default::default()
    };
    assert!(validate_mcp_gate(&mcp_anon, "mcp", false).is_ok());
    // Given auth 已装配，Then 任意 mcp 配置都通过。
    assert!(validate_mcp_gate(&mcp, "mcp", true).is_ok());
}

// Given rpc.enabled=true、无 auth、未显式 allow_anonymous（v0.1.110 终审 major）
// Then 同 mcp 口径拒启（/rpc 与 /mcp 共用执行链，门控不得一严一宽）
#[test]
fn rpc_without_auth_plugin_fails_fast() {
    let rpc = config::RpcCfg {
        enabled: true,
        ..Default::default()
    };
    let err = validate_mcp_gate(&rpc, "rpc", false).unwrap_err();
    assert!(err.contains("server.rpc") && err.contains("auth"), "{err}");
    let rpc_anon = config::RpcCfg {
        enabled: true,
        allow_anonymous: true,
    };
    assert!(validate_mcp_gate(&rpc_anon, "rpc", false).is_ok());
}

// Given mcp.enabled=false（缺省）
// Then 永远通过（无 auth 也不拦——端点本就不挂）
#[test]
fn mcp_disabled_never_blocks_assembly() {
    let mcp = config::McpCfg::default();
    assert!(validate_mcp_gate(&mcp, "mcp", false).is_ok());
}

// ---- U-18 批 T1.2：route_timeouts 装配期校验（纯函数） ----

#[test]
fn validate_route_timeouts_matrix() {
    let keys = vec![
        "/v1/api/a".to_string(),
        "/v1/api/orders/{id}".to_string(),
        "/v1/api/deep/x/y".to_string(),
    ];
    // Given pattern 命中（字面含 base / 单段 `*` 对 `{param}` / `**`），When 校验，Then Ok。
    for ok in ["/v1/api/a", "/v1/api/orders/*", "/v1/api/**", "/**"] {
        assert!(
            validate_route_timeouts(&[ok.to_string()], &keys).is_ok(),
            "{ok} 应命中"
        );
    }
    // Given 零命中 pattern，When 校验，Then Err 点名 pattern 与路由数。
    let e = validate_route_timeouts(&["/v1/api/nope".to_string()], &keys).unwrap_err();
    assert!(e.contains("/v1/api/nope"), "{e}");
    assert!(e.contains('3'), "{e}");
    // Given pattern 含 `{}`（运行期永不命中的具体实例写法），When 校验，Then Err。
    let e = validate_route_timeouts(&["/v1/api/orders/{id}".to_string()], &keys).unwrap_err();
    assert!(e.contains("{id}"), "{e}");
    // 多条中一条零命中 → 整体 Err 且点名那一条。
    let e = validate_route_timeouts(
        &["/v1/api/a".to_string(), "/v1/api/ghost".to_string()],
        &keys,
    )
    .unwrap_err();
    assert!(e.contains("ghost"), "{e}");
}

// ---- v0.1.34：资源根 key 多源选择（connect_kv 的 --redis 解析） ----

/// 空 redis 段 → 内置 InMemoryKV 兜底；多源 redis 选错 default → fail-fast；
/// 选中存在的 profile 但无 kv 插件 → 明确提示缺插件（不静默回落）。
#[tokio::test]
async fn connect_kv_redis_profile_selection_and_fail_fast() {
    // 空 redis 段：无论 key 是什么都回落 InMemoryKV。
    let cfg = Config::default();
    let reg = Registries::default();
    let _ = connect_kv(&cfg, &reg, "default").await.unwrap();

    // 多源 redis（无 default）+ 未传 --redis（默认 default）→ fail-fast。
    let mut cfg = Config::default();
    cfg.redis
        .insert("cache".into(), "redis://:pw@h:6379/1".into());
    let e = connect_kv(&cfg, &reg, "default")
        .await
        .err()
        .unwrap_or_default();
    assert!(
        e.contains("--redis profile 'default' not declared"),
        "expected fail-fast on missing default profile, got: {e}"
    );

    // 选 cache：profile 存在，但无 kv 插件 → 另一种 fail-fast（提示装插件）。
    let e2 = connect_kv(&cfg, &reg, "cache")
        .await
        .err()
        .unwrap_or_default();
    assert!(
        e2.contains("no kv plugin loaded"),
        "expected missing-plugin error, got: {e2}"
    );
}

// ---- v0.1.25：server.html_meta_handler 装配期校验 ----

/// meta handler 必须命中一个 GET 路由：命中（含尾斜杠归一）/ 未命中 / 非法路径 / 未配。
#[test]
fn html_meta_handler_must_resolve_to_a_get_route() {
    let dir = std::env::temp_dir().join(format!("oj-metah-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("meta.ts"), "export default {};\n").unwrap();
    let entries = vec![
        routes::RouteEntry {
            method: "get".into(),
            pattern: "/v1/api/html-meta".into(),
            file: "meta.ts".into(),
            handler: None,
            schema: None,
        },
        routes::RouteEntry {
            method: "post".into(),
            pattern: "/v1/api/post-only".into(),
            file: "meta.ts".into(),
            handler: None,
            schema: None,
        },
    ];
    let (table, failures) = routes::RouteTable::from_entries(&dir, &entries);
    assert!(failures.is_empty(), "{failures:?}");
    let tables = vec![("/v1/api".to_string(), table)];

    let mut cfg = Config::default();
    // 未配置 → 放行（不开动态 meta 是默认形态）
    assert!(validate_html_meta_handler(&cfg, &tables).is_ok());
    // 命中（尾斜杠与 normalize 等价）
    cfg.server.html_meta_handler = Some("/v1/api/html-meta/".into());
    assert!(validate_html_meta_handler(&cfg, &tables).is_ok());
    // 未命中 → 拼错必须 fail-fast，不能留到运行期只在爬虫侧暴露
    cfg.server.html_meta_handler = Some("/v1/api/typo".into());
    let e = validate_html_meta_handler(&cfg, &tables).unwrap_err();
    assert!(e.contains("不在路由表"), "{e}");
    // 只有 POST → 报「未映射 GET 方法」（而不是含混的未命中）
    cfg.server.html_meta_handler = Some("/v1/api/post-only".into());
    let e = validate_html_meta_handler(&cfg, &tables).unwrap_err();
    assert!(e.contains("未映射 GET 方法"), "{e}");
    // 不以 / 开头 → 非法路径
    cfg.server.html_meta_handler = Some("v1/api/html-meta".into());
    let e = validate_html_meta_handler(&cfg, &tables).unwrap_err();
    assert!(e.contains("不是合法路径"), "{e}");
    // 在任何 api 挂载前缀之外 → 点名挂载前缀
    cfg.server.html_meta_handler = Some("/v9/handler".into());
    let e = validate_html_meta_handler(&cfg, &tables).unwrap_err();
    assert!(e.contains("不在任何 api 挂载前缀下"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- platform 段装配（ojm 批次，BDD S4：抢注防护 + trusted_paths fail-fast）----

/// S4（设计稿 §1.2「抢注防护与受控挂载」）：特权名单非空 → platform.root 必配
/// 且已挂载；特权名出现在安装树且不在受控根产物中 → Err；受控根产物豁免；
/// 名单悬空 → Err；trusted_paths 不存在 → Err；canonical 集合含 fs.root。
#[test]
fn platform_grants_squatting_protection() {
    let dir = std::env::temp_dir().join(format!("oj-platasm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // macOS 下 temp_dir 经 symlink：统一 canonical（build_platform_grants 内部
    // 对 platform.root canonicalize 后与 trees 比对，两侧须同形态）。
    let dir = dir.canonicalize().unwrap();
    // 布局：dist/ = 市场安装树（挂载）；platform-modules/ = 受控挂载根。
    // discover 可发现（dev 判定，name==目录名）。
    for (rel, name) in [
        ("dist/ojm-installer", "ojm-installer"),
        ("platform-modules/inst", "inst"),
    ] {
        std::fs::create_dir_all(dir.join(rel)).unwrap();
        std::fs::write(
            dir.join(rel).join("manifest.yaml"),
            format!("name: {name}\ndesc: d\nversion: 0.1.0\n"),
        )
        .unwrap();
    }
    let dist = dir.join("dist");
    let proot = dir.join("platform-modules");
    let trees = vec![(dist.clone(), true), (proot.clone(), true)];
    let installed: std::collections::HashSet<String> = trees
        .iter()
        .flat_map(|(r, ts)| {
            crate::manifest::discover(r, *ts, None)
                .unwrap()
                .into_iter()
                .map(|(n, _)| n)
        })
        .collect();
    assert!(installed.contains("ojm-installer") && installed.contains("inst"));

    let pcfg = |modules: Vec<&str>, root: Option<&str>| only_js::config::PlatformCfg {
        modules: modules.into_iter().map(String::from).collect(),
        root: root.map(String::from),
        trusted_paths: vec![],
        exec: Default::default(),
    };

    // ① 名单非空但缺 platform.root → Err（受控挂载根必配，fail-fast）。
    let cfg = Config {
        platform: Some(pcfg(vec!["inst"], None)),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap_err();
    assert!(e.contains("platform.root is missing"), "{e}");

    // ② root 未挂载为 api mount（trees 不含）→ Err（归属/路由无从产生）。
    let cfg = Config {
        platform: Some(pcfg(vec!["inst"], Some("./platform-modules"))),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &[(dist.clone(), true)], &installed, None)
        .unwrap_err();
    assert!(e.contains("must be mounted as an api mount"), "{e}");

    // ②b 第 8 态（互斥校验）：root 为某挂载树的**父目录**（前缀重叠）→ Err——
    //    受控根本身并非挂载树，其子树里的市场产物会静默穿透抢注绊线。
    std::fs::create_dir_all(dir.join("deploy/deploy-dist/ojm-installer")).unwrap();
    std::fs::write(
        dir.join("deploy/deploy-dist/ojm-installer/manifest.yaml"),
        "name: ojm-installer\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    let nested_trees = vec![(dir.join("deploy/deploy-dist"), true)];
    let installed_nested: std::collections::HashSet<String> =
        crate::manifest::discover(&dir.join("deploy/deploy-dist"), true, None)
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
    let cfg = Config {
        platform: Some(pcfg(vec!["ojm-installer"], Some("./deploy"))),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &nested_trees, &installed_nested, None)
        .unwrap_err();
    assert!(
        e.contains("must not overlap another api mount") && e.contains("anti-squatting tripwire"),
        "{e}"
    );

    // ②c 反方向（root 指到挂载树内部的子目录）→ 同拒。
    std::fs::create_dir_all(dir.join("dist/sub")).unwrap();
    let cfg = Config {
        platform: Some(pcfg(vec!["inst"], Some("./dist/sub"))),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap_err();
    assert!(e.contains("must not overlap another api mount"), "{e}");

    // ③ 抢注：特权名在安装树（dist）且不在受控根产物中 → Err 点名。
    let cfg = Config {
        platform: Some(pcfg(vec!["ojm-installer"], Some("./platform-modules"))),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap_err();
    assert!(
        e.contains("platform module 'ojm-installer' must not come from an install tree"),
        "{e}"
    );

    // ④ 悬空：名单含受控根下不存在的模块 → Err。
    let cfg = Config {
        platform: Some(pcfg(vec!["inst", "ghost"], Some("./platform-modules"))),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap_err();
    assert!(
        e.contains("platform module 'ghost' is not deployed under platform.root"),
        "{e}"
    );

    // ⑤ 正经部署（受控根产物豁免抢注）→ Ok；exec 矩阵随 grant 原样带入。
    let mut exec = std::collections::BTreeMap::new();
    exec.insert(
        "inst".to_string(),
        only_js::config::ExecAllowCfg {
            allow: vec!["tar -tzf ${path:archive}".to_string()],
            env: vec!["OJ_SECRET_KEY".to_string()],
        },
    );
    let cfg = Config {
        platform: Some(only_js::config::PlatformCfg {
            modules: vec!["inst".to_string()],
            root: Some("./platform-modules".to_string()),
            trusted_paths: vec![],
            exec,
        }),
        ..Default::default()
    };
    let (pg, eg) = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap();
    let pg = pg.expect("platform grant present");
    assert!(pg.modules.contains("inst"));
    assert!(
        eg.expect("exec grant present")
            .per_module
            .contains_key("inst")
    );

    // ⑥ trusted_paths 不存在 → Err 点名（canonicalize fail-fast，对齐 FsGrant::new）。
    let cfg = Config {
        platform: Some(only_js::config::PlatformCfg {
            modules: vec![],
            root: None,
            trusted_paths: vec!["no-such-path".to_string()],
            exec: Default::default(),
        }),
        ..Default::default()
    };
    let e = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap_err();
    assert!(
        e.contains("platform.trusted_paths canonicalize failed"),
        "{e}"
    );

    // ⑦ trusted_paths canonical + fs.root 自动并入（重复项去重）。
    std::fs::write(dir.join("config.yaml"), b"cfg").unwrap();
    let fs_root = dir.join("data");
    std::fs::create_dir_all(&fs_root).unwrap();
    let fs_grant = Arc::new(only_js::bridge::fs::FsGrant::new(&fs_root, false).unwrap());
    let cfg = Config {
        platform: Some(only_js::config::PlatformCfg {
            modules: vec![],
            root: None,
            trusted_paths: vec!["./config.yaml".to_string(), "data".to_string()],
            exec: Default::default(),
        }),
        ..Default::default()
    };
    let (pg, _) =
        build_platform_grants(&cfg, &dir, &Some(fs_grant), &trees, &installed, None).unwrap();
    let pg = pg.unwrap();
    assert!(pg.trusted.contains(&fs_root.canonicalize().unwrap()));
    assert!(
        pg.trusted
            .contains(&dir.join("config.yaml").canonicalize().unwrap())
    );
    assert_eq!(pg.trusted.len(), 2, "fs.root ∪ trusted_paths 去重后恰两项");

    std::fs::remove_dir_all(&dir).ok();
}

/// 装配期正例（ojm 批次 D4 补遗的 sample platform demo 同形）：受控挂载根下部署
/// `ojm-demo` 模块、trusted_paths 指向真实存在的 config.yaml、缺省 fs grant 缺失下
/// `build_platform_grants` 仍能产出含 ojm-demo 的特权授权且未 fail-fast。
/// 同时验证 mounts 三前缀（/v1/api=src、/=dist、/_platform=platform-root）互不重叠、
/// 不触发 anti-squatting 绊线。
#[test]
fn platform_grants_sample_demo_assembles() {
    let dir = std::env::temp_dir().join(format!("oj-platdemo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();

    // 模拟 sample 布局：src / dist / platform-root/ojm-demo（含 manifest）。
    std::fs::create_dir_all(dir.join("src/user")).unwrap();
    std::fs::write(
        dir.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    std::fs::create_dir_all(dir.join("platform-root/ojm-demo")).unwrap();
    std::fs::write(
        dir.join("platform-root/ojm-demo/manifest.yaml"),
        "name: ojm-demo\ndesc: 平台特权演示\nversion: 0.1.0\n",
    )
    .unwrap();
    // trusted_paths 指向真实存在的 config.yaml。
    std::fs::write(dir.join("config.yaml"), "fs:\n  root: \".\"\n").unwrap();

    let src = dir.join("src");
    let dist = dir.join("dist");
    let proot = dir.join("platform-root");
    // 三挂载树：与 sample 同形（/_platform 指向 platform-root，且互不为父子）。
    let trees = vec![
        (src.clone(), true),
        (dist.clone(), true),
        (proot.clone(), true),
    ];
    let installed: std::collections::HashSet<String> = trees
        .iter()
        .flat_map(|(r, ts)| {
            crate::manifest::discover(r, *ts, None)
                .unwrap()
                .into_iter()
                .map(|(n, _)| n)
        })
        .collect();
    assert!(installed.contains("ojm-demo") && installed.contains("user"));

    let cfg = Config {
        platform: Some(only_js::config::PlatformCfg {
            modules: vec!["ojm-demo".to_string()],
            root: Some("./platform-root".to_string()),
            trusted_paths: vec!["./config.yaml".to_string()],
            exec: {
                let mut m = std::collections::BTreeMap::new();
                m.insert(
                    "ojm-demo".to_string(),
                    only_js::config::ExecAllowCfg {
                        allow: vec!["tar -tzf ${path:archive}".to_string()],
                        env: vec![],
                    },
                );
                m
            },
        }),
        ..Default::default()
    };

    // 缺省 fs grant（fs: 未配置）→ 装配不应 fail-fast，且特权集合含 ojm-demo。
    let (pg, eg) = build_platform_grants(&cfg, &dir, &None, &trees, &installed, None).unwrap();
    let pg = pg.expect("platform grant present");
    assert!(pg.modules.contains("ojm-demo"));
    assert!(
        eg.expect("exec grant present")
            .per_module
            .contains_key("ojm-demo"),
        "exec 授权应含 ojm-demo"
    );
    // trusted_paths 已并入（受控根缺省 = [fs.root]，此处 fs 未配 → 仅 trusted）。
    assert!(
        pg.trusted.iter().any(|t| t.ends_with("config.yaml")),
        "trusted_paths 应含 config.yaml"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// 受控挂载根归属（BDD 补充，设计稿 §1.2 ①）：platform.root 下的模块经
/// 真实装配获得归属——特权模块读 trusted 文件可及、普通挂载树模块同路径
/// 仍 NotCapable；归属名喂给 assert_platform 直接调用逐态验证。
#[tokio::test(flavor = "current_thread")]
async fn platform_root_mount_gives_privileged_module_ownership() {
    use only_js::bridge::platform::assert_platform;
    let dir = std::env::temp_dir().join(format!("oj-platown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::create_dir_all(dir.join("src/plain")).unwrap();
    std::fs::create_dir_all(dir.join("platform-modules/inst")).unwrap();
    std::fs::write(dir.join("config.yaml"), b"cfg-content").unwrap();
    for (rel, name) in [("src/plain", "plain"), ("platform-modules/inst", "inst")] {
        std::fs::write(
            dir.join(rel).join("manifest.yaml"),
            format!("name: {name}\ndesc: d\nversion: 0.1.0\n"),
        )
        .unwrap();
    }
    let cfg_path = dir.join("config.yaml");
    // Windows canonicalize 得 verbatim `\\?\C:\...`：反斜杠进 JS 双引号串字面量
    // 会被当转义吃掉（`\r`→CR、`\U`→U……），路径被毁成相对路径落回 jail 内，
    // 再被 deno_permissions 拒掉（毁错的路径含 CR 控制字符，dunce 不肯剥
    // verbatim 前缀，与已剥前缀的 allow 根比对失配 → NotCapable）。嵌入 JS
    // 前先把 `\` 转义成 `\\`（unix 路径无反斜杠，no-op）。
    let cfg_js = cfg_path.display().to_string().replace('\\', "\\\\");
    let api_js = format!(
        "export async function readCfg() {{\n\
             \x20 const t = await fs.readTextFile(\"{cfg_js}\");\n\
             \x20 json.ok({{ t }});\n\
             }}\n\
             export default {{ readCfg }};\n",
    );
    std::fs::write(dir.join("src/plain/api.js"), &api_js).unwrap();
    std::fs::write(dir.join("platform-modules/inst/api.js"), &api_js).unwrap();

    let cfg = Config {
        fs: Some(only_js::config::FsSection {
            root: "data".into(),
            readonly: false,
        }),
        platform: Some(only_js::config::PlatformCfg {
            modules: vec!["inst".to_string()],
            root: Some("./platform-modules".to_string()),
            trusted_paths: vec!["./config.yaml".to_string()],
            exec: Default::default(),
        }),
        ..Default::default()
    };
    let trees = vec![
        (dir.join("src"), true),
        (dir.join("platform-modules"), true),
    ];
    let backend = assemble_backend(
        &cfg,
        &serde_json::Value::Null,
        &dir,
        &trees,
        "/v1/api",
        true,
        &ResourceProfiles::default(),
        None,
        None,
    )
    .await
    .unwrap();
    let stable = backend.stable().clone();
    assert!(
        stable
            .platform
            .as_deref()
            .is_some_and(|p| p.modules.contains("inst")),
        "platform grant assembled"
    );
    let t = std::time::Duration::from_secs(5);

    // ① 受控根模块：归属成立 + 特权可及（trusted 文件读取走完整 fs 链路）。
    let cap = backend.make_bridge()()
        .run_module(
            &dir.join("platform-modules/inst/api.js"),
            "readCfg",
            only_js::bridge::RequestInfo::default(),
            t,
        )
        .await
        .unwrap();
    assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
    assert!(
        String::from_utf8_lossy(&cap.body).contains("cfg-content"),
        "{}",
        String::from_utf8_lossy(&cap.body)
    );

    // ② 普通挂载树模块：同路径仍 NotCapable（特权面不随挂载树扩散）。
    let e = backend.make_bridge()()
        .run_module(
            &dir.join("src/plain/api.js"),
            "readCfg",
            only_js::bridge::RequestInfo::default(),
            t,
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("NotCapable"), "{e}");

    // ③ assert_platform 直接调用（run_module 后的归属形态）：
    //    受控根归属名过、普通名拒、无归属拒。
    let opstate_for = |module: Option<&str>| {
        use deno_core::OpState;
        let mut st = OpState::new(None);
        st.put(stable.clone());
        // ReqState 含 pub(crate) 字段，跨 crate 只能 default + 逐字段赋值。
        let mut rs = only_js::bridge::ReqState::default();
        rs.module = module.map(String::from);
        st.put(rs);
        st
    };
    assert_platform(&opstate_for(Some("inst"))).expect("controlled-root module passes");
    let e = assert_platform(&opstate_for(Some("plain"))).unwrap_err();
    assert!(
        e.to_string()
            .contains("module 'plain' is not platform-privileged"),
        "{e}"
    );
    assert!(assert_platform(&opstate_for(None)).is_err());

    std::fs::remove_dir_all(&dir).ok();
}

// ---- v0.1.58：resolve_mounts 矩阵（归一/配对/根/缺目录/同类重复）----

fn tmp_dirs(names: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let base = std::env::temp_dir().join(format!(
        "oj-sites-{}-{}",
        std::process::id(),
        names.join("_")
    ));
    let _ = std::fs::remove_dir_all(&base);
    let dirs: Vec<PathBuf> = names
        .iter()
        .map(|n| {
            let d = base.join(n);
            std::fs::create_dir_all(&d).unwrap();
            d
        })
        .collect();
    (base, dirs)
}

fn mount(
    prefix: &str,
    api: Option<&str>,
    web: Option<&str>,
    spa: Option<bool>,
) -> config::MountConf {
    config::MountConf {
        prefix: prefix.into(),
        api: api.map(Into::into),
        web: web.map(Into::into),
        spa,
        headers: Default::default(),

        ..Default::default()
    }
}

#[test]
fn resolve_mounts_matrix() {
    let (base, dirs) = tmp_dirs(&["api", "web", "docs"]);
    // 归一：尾斜杠剪除；目录 canonicalize；api 逐条目 dev 判定（无 manifests.yaml → ts）。
    let cfg = Config {
        mounts: vec![
            mount(
                "/v1/api/",
                Some(dirs[0].to_string_lossy().as_ref()),
                None,
                None,
            ),
            mount("/", None, Some(dirs[1].to_string_lossy().as_ref()), None),
        ],
        ..Default::default()
    };
    let out = resolve_mounts(&cfg, &base).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].prefix, "/v1/api");
    let (d, ts) = out[0].kind.api().unwrap();
    assert_eq!(d, &dirs[0].canonicalize().unwrap());
    assert!(ts, "无 manifests.yaml → dev");
    assert!(matches!(
        out[1].kind,
        ResolvedMountKind::Web { spa: false, .. }
    ));

    // 嵌套合法：/v1 web(spa) + /v1/api api；根 `/` api 挂载合法。
    let cfg = Config {
        mounts: vec![
            mount(
                "/v1",
                None,
                Some(dirs[1].to_string_lossy().as_ref()),
                Some(true),
            ),
            mount(
                "/v1/api",
                Some(dirs[0].to_string_lossy().as_ref()),
                None,
                None,
            ),
            mount("/", Some(dirs[2].to_string_lossy().as_ref()), None, None),
        ],
        ..Default::default()
    };
    let out = resolve_mounts(&cfg, &base).unwrap();
    assert_eq!(out.len(), 3);
    assert!(matches!(
        out[0].kind,
        ResolvedMountKind::Web { spa: true, .. }
    ));

    // 同类重复（含尾斜杠归一口径）→ Err；api+web 配对 → 合法。
    let cfg = Config {
        mounts: vec![
            mount("/docs", None, Some("docs"), None),
            mount("/docs/", None, Some("docs2"), None),
        ],
        ..Default::default()
    };
    let e = resolve_mounts(&cfg, &base).unwrap_err();
    assert!(e.contains("duplicate web mount"), "{e}");
    let cfg = Config {
        mounts: vec![
            mount("/p", Some("api"), None, None),
            mount("/p", None, Some("web"), None),
        ],
        ..Default::default()
    };
    assert_eq!(resolve_mounts(&cfg, &base).unwrap().len(), 2);

    // 缺失目录 → Err（canonicalize fail-fast，带条目序号）。
    let cfg = Config {
        mounts: vec![mount("/x", None, Some("no-such-dir"), None)],
        ..Default::default()
    };
    let e = resolve_mounts(&cfg, &base).unwrap_err();
    assert!(e.contains("mounts[0]") && e.contains("no-such-dir"), "{e}");

    // 声明序保留（primary = 第一条 api）。
    let cfg = Config {
        mounts: vec![
            mount(
                "/second",
                Some(dirs[0].to_string_lossy().as_ref()),
                None,
                None,
            ),
            mount(
                "/first",
                Some(dirs[2].to_string_lossy().as_ref()),
                None,
                None,
            ),
        ],
        ..Default::default()
    };
    let out = resolve_mounts(&cfg, &base).unwrap();
    assert_eq!(out[0].prefix, "/second");

    // 未配置 → 空表（纯静态占位由 from_config 决定）。
    assert!(
        resolve_mounts(&Config::default(), &base)
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// 跨树模块名重复 → Err（旧单树形态下静默共存/遮蔽，多挂载下必须 fail-fast）。
#[tokio::test(flavor = "current_thread")]
async fn build_schema_rejects_cross_tree_module_name_dup() {
    let (base, dirs) = tmp_dirs(&["t1", "t2"]);
    for d in &dirs {
        std::fs::create_dir_all(d.join("u")).unwrap();
        std::fs::write(
            d.join("u/manifest.yaml"),
            "name: u\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
    }
    let trees = vec![(dirs[0].clone(), true), (dirs[1].clone(), true)];
    let e = build_schema_and_modules(&trees, only_js::bridge::SqlGuard::Off, &[], None)
        .await
        .err()
        .unwrap_or_else(|| panic!("expected cross-tree dup error"));
    assert!(e.contains("u") && e.contains("multiple"), "{e}");
    // 同一树两份没问题（不触发跨树重名）。
    let trees = vec![(dirs[0].clone(), true)];
    assert!(
        build_schema_and_modules(&trees, only_js::bridge::SqlGuard::Off, &[], None)
            .await
            .is_ok()
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ext_boot_spec_absent_vs_present() {
    let dir = std::env::temp_dir().join(format!("oj-bootspec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(ext_boot_spec(&dir).unwrap().is_none());

    let p = dir.join("ext_boot.js");
    std::fs::write(&p, "globalThis.foo = 1;\n").unwrap();
    let spec = ext_boot_spec(&dir).unwrap().unwrap();
    assert!(spec.starts_with("file://"), "{spec}");
    assert!(spec.contains("?v="), "{spec}");
    assert!(spec.contains("ext_boot.js"), "{spec}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// exec 拆分契约（spec §2/§3.1）：后端装配不含证书门禁——Backend 可在无证书
/// 配置的最小 config 上构造，stable 是唯一数据源（访问器直达）。
#[tokio::test(flavor = "current_thread")]
async fn backend_constructs_without_certificate() {
    let base = std::env::temp_dir().join(format!("oj-backend-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let cfg = Config::default(); // 无证书、无 db/redis 段
    let backend = super::assemble_backend(
        &cfg,
        &serde_json::Value::Null,
        &base,
        &[(base.clone(), true)],
        "/v1/api",
        true,
        &ResourceProfiles::default(),
        None,
        None,
    )
    .await
    .unwrap();
    // stable 单源：kv/dbs/loader 都经访问器直达，Backend 不复制字段。
    assert!(backend.stable().loader.is_some());
    assert!(backend.stable().dbs.is_empty());
    let _ = std::fs::remove_dir_all(&base);
}

// ---- v0.1.23：匿名路径迁移 WARN 的影响面判定（U39）----

/// 路由 pattern → 匿名口径：剥 base，`{param}`→`*`、`{*rest}`→`**`。
#[test]
fn anon_view_strips_base_and_normalizes_params() {
    assert_eq!(
        anon_view_of_route("/v1/api/oidc/{id}", "/v1/api"),
        "/oidc/*"
    );
    assert_eq!(
        anon_view_of_route("/v1/api/idp/.well-known/openid-configuration", "/v1/api"),
        "/idp/.well-known/openid-configuration"
    );
    assert_eq!(
        anon_view_of_route("/v1/api/assets/{*path}", "/v1/api"),
        "/assets/**"
    );
    // base 不在 pattern 里（非常规）→ 原样归一，不 panic。
    assert_eq!(anon_view_of_route("/health", "/v1/api"), "/health");
}

/// 旧式前缀形态：只有尾段一个 `*`、其余段全字面。
///
/// 这是「可能受 v0.1.20 收紧影响」的**唯一**形态——旧 oj-auth 是 `strip_suffix("/*")` +
/// `starts_with`，含中段 `*` 的条目当时匹配不上任何真实请求（前缀里带字面 `*`），
/// 只能诞生于 v0.1.20 之后的四形态语义，即刻意写出的结构。
#[test]
fn legacy_prefix_shape_is_tail_wildcard_on_literal_segments() {
    assert!(is_legacy_prefix_shape("/idp/*"));
    assert!(is_legacy_prefix_shape("/users/me/accounts/*"));
    assert!(is_legacy_prefix_shape("/public/assets/v2/anchor/*"));
    // 结构条目（中段含 `*`）→ 不是旧前缀形态（下游 config 实测里的真实写法）。
    assert!(!is_legacy_prefix_shape("/public/anchor/*/issues/*"));
    assert!(!is_legacy_prefix_shape("/public/assets/v2/anchor/*/*"));
    assert!(!is_legacy_prefix_shape("/a/*/b")); // 尾段非 `*`
    assert!(!is_legacy_prefix_shape("/health"));
    assert!(!is_legacy_prefix_shape("/a/**"));
}

/// 判据核心：旧前缀形态 **且** 自己的面确实比严格一层更深 → 告警；否则静默。
///
/// 语义口径：旧 oj-auth 是「任意深度」，收紧成严格一层后**丢掉的**恰好是「head + ≥2 段」；
/// `**` 多出来的零层（裸 head）旧语义也没覆盖 → 不算丢面、不告警。
#[test]
fn coverage_loss_requires_a_deeper_registered_route() {
    // 深两层的注册路由被收回 → 告警（U39 真阳性样本）。
    assert!(tail_entry_loses_coverage(
        "/idp/*",
        &["/idp/.well-known/openid-configuration".to_string()]
    ));
    // 本就没有更深路由 → 静默。
    assert!(!tail_entry_loses_coverage(
        "/auth/oidc/*",
        &["/auth/oidc/callback".to_string()]
    ));
    // 同层 → 不新增命中 → 静默。
    assert!(!tail_entry_loses_coverage("/a/*", &["/a/b".to_string()]));
    // 无任何路由（纯 404 面）→ 静默。
    assert!(!tail_entry_loses_coverage("/a/*", &[]));
    // 评审反例①：catch-all 路由（`{*path}` → 视图 `**`）。旧行为下 /file/a/b 是免鉴权的，
    // 收紧确实收回了面 → 必须告警（此前用 path_matches 对拍两个模式会漏报）。
    assert!(tail_entry_loses_coverage(
        "/file/*",
        &["/file/**".to_string()]
    ));
    // 评审反例②：路由带参数段（视图 `/*/detail/sub`）→ `/zzz/detail/sub` 真实可达且更深
    // → 必须告警（此前两侧都匹配不上而漏报）。
    assert!(tail_entry_loses_coverage(
        "/zzz/*",
        &["/*/detail/sub".to_string()]
    ));
    // 评审反例③：模块根路由（视图与 head 同层，`**` 只多出「零层」）→ 旧语义也没覆盖裸路径
    // → 不算丢面，静默（否则下游「模块根路由 + /module/* 条目」这种常见形状会被迫标 one_layer）。
    assert!(!tail_entry_loses_coverage(
        "/user/account/*",
        &["/user/account".to_string()]
    ));
    // 「head + 恰好 1 段」= 严格一层已覆盖的深度 → 不算丢面；再加一层才算。
    assert!(!tail_entry_loses_coverage("/a/*", &["/a/b".to_string()]));
    assert!(tail_entry_loses_coverage("/a/*", &["/a/b/c".to_string()]));
    // 结构条目：即使更深路由存在，也不按旧前缀看待 → 静默（改 `**` 不是它想要的形状）。
    let structured = vec!["/public/anchor/*/issues/*/comments/*".to_string()];
    assert!(!tail_entry_loses_coverage(
        "/public/anchor/*/issues/*",
        &structured
    ));
    assert!(!tail_entry_loses_coverage(
        "/public/assets/v2/anchor/*/*",
        &structured
    ));
    // 对照：同一份路由下，旧前缀形态的 /public/anchor/* 仍会告警。
    assert!(tail_entry_loses_coverage("/public/anchor/*", &structured));
}

/// 聚合清单（`warn_legacy_tail_wildcards` 的实际输入）：`one_layer` 确认过的、非旧前缀
/// 形态的、以及没丢面的条目都被滤掉。**直接打这个函数**，不复制它的过滤链。
#[test]
fn legacy_entries_filters_by_shape_and_impact() {
    use only_js::config::AnonPath;
    let list = vec![
        AnonPath::Plain("/idp/*".into()),       // 丢面 → 点名
        AnonPath::Plain("/auth/oidc/*".into()), // 无更深路由 → 静默
        AnonPath::Detailed {
            path: "/x/*".into(),
            one_layer: true,
        }, // 已确认 → 静默
        AnonPath::Plain("/health".into()),      // 非尾 /* → 静默
        // 结构条目（中段 `*`）→ 不按旧前缀看待 → 静默（下游 config 实测的同款写法）
        AnonPath::Plain("/public/anchor/*/issues/*".into()),
        AnonPath::Plain("/public/assets/v2/anchor/*/*".into()),
    ];
    let routes = vec![
        "/idp/.well-known/openid-configuration".to_string(),
        "/auth/oidc/callback".to_string(),
        "/x/a/b/c".to_string(), // 即使会丢面，one_layer 也压掉
        "/public/anchor/*/issues/*/comments/*".to_string(),
    ];
    assert_eq!(legacy_entries(&list, &routes), vec!["/idp/*"]);
    // 空列表 / 空路由表不 panic。
    assert!(legacy_entries(&[], &routes).is_empty());
    assert!(legacy_entries(&list, &[]).is_empty());
}

/// 迁移 WARN **只针对 auth 列表**（v0.1.23 订正）：租户侧自引入起即为严格一层
/// （`git show v0.1.19:server/src/lib.rs` 的 `path_matches`，v0.1.19 的租户豁免 `:364`
/// 走的就是它），v0.1.20 的收紧没碰它，对 tenant 条目说「收回了面」是伪前提。
#[test]
fn migration_warn_is_auth_only() {
    use only_js::config::{AnonPath, AuthCfg};
    let routes = vec!["/idp/.well-known/openid-configuration".to_string()];

    // 只配 tenant → 一条都不点（尽管该条目在 auth 侧同形是会丢面的）。
    let mut cfg = Config::default();
    cfg.tenant.anonymous_paths = vec![AnonPath::Plain("/idp/*".into())];
    assert!(migration_warn_entries(&cfg, &routes).is_empty());

    // 同形条目出现在 auth 侧 → 点名，且 what 是 auth 段。
    cfg.auth = Some(AuthCfg {
        jwt_secret: "k".into(),
        anonymous_paths: vec![AnonPath::Plain("/idp/*".into())],
        ..Default::default()
    });
    let entries = migration_warn_entries(&cfg, &routes);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "auth.anonymous_paths");
    assert_eq!(entries[0].1, vec!["/idp/*"]);
}
