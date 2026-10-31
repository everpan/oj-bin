use super::*;

fn fixture(files: &[&str]) -> std::path::PathBuf {
    // 计数器唯一化：并行测试下 `{:p}` 指针可被分配器复用，曾致临时目录串台。
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let base = std::env::temp_dir().join(format!(
        "oj-routes-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
    assert_eq!(
        r.resolve("/v1/api/user/account/").map(|(f, _)| f),
        Some(root.join("user/account/api.ts"))
    );
    assert_eq!(
        r.resolve("/v1/api/user/account").map(|(f, _)| f),
        Some(root.join("user/account/api.ts"))
    );
    assert_eq!(
        r.resolve("/v1/api/user/profile/detail/").map(|(f, _)| f),
        Some(root.join("user/profile/detail/api.ts"))
    );
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
    assert_eq!(r.resolve("/v1/apifoo/user/"), None);
}

#[test]
fn release_mode_maps_api_js() {
    let root = fixture(&["user/account/api.js"]);
    assert!(
        Routes::new("/v1/api", &root, false)
            .resolve("/v1/api/user/account/")
            .is_some()
    );
    assert!(
        Routes::new("/v1/api", &root, true)
            .resolve("/v1/api/user/account/")
            .is_none()
    );
}

#[test]
fn resolve_underscore_dir_descends_with_params() {
    let root = fixture(&["a/_id_/api.ts"]);
    let r = Routes::new("/v1/api", &root, true);
    match r.resolve("/v1/api/a/42") {
        Some((f, p)) => {
            assert_eq!(f, root.join("a/_id_/api.ts"));
            assert_eq!(p["id"], "42");
        }
        None => panic!("expected hit"),
    }
    // 字面 `_id_` URL：兜底快路径命中磁盘字面目录（无参数）——生产上流
    // 程表查询在前，注册过的 `_id_` 目录由 `{id}` 参数吃掉（见
    // table_underscore_dir_becomes_param）；兜底只服务表外文件，保持字面优先。
    let (f, p) = r.resolve("/v1/api/a/_id_").unwrap();
    assert_eq!(f, root.join("a/_id_/api.ts"));
    assert!(p.is_empty(), "{p:?}");
}

#[test]
fn resolve_descend_literal_first_and_backtracks() {
    // 字面目录优先：a/me/api.ts 存在时 /a/me 命中它（无参数）。
    let root = fixture(&["a/_id_/api.ts", "a/me/api.ts"]);
    let r = Routes::new("/v1/api", &root, true);
    let (f, p) = r.resolve("/v1/api/a/me").unwrap();
    assert_eq!(f, root.join("a/me/api.ts"));
    assert!(p.is_empty(), "{p:?}");
    // 回溯：字面 a/b/ 存在但子树死路，回退到 _x_ 候选。
    let root2 = fixture(&["a/_x_/c/api.ts"]);
    std::fs::create_dir_all(root2.join("a/b")).unwrap();
    let r2 = Routes::new("/v1/api", &root2, true);
    let (f2, p2) = r2.resolve("/v1/api/a/b/c").unwrap();
    assert_eq!(f2, root2.join("a/_x_/c/api.ts"));
    assert_eq!(p2["x"], "b");
}

#[test]
fn resolve_descend_first_candidate_only_and_smuggling() {
    // 同层多个 _x_：只试排序首个（对齐建表期同位异名丢弃语义）。
    let root = fixture(&["a/_aa_/api.ts", "a/_bb_/api.ts"]);
    let r = Routes::new("/v1/api", &root, true);
    let (f, p) = r.resolve("/v1/api/a/zz").unwrap();
    assert_eq!(f, root.join("a/_aa_/api.ts"));
    assert_eq!(p["aa"], "zz");
    // 走私：%2e%2e / a%2Fb 经参数段 → decode_params 拒 → 404
    assert!(r.resolve("/v1/api/a/%2e%2e").is_none());
    assert!(r.resolve("/v1/api/a/a%2Fb").is_none());
}

#[test]
fn resolve_deep_mixed_underscore_dirs() {
    // PRD 原例形态（含模块段 _aa_）。
    let root = fixture(&["_aa_/bb/_cc_/api.ts"]);
    let r = Routes::new("/v1/api", &root, true);
    let (f, p) = r.resolve("/v1/api/11/bb/22").unwrap();
    assert_eq!(f, root.join("_aa_/bb/_cc_/api.ts"));
    assert_eq!(p["aa"], "11");
    assert_eq!(p["cc"], "22");
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

#[test]
fn normalize_guards_and_trims() {
    assert_eq!(
        normalize("/v1/api/user/account/"),
        Some("/v1/api/user/account".into())
    );
    assert_eq!(
        normalize("/v1/api/user/account"),
        Some("/v1/api/user/account".into())
    );
    assert_eq!(normalize("/"), Some("/".into()));
    assert_eq!(normalize("/v1/api//dbl"), None); // 空段（missing_or_traversal_is_none 契约）
    assert_eq!(normalize("/v1/api/../etc"), None); // 穿越段
    assert_eq!(normalize("/v1/api/./x"), None);
    assert_eq!(normalize("/v1/api/a\\b"), None); // 反斜杠
    assert_eq!(normalize("/v1/api/a\0b"), None); // NUL
}

#[test]
fn decode_params_validates_smuggling() {
    let one = |v: &str| decode_params(vec![("id".into(), v.into())].into_iter());
    assert_eq!(one("42").unwrap()["id"], "42");
    assert_eq!(one("%41").unwrap()["id"], "A"); // 正常解码
    assert!(one("%2e%2e").is_none()); // 编码穿越
    assert!(one(".").is_none());
    // 单段走私斜杠：raw 无 / 而解码后有 → 拒绝
    assert!(one("a%2Fb").is_none());
    assert!(one("a%5Cb").is_none());
    // catch-all 值：raw 含真实分隔符 → 放行
    let ca = decode_params(vec![("path".into(), "a/b%20c".into())].into_iter());
    assert_eq!(ca.unwrap()["path"], "a/b c");
}

// ----- fs 段转换（v0.1.27 `_name_` 约定） -----

#[test]
fn fs_seg_conversion_rules() {
    // 转换：整段 `_name_`，inner 可含下划线
    assert_eq!(fs_seg_to_pattern("_aa_"), "{aa}");
    assert_eq!(fs_seg_to_pattern("_a_b_"), "{a_b}");
    // 不转换（原样返回）
    for lit in [
        "_", "__", "_a", "a_", "__x__", "___", "_a{b}_", "_shared", "a_bb_c", "plain",
    ] {
        assert_eq!(fs_seg_to_pattern(lit), lit, "{lit}");
    }
    // 宽松判定（告警用）与严格转换的区别
    assert!(looks_like_underscore_param("__x__")); // 宽松命中……
    assert_eq!(fs_seg_to_pattern("__x__"), "__x__"); // ……但严格转换拒绝
}

#[test]
fn table_underscore_dir_becomes_param() {
    let (t, f) = tbl(&["user/_id_/api.ts"], &[("user/_id_/api.ts", "get", "")]);
    assert!(f.is_empty(), "{f:?}");
    match t.lookup("/v1/api/user/42", "GET") {
        Lookup::Hit { file, params, .. } => {
            assert!(file.ends_with("_id_/api.ts"), "{file:?}");
            assert_eq!(params["id"], "42");
        }
        other => panic!("{}", kind(&other)),
    }
}

#[test]
fn table_underscore_dir_static_sibling_priority() {
    let (t, f) = tbl(
        &["x/_pk_/api.ts", "x/me/api.ts"],
        &[("x/_pk_/api.ts", "get", ""), ("x/me/api.ts", "get", "")],
    );
    assert!(f.is_empty(), "{f:?}");
    match t.lookup("/v1/api/x/me", "GET") {
        Lookup::Hit { file, params, .. } => {
            assert!(file.ends_with("x/me/api.ts"), "{file:?}");
            assert!(params.is_empty(), "{params:?}");
        }
        other => panic!("{}", kind(&other)),
    }
    match t.lookup("/v1/api/x/42", "GET") {
        Lookup::Hit { params, .. } => assert_eq!(params["pk"], "42"),
        other => panic!("{}", kind(&other)),
    }
}

#[test]
fn table_same_level_underscore_params_conflict() {
    // 同层异名 `_x_` = matchit 同位异名参数 → 结构性冲突，后者丢弃。
    let (t, f) = tbl(
        &["u/_aa_/api.ts", "u/_bb_/api.ts"],
        &[("u/_aa_/api.ts", "get", ""), ("u/_bb_/api.ts", "get", "")],
    );
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].contains("invalid route"), "{f:?}");
    assert!(matches!(t.lookup("/v1/api/u/1", "GET"), Lookup::Hit { .. }));
}

#[test]
fn table_underscore_dir_plus_relative_route() {
    let (t, f) = tbl(&["u/_id_/api.ts"], &[("u/_id_/api.ts", "get", "{sub}")]);
    assert!(f.is_empty(), "{f:?}");
    match t.lookup("/v1/api/u/42/99", "GET") {
        Lookup::Hit { params, .. } => {
            assert_eq!(params["id"], "42");
            assert_eq!(params["sub"], "99");
        }
        other => panic!("{}", kind(&other)),
    }
}

#[test]
fn table_brace_dir_conflicts_with_underscore_dir() {
    // `{id}` 字面目录今天已意外可用；与 `_id_` 同 pattern 同方法 → 冲突钉死 500。
    let (t, f) = tbl(
        &["u/{id}/api.ts", "u/_id_/api.ts"],
        &[("u/{id}/api.ts", "get", ""), ("u/_id_/api.ts", "get", "")],
    );
    assert!(f.iter().any(|s| s.contains("route conflict")), "{f:?}");
    assert!(matches!(
        t.lookup("/v1/api/u/1", "GET"),
        Lookup::Conflict(_)
    ));
}

#[test]
fn table_route_value_underscore_shape_warns() {
    // `.route = "_id_"` 是字面段（不转换），但须给出 warning: 前缀告警。
    let (t, f) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "_id_")]);
    assert!(f.iter().any(|s| s.starts_with("warning:")), "{f:?}");
    match t.lookup("/v1/api/u/_id_", "GET") {
        Lookup::Hit { params, .. } => assert!(params.is_empty(), "{params:?}"),
        other => panic!("{}", kind(&other)),
    }
}

#[test]
fn check_patterns_rejects_invalid_and_conflicting() {
    // 合法：重复 pattern（多方法）可合并
    assert!(check_patterns(&["u/{id}".into(), "u/{id}".into()]).is_ok());
    // 非法语法（matchit 混合段）
    assert!(check_patterns(&["u/{id}.json".into()]).is_err());
    // 同位异名参数（两个 _x_ 目录的产物形态）
    assert!(check_patterns(&["u/{aa}".into(), "u/{bb}".into()]).is_err());
}

// ----- RouteTable（纯逻辑，假内省闭包） -----

#[test]
fn patterns_exposes_registered_url_patterns() {
    // U-18 批 T1.1：装配期 route_timeouts 校验需要注册 pattern 全集。
    // Given 建表含字面路由与 {param} 路由，When patterns()，Then 两者都在（含 base）。
    let (t, f) = tbl(
        &["a/api.ts"],
        &[("a/api.ts", "get", ""), ("a/api.ts", "post", "{id}")],
    );
    assert!(f.is_empty(), "{f:?}");
    let ps = t.patterns();
    assert!(ps.contains(&"/v1/api/a"), "{ps:?}");
    assert!(ps.contains(&"/v1/api/a/{id}"), "{ps:?}");
}

fn tbl(files: &[&str], decls: &[(&str, &str, &str)]) -> (RouteTable, Vec<String>) {
    // decls: (文件相对路径, handler 链, 有效 .route 值；空串 = 未挂)。
    // v0.1.64：链即内省产物的 chain（`"cccc.get"`；顶层动词为单段），route 为
    // 组前缀解析后的有效路由（JS 侧完成，Rust 只见最终词汇）。
    let root = fixture(files);
    let mut m: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
    for (f, c, r) in decls {
        m.entry(f.to_string()).or_default().push((
            c.to_string(),
            if r.is_empty() {
                None
            } else {
                Some(r.to_string())
            },
        ));
    }
    RouteTable::build("/v1/api", &root, true, None, |p: &Path| {
        // 跨平台：file 路径在 Windows 用反斜杠，而 decls 的 key 用正斜杠，
        // 统一转正斜杠再查表，否则 Windows 下 key 不匹配 → 整表空注册。
        let key = p
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        Ok(m.get(&key)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(c, r)| (c, r, None))
            .collect())
    })
}

// ----- v0.1.64 嵌套 handler 组 -----

#[test]
fn table_registers_nested_group_routes() {
    // 组无 .route → 挂 {父前缀}/{组名}；Lookup::Hit 带 handler 链。
    // 注意 route 列为**有效路由**（内省已展开组名前缀）：组动词恒非空。
    let (t, f) = tbl(
        &["aaa/bbb/api.ts"],
        &[
            ("aaa/bbb/api.ts", "get", ""),
            ("aaa/bbb/api.ts", "cccc.get", "cccc"),
            ("aaa/bbb/api.ts", "cccc.post", "cccc"),
            ("aaa/bbb/api.ts", "cccc.list.get", "cccc/list"),
        ],
    );
    assert!(f.is_empty(), "{f:?}");
    for (path, verb, want) in [
        ("/v1/api/aaa/bbb", "GET", "get"),
        ("/v1/api/aaa/bbb/cccc", "GET", "cccc.get"),
        ("/v1/api/aaa/bbb/cccc", "POST", "cccc.post"),
        ("/v1/api/aaa/bbb/cccc/list", "GET", "cccc.list.get"),
    ] {
        match t.lookup(path, verb) {
            Lookup::Hit { handler, .. } => {
                assert_eq!(handler, want, "{path} {verb}")
            }
            _ => panic!("expected hit for {path} {verb}"),
        }
    }
}

#[test]
fn table_group_route_prefix_and_verb_override() {
    // 组 .route 已由内省展开为有效路由：cccc.get → /x；cccc.post 自身 /y 覆盖
    let (t, f) = tbl(
        &["g/api.ts"],
        &[
            ("g/api.ts", "cccc.get", "/x"),
            ("g/api.ts", "cccc.post", "/y"),
        ],
    );
    assert!(f.is_empty(), "{f:?}");
    for (path, verb, want) in [
        ("/v1/api/x", "GET", "cccc.get"),
        ("/v1/api/y", "POST", "cccc.post"),
    ] {
        match t.lookup(path, verb) {
            Lookup::Hit { handler, .. } => assert_eq!(handler, want, "{path} {verb}"),
            _ => panic!("expected hit for {path} {verb}"),
        }
    }
    // 组动词各自 replaced：目录镜像不复活
    assert!(matches!(t.lookup("/v1/api/g", "GET"), Lookup::NotFound));
}

#[test]
fn table_replaced_semantics_with_groups() {
    // 顶层动词无 .route → 不标记 replaced（旧语义保持）
    let (t, _) = tbl(&["a/api.ts"], &[("a/api.ts", "get", "")]);
    let id = t.listing()[0].file;
    assert!(!t.is_replaced(t.file_path(id), "get"));
    // 组动词（有效路由非空）→ 各自标记；同文件无 .route 的 post 不标记
    let (t, _) = tbl(
        &["b/api.ts"],
        &[("b/api.ts", "cccc.get", "cccc"), ("b/api.ts", "post", "")],
    );
    let id = t
        .listing()
        .iter()
        .find(|r| r.handler == "cccc.get")
        .unwrap()
        .file;
    assert!(t.is_replaced(t.file_path(id), "get"));
    assert!(!t.is_replaced(t.file_path(id), "post"));
    // 表内该 pattern 已存在（post 注册于目录镜像）→ GET 得 405；
    // replaced 的「兜底 404 不复活」在 serve::handle 的 fs 兜底层判定
    // （表 miss 才走兜底），集成用例见 oj/tests/e2e.rs。
    assert!(matches!(
        t.lookup("/v1/api/b", "GET"),
        Lookup::MethodNotAllowed
    ));
    match t.lookup("/v1/api/b", "POST") {
        Lookup::Hit { handler, .. } => assert_eq!(handler, "post"),
        _ => panic!("expected hit"),
    }
}

// ---- v0.1.70：模块归属 / 相对路径（供 openapi 出 tags、x-oj-file） ----

#[test]
fn module_of_strips_release_version_suffix_but_keeps_hyphen_names() {
    let dir = fixture(&[]);
    let mk = |file: &str| -> (RouteTable, FileId) {
        let (t, f) = RouteTable::from_entries(
            &dir,
            &[RouteEntry {
                method: "get".to_string(),
                pattern: "/v1/api/x".to_string(),
                file: file.to_string(),
                handler: None,
                schema: None,
            }],
        );
        assert!(f.is_empty(), "{f:?}");
        let id = t.listing()[0].file;
        (t, id)
    };
    // release：`<module>-<version>/...` → 剥版本
    assert_eq!(
        mk("wiki-0.1.0/api.js")
            .0
            .module_of(mk("wiki-0.1.0/api.js").1)
            .as_deref(),
        Some("wiki")
    );
    assert_eq!(
        mk("wiki-0.1.0/collections/api.js")
            .0
            .module_of(mk("wiki-0.1.0/collections/api.js").1)
            .as_deref(),
        Some("wiki")
    );
    // 模块名自带连字符不得被误剥（尾段不是版本号）
    assert_eq!(
        mk("my-module-0.1.0/api.js")
            .0
            .module_of(mk("my-module-0.1.0/api.js").1)
            .as_deref(),
        Some("my-module")
    );
    // 只剩一层（release 根的裸 api.js）→ 无模块归属
    assert_eq!(mk("api.js").0.module_of(mk("api.js").1), None);
}

#[test]
fn module_of_keeps_version_like_module_name_in_dev_layout() {
    // dev（build 建表，release=false）：`preview-0.9` 是合法模块目录名，
    // 不得当版本后缀剥掉（v0.1.83——此前 dev/release 不分一律剥，openapi
    // tags / 生成客户端会被归到不存在的 `preview` 模块）。
    let dir = fixture(&[]);
    let mk = |file: &str| -> (RouteTable, FileId) {
        let (t, f) = RouteTable::from_entries(
            &dir,
            &[RouteEntry {
                method: "get".to_string(),
                pattern: "/v1/api/x".to_string(),
                file: file.to_string(),
                handler: None,
                schema: None,
            }],
        );
        assert!(f.is_empty(), "{f:?}");
        let id = t.listing()[0].file;
        (t, id)
    };
    let (t, id) = mk("preview-0.9/api.ts");
    // from_entries 即 release 布局：仍剥（既有语义不变）。
    assert_eq!(t.module_of(id).as_deref(), Some("preview"));
    // 同一文件在 dev 布局表：整段保留。
    let mut dev = t;
    dev.release = false;
    assert_eq!(dev.module_of(id).as_deref(), Some("preview-0.9"));
}

#[test]
fn rel_file_never_leaks_absolute_path() {
    // openapi.json 是公开产物：x-oj-file 走 rel_file，不得带本机绝对路径。
    let dir = fixture(&[]);
    let (t, f) = RouteTable::from_entries(
        &dir,
        &[RouteEntry {
            method: "get".to_string(),
            pattern: "/v1/api/x".to_string(),
            file: "wiki-0.1.0/api.js".to_string(),
            handler: None,
            schema: None,
        }],
    );
    assert!(f.is_empty(), "{f:?}");
    let id = t.listing()[0].file;
    let rel = t.rel_file(id);
    assert!(
        !rel.is_absolute(),
        "rel_file 不得是绝对路径: {}",
        rel.display()
    );
    assert_eq!(
        rel.to_string_lossy().replace('\\', "/"),
        "wiki-0.1.0/api.js"
    );
    // 溯源仍可用（绝对形态没丢）。
    assert!(t.file_path(id).is_absolute());
}

#[test]
fn audit_schemas_is_the_single_door_and_fails_on_bad_declaration() {
    // release 的 routes.js 可能来自第三方 .tgz 模块：其 .schema 属不可信输入，
    // 必须经 `audit_schemas` 兜底，不能只靠「本仓 build 时审过」。
    let dir = fixture(&[]);
    let entry = |schema: Option<serde_json::Value>| RouteEntry {
        method: "get".to_string(),
        pattern: "/v1/api/x".to_string(),
        file: "wiki-0.1.0/api.js".to_string(),
        handler: None,
        schema,
    };
    let (t, f) = RouteTable::from_entries(
        &dir,
        &[entry(Some(
            serde_json::json!({ "body": { "type": "object" } }),
        ))],
    );
    assert!(f.is_empty());
    assert!(t.audit_schemas().is_ok(), "合法契约放行");

    let (t2, f2) = RouteTable::from_entries(
        &dir,
        &[entry(Some(
            serde_json::json!({ "bodys": { "type": "object" } }),
        ))],
    );
    assert!(f2.is_empty());
    let err = t2.audit_schemas().unwrap_err();
    assert!(
        err.contains("bodys") && err.contains("unsupported channel"),
        "拼错通道必须被 audit_schemas 拦: {err}"
    );
    // 报错带 handler 链与相对文件，便于定位。
    assert!(err.contains("wiki-0.1.0/api.js"), "{err}");
}

#[test]
fn table_unknown_chain_tail_is_filtered() {
    // 链尾非动词（内省纵深防线）：不注册也不炸
    let (t, f) = tbl(&["a/api.ts"], &[("a/api.ts", "cccc.upload", "cccc")]);
    assert!(f.is_empty(), "{f:?}");
    assert!(t.listing().is_empty());
    assert!(matches!(
        t.lookup("/v1/api/a/cccc", "GET"),
        Lookup::NotFound
    ));
}

#[test]
fn decls_from_value_parses_flat_leaves() {
    let v = serde_json::json!([
        { "chain": "get", "route": null, "schema": null },
        {
            "chain": "cccc.get",
            "route": "cccc",
            "schema": { "query": { "type": "object" } },
        },
    ]);
    let d = decls_from_value(&v);
    assert_eq!(d.len(), 2, "{d:?}");
    assert_eq!(d[0].0, "get");
    assert_eq!(d[0].1, None);
    assert_eq!(d[1].0, "cccc.get");
    assert_eq!(d[1].1.as_deref(), Some("cccc"));
    assert!(d[1].2.is_some());
    // 旧对象形状 / 垃圾输入 → 空表（不炸）
    assert!(decls_from_value(&serde_json::json!({})).is_empty());
    assert!(decls_from_value(&serde_json::json!(5)).is_empty());
}

#[test]
fn entries_from_value_parses_handler_with_fallback() {
    let v = serde_json::json!([
        { "method": "get", "pattern": "/a", "file": "a/api.js", "handler": "cccc.get" },
        { "method": "post", "pattern": "/b", "file": "b/api.js" },
    ]);
    let e = entries_from_value(&v);
    assert_eq!(e[0].handler.as_deref(), Some("cccc.get"));
    assert_eq!(e[1].handler, None); // 旧产物缺字段
    let dir = fixture(&[]);
    let (t, f) = RouteTable::from_entries(&dir, &e);
    assert!(f.is_empty(), "{f:?}");
    let rows: Vec<&str> = t.listing().iter().map(|r| r.handler.as_str()).collect();
    assert!(rows.contains(&"cccc.get"), "{rows:?}");
    assert!(rows.contains(&"post"), "{rows:?}"); // None → method 兜底填链
}

#[test]
fn from_entries_rejects_illegal_handler_chain() {
    let dir = fixture(&[]);
    for bad in [
        "__proto__.get",
        "a.constructor",
        "a.prototype",
        "a//b",
        "",
        "a..get",
    ] {
        let e = vec![RouteEntry {
            method: "get".into(),
            pattern: "/x".into(),
            file: "a/api.js".into(),
            handler: Some(bad.into()),
            ..Default::default()
        }];
        let (t, f) = RouteTable::from_entries(&dir, &e);
        assert!(
            f.iter().any(|s| s.contains("handler")),
            "chain {bad:?}: {f:?}"
        );
        assert!(t.listing().is_empty(), "chain {bad:?} must not register");
    }
}

#[test]
fn from_entries_registers_nested_chain() {
    let dir = fixture(&[]);
    let e = vec![RouteEntry {
        method: "get".into(),
        pattern: "/api/x".into(),
        file: "a/api.js".into(),
        handler: Some("cccc.get".into()),
        ..Default::default()
    }];
    let (t, f) = RouteTable::from_entries(&dir, &e);
    assert!(f.is_empty(), "{f:?}");
    match t.lookup("/api/x", "GET") {
        Lookup::Hit { handler, .. } => assert_eq!(handler, "cccc.get"),
        _ => panic!("expected hit"),
    }
}

#[test]
fn table_registers_relative_and_rooted() {
    let (t, f) = tbl(
        &["user/account/api.ts"],
        &[("user/account/api.ts", "get", "")],
    );
    assert!(f.is_empty(), "{f:?}");
    assert!(matches!(
        t.lookup("/v1/api/user/account", "GET"),
        Lookup::Hit { .. }
    ));
    assert!(matches!(
        t.lookup("/v1/api/user/account", "POST"),
        Lookup::MethodNotAllowed
    ));
    assert!(matches!(t.lookup("/v1/api/none", "GET"), Lookup::NotFound));
    // 根级 api.ts：dir_base = base 本身（无尾斜杠）
    let (t2, f2) = tbl(&["api.ts"], &[("api.ts", "get", "")]);
    assert!(f2.is_empty(), "{f2:?}");
    assert!(matches!(t2.lookup("/v1/api", "GET"), Lookup::Hit { .. }));
}

#[test]
fn table_param_extraction_and_route_suffix() {
    let (t, _) = tbl(
        &["user/account/api.ts"],
        &[("user/account/api.ts", "get", "{id}")],
    );
    match t.lookup("/v1/api/user/account/42", "GET") {
        Lookup::Hit { params, .. } => assert_eq!(params["id"], "42"),
        _ => panic!("expected hit"),
    }
    // 挂 .route 后目录镜像不再注册（替换语义）
    assert!(matches!(
        t.lookup("/v1/api/user/account", "GET"),
        Lookup::NotFound
    ));
    let id = t.listing().iter().find(|r| r.method == "get").unwrap().file;
    assert!(t.is_replaced(t.file_path(id), "get"));
    assert!(!t.is_replaced(t.file_path(id), "post"));
}

#[test]
fn table_rooted_route_ignores_dir() {
    let (t, _) = tbl(
        &["legacy/compat/api.ts"],
        &[("legacy/compat/api.ts", "get", "/v2/user/{id}")],
    );
    assert!(matches!(
        t.lookup("/v1/api/v2/user/42", "GET"),
        Lookup::Hit { .. }
    ));
    assert!(matches!(
        t.lookup("/v1/api/legacy/compat", "GET"),
        Lookup::NotFound
    ));
}

#[test]
fn table_duplicate_is_conflict_500() {
    let (t, f) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[
            ("a/api.ts", "get", "/user/{id}"),
            ("b/api.ts", "get", "/user/{id}"),
        ],
    );
    assert!(f.iter().any(|s| s.contains("route conflict")), "{f:?}");
    assert!(matches!(
        t.lookup("/v1/api/user/1", "GET"),
        Lookup::Conflict(_)
    ));
    // 冲突 pattern 的其它 verb 仍 405 语义
    assert!(matches!(
        t.lookup("/v1/api/user/1", "POST"),
        Lookup::MethodNotAllowed
    ));
}

#[test]
fn table_merges_verbs_across_files() {
    let (t, f) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[("a/api.ts", "get", "/x"), ("b/api.ts", "post", "/x")],
    );
    assert!(f.is_empty(), "{f:?}");
    assert!(matches!(t.lookup("/v1/api/x", "GET"), Lookup::Hit { .. }));
    assert!(matches!(t.lookup("/v1/api/x", "POST"), Lookup::Hit { .. }));
}

#[test]
fn table_invalid_pattern_dropped_with_failure() {
    let (t, f) = tbl(&["a/api.ts"], &[("a/api.ts", "get", "{*p}/tail")]); // catch-all 非末尾
    assert!(!f.is_empty());
    assert!(matches!(t.lookup("/v1/api/a", "GET"), Lookup::NotFound));
}

#[test]
fn table_mixed_segment_patterns_rejected() {
    // axum 钉 matchit =0.8.4：参数段内混字面一律非法（{id}.json / v{major}.{minor}）
    // → 丢弃 + failures；0.8.6 放宽后此测试需反转（手册 §7.1）。
    let (t, f) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[
            ("a/api.ts", "get", "{id}.json"),
            ("b/api.ts", "get", "v{major}.{minor}"),
        ],
    );
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(f[0].contains("invalid route"), "{f:?}");
    assert!(matches!(
        t.lookup("/v1/api/a/42.json", "GET"),
        Lookup::NotFound
    ));
    assert!(matches!(
        t.lookup("/v1/api/b/v1.2", "GET"),
        Lookup::NotFound
    ));
}

#[test]
fn table_catch_all_needs_one_segment() {
    let (t, _) = tbl(&["file/api.ts"], &[("file/api.ts", "get", "{*path}")]);
    match t.lookup("/v1/api/file/a/b/c", "GET") {
        Lookup::Hit { params, .. } => assert_eq!(params["path"], "a/b/c"),
        _ => panic!("expected hit"),
    }
    assert!(matches!(t.lookup("/v1/api/file", "GET"), Lookup::NotFound));
}

#[test]
fn table_introspect_failure_skips_file() {
    let root = fixture(&["bad/api.ts", "good/api.ts"]);
    let (t, f) = RouteTable::build("/v1/api", &root, true, None, |p: &Path| {
        if p.ends_with("bad/api.ts") {
            Err("syntax error".into())
        } else {
            Ok(vec![("get".into(), None, None)])
        }
    });
    assert_eq!(f.len(), 1);
    assert!(matches!(
        t.lookup("/v1/api/good", "GET"),
        Lookup::Hit { .. }
    ));
    assert!(matches!(t.lookup("/v1/api/bad", "GET"), Lookup::NotFound));
}

#[test]
fn table_unmapped_verb_405_when_path_exists() {
    let (t, _) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "")]);
    assert!(matches!(
        t.lookup("/v1/api/u", "TRACE"),
        Lookup::MethodNotAllowed
    ));
    assert!(matches!(
        t.lookup("/v1/api/none", "TRACE"),
        Lookup::NotFound
    ));
}

#[test]
fn table_empty_route_string_means_unset() {
    // .route = "" 视同未挂：目录镜像照常注册
    let (t, _) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "")]);
    assert!(matches!(t.lookup("/v1/api/u", "GET"), Lookup::Hit { .. }));
    let (t2, _) = tbl(&["v/api.ts"], &[("v/api.ts", "get", "  ")]);
    // 非空但仅空白：作为字面 pattern 注册（不特判，文档写明空串视同未挂）
    assert!(matches!(
        t2.lookup("/v1/api/v/  ", "GET"),
        Lookup::Hit { .. }
    ));
}

// ----- 参数路由与静态兄弟的优先级（注册顺序无关） -----

#[test]
fn table_param_route_does_not_graft_later_static_siblings() {
    // 缺陷形态（upstream issue: matchit grafting）：参数路由先注册时，后到的静态兄弟
    // 曾被去重用的 at_mut(pattern) 当成实参匹配，把方法嫁接到 {pk} 节点上。
    let root = PathBuf::from("/r");
    let es = |m: &str, p: &str, f: &str| RouteEntry {
        method: m.into(),
        pattern: p.into(),
        file: f.into(),
        handler: None,
        schema: None,
    };
    let (t, failures) = RouteTable::from_entries(
        &root,
        &[
            es("del", "/x/admins/{pk}", "pk/api.js"),
            es("get", "/x/admins/me", "me/api.js"),
            es("get", "/x/admins/session", "session/api.js"),
            es("post", "/x/admins/sign-in", "sign-in/api.js"),
        ],
    );
    assert!(failures.is_empty(), "{failures:?}");
    let hit = |path: &str, verb: &str, expect: &str| match t.lookup(path, verb) {
        Lookup::Hit { file, .. } => assert_eq!(file, PathBuf::from(expect), "{path} {verb}"),
        other => panic!("{path} {verb} → {}", kind(&other)),
    };
    hit("/x/admins/me", "GET", "/r/me/api.js");
    hit("/x/admins/session", "GET", "/r/session/api.js");
    hit("/x/admins/sign-in", "POST", "/r/sign-in/api.js");
    hit("/x/admins/42", "DELETE", "/r/pk/api.js");
    // 真实静态段优先：me 不得被 {pk} 吞掉（反之亦然）
    assert!(matches!(
        t.lookup("/x/admins/me", "DELETE"),
        Lookup::MethodNotAllowed
    ));
}

#[test]
fn table_static_sibling_beats_same_verb_param() {
    // 最凶的一形态：参数与静态**同动词**。旧实现里 `{pk}` 先注册会把 `me` 判成
    // 假冲突（`at_mut` 把 me 当实参），该 (pattern, method) 直接 500。
    let (t, failures) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[("a/api.ts", "get", "/x/{pk}"), ("b/api.ts", "get", "/x/me")],
    );
    assert!(failures.is_empty(), "{failures:?}");
    match t.lookup("/v1/api/x/me", "GET") {
        // 静态命中不得带参数（被嫁接时会命中 {pk} 节点、带上 pk=me）
        Lookup::Hit { file, params, .. } => {
            assert!(file.ends_with("b/api.ts"), "{file:?}");
            assert!(params.is_empty(), "{params:?}");
        }
        other => panic!("{}", kind(&other)),
    }
    match t.lookup("/v1/api/x/42", "GET") {
        Lookup::Hit { file, params, .. } => {
            assert!(file.ends_with("a/api.ts"), "{file:?}");
            assert_eq!(params["pk"], "42");
        }
        other => panic!("{}", kind(&other)),
    }
}

#[test]
fn table_conflict_survives_third_declaration() {
    // 冲突哨兵必须钉死：第三个文件再声明同一 (pattern, method) 不得把它冲回 File
    // （否则请求从 500 静默变 200，且指向第三个文件）。
    let root = PathBuf::from("/r");
    let (t, failures) = RouteTable::from_entries(
        &root,
        &[
            RouteEntry {
                method: "get".into(),
                pattern: "/a/{id}".into(),
                file: "a/api.js".into(),
                handler: None,
                schema: None,
            },
            RouteEntry {
                method: "get".into(),
                pattern: "/a/{id}".into(),
                file: "b/api.js".into(),
                handler: None,
                schema: None,
            },
            RouteEntry {
                method: "get".into(),
                pattern: "/a/{id}".into(),
                file: "c/api.js".into(),
                handler: None,
                schema: None,
            },
        ],
    );
    assert_eq!(failures.len(), 2, "{failures:?}");
    match t.lookup("/a/1", "GET") {
        Lookup::Conflict(msg) => assert!(msg.contains("a/api.js"), "{msg}"),
        other => panic!("{}", kind(&other)),
    }
}

#[test]
fn table_differing_param_names_at_same_slot_conflict() {
    // 同位置**异名**参数是结构性冲突（设计 §5）：后来者不进树。
    // 修复前靠 at_mut 命中同类节点而被静默合并，与文档口径不符。
    let (t, failures) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[
            ("a/api.ts", "get", "/x/{id}"),
            ("b/api.ts", "get", "/x/{name}"),
        ],
    );
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("invalid route"), "{failures:?}");
    assert!(matches!(t.lookup("/v1/api/x/1", "GET"), Lookup::Hit { .. }));
}

#[test]
fn table_build_keeps_static_siblings_registered_after_param() {
    // dev/release 共用的 build 路径：文件按路径排序，a→b→c 即「参数在前、静态在后」。
    let (t, failures) = tbl(
        &["a/api.ts", "b/api.ts", "c/api.ts", "d/api.ts"],
        &[
            ("a/api.ts", "del", "/admins/{pk}"),
            ("b/api.ts", "get", "/admins/me"),
            ("c/api.ts", "get", "/admins/session"),
            ("d/api.ts", "post", "/admins/sign-in"),
        ],
    );
    assert!(failures.is_empty(), "{failures:?}");
    let hit = |path: &str, verb: &str, expect: &str| match t.lookup(path, verb) {
        Lookup::Hit { file, .. } => assert!(file.ends_with(expect), "{file:?} vs {expect}"),
        other => panic!("{path} {verb} → {}", kind(&other)),
    };
    hit("/v1/api/admins/me", "GET", "b/api.ts");
    hit("/v1/api/admins/session", "GET", "c/api.ts");
    hit("/v1/api/admins/sign-in", "POST", "d/api.ts");
    hit("/v1/api/admins/42", "DELETE", "a/api.ts");
    // rows/listing 须与查表一致（嫁接时 rows 会列出实际不在树里的行）
    assert_eq!(t.listing().len(), 4, "{:?}", t.listing().len());
}

fn kind(l: &Lookup) -> &'static str {
    match l {
        Lookup::Hit { .. } => "Hit",
        Lookup::Conflict(_) => "Conflict",
        Lookup::MethodNotAllowed => "MethodNotAllowed",
        Lookup::NotFound => "NotFound",
    }
}

// ----- release 直载（routes.js）-----

#[test]
fn from_entries_registers_and_conflicts() {
    let root = PathBuf::from("/r");
    let es = |m: &str, p: &str, f: &str| RouteEntry {
        method: m.into(),
        pattern: p.into(),
        file: f.into(),
        handler: None,
        schema: None,
    };
    let (t, failures) = RouteTable::from_entries(
        &root,
        &[
            es("get", "/a/{id}", "a/api.js"),
            es("post", "/a/{id}", "a/api.js"),      // 跨方法合并
            es("get", "/a/{id}", "b/api.js"),       // 同 (pattern, method) → 冲突（请求期 500）
            es("get", "/bad/{*x}tail", "c/api.js"), // matchit 拒绝 → failure
            es("brew", "/a", "d/api.js"),           // 未知方法 → failure
        ],
    );
    assert_eq!(failures.len(), 3, "{failures:?}");
    assert!(matches!(t.lookup("/a/1", "GET"), Lookup::Conflict(_)));
    assert!(matches!(t.lookup("/a/1", "POST"), Lookup::Hit { .. }));
    assert!(matches!(t.lookup("/a/1", "PUT"), Lookup::MethodNotAllowed));
    // 无冲突表：Hit 的 file 相对 root 解析
    let (t2, _) = RouteTable::from_entries(&root, &[es("get", "/a/{id}", "a/api.js")]);
    let Lookup::Hit { file, .. } = t2.lookup("/a/1", "GET") else {
        panic!()
    };
    assert_eq!(file, PathBuf::from("/r/a/api.js"));
    // release 无 fs 兜底：表外路径 404
    assert!(matches!(t.lookup("/nope", "GET"), Lookup::NotFound));
}

#[test]
fn entries_from_value_parses_and_skips() {
    let v = serde_json::json!([
        { "method": "get", "pattern": "/a/{id}", "file": "a/api.js" },
        { "method": 1 },  // 缺字段/类型错 → 跳过
        "junk",
    ]);
    let es = entries_from_value(&v);
    assert_eq!(es.len(), 1);
    assert_eq!(es[0].pattern, "/a/{id}");
    assert!(entries_from_value(&serde_json::json!(null)).is_empty());
}

#[test]
fn from_entries_rejects_traversal_and_bad_pattern() {
    let root = PathBuf::from("/r");
    let es = |m: &str, p: &str, f: &str| RouteEntry {
        method: m.into(),
        pattern: p.into(),
        file: f.into(),
        handler: None,
        schema: None,
    };
    let (t, failures) = RouteTable::from_entries(
        &root,
        &[
            es("get", "/a/{id}", "../etc/passwd"), // 穿越
            es("get", "/a/{id}", "a/../b.js"),     // 中段 ..
            es("get", "/a/{id}", "a\\b.js"),       // 反斜杠
            es("get", "/a//x", "a/api.js"),        // pattern 空段
            es("get", "a/x", "a/api.js"),          // pattern 无首斜杠
            es("get", "/a/{id}", "a/api.js"),      // 合法行仍注册
        ],
    );
    assert_eq!(failures.len(), 5, "{failures:?}");
    assert!(
        failures.iter().all(|f| f.contains("illegal")),
        "{failures:?}"
    );
    assert!(matches!(t.lookup("/a/1", "GET"), Lookup::Hit { .. }));
}
