use super::*;

#[test]
fn strip_route_removes_assignments_only() {
    let src =
        "function get() {}\nget.route = \"{id}\";\nconst r = x.route;\nexport default { get };\n";
    let out = strip_route_decls(src);
    assert!(!out.contains(".route ="), "{out}");
    assert!(out.contains("function get()"), "{out}");
    assert!(out.contains("x.route"), "{out}"); // 读取不剥
}

/// 测试辅助：搭一棵真实 src 树（改写要探盘，字符串单测已不成立）。
/// 返回 (src 根, 临时根)。
fn fix_fixture(tag: &str, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let t = std::env::temp_dir().join(format!(
        "oj-fix-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&t);
    let src = t.join("src");
    for (rel, content) in files {
        let p = src.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    (src, t)
}

/// 测试辅助：模块 a（0.1.0）+ b（0.2.0）双模块夹具，覆盖索引目录/别名/跨模块。
fn two_modules(tag: &str) -> (PathBuf, PathBuf) {
    fix_fixture(
        tag,
        &[
            ("a/manifest.yaml", "name: a\ndesc: d\nversion: 0.1.0\n"),
            ("b/manifest.yaml", "name: b\ndesc: d\nversion: 0.2.0\n"),
            ("b/util.ts", "export const v = 1;\n"),
            ("a/_shared/validate.ts", "export const v = 1;\n"),
            ("a/_shared/mod/index.ts", "export const x = 1;\n"),
            ("a/y/g.ts", "export const g = 1;\n"),
            ("a/sub/_shared/deep.ts", "export const d = 1;\n"),
        ],
    )
}

/// 测试辅助：在模块 a 的某目录下改写（模块 a 0.1.0、给定版本视图）。
fn fix_in(
    src_root: &Path,
    src: &str,
    rel_dir: &str,
    view: &std::collections::BTreeMap<String, String>,
) -> String {
    fix_import_specifiers(src_root, src, "a", "0.1.0", rel_dir, view, None).unwrap()
}

/// 版本视图 {b: 0.2.0}。
fn view_b() -> std::collections::BTreeMap<String, String> {
    [("b".to_string(), "0.2.0".to_string())]
        .into_iter()
        .collect()
}

#[test]
fn relative_imports_rewritten_to_product_paths() {
    let (src_root, t) = fix_fixture(
        "rel",
        &[
            ("a/manifest.yaml", "name: a\ndesc: d\nversion: 0.1.0\n"),
            ("a/_shared/validate.ts", "export const v = 1;\n"),
            ("a/_shared/deep.ts", "export const d = 1;\n"),
        ],
    );
    let src = "import { v } from \"../_shared/validate\";\nimport p from \"pkg\";\nexport { v } from \"../_shared/validate.ts\";\nconst s = \"from \\\"../_shared/validate\\\"\";\n";
    let out = fix_in(&src_root, src, "item", &Default::default());
    assert!(out.contains("\"../_shared/validate.js\""), "{out}");
    assert!(out.contains("from \"pkg\""), "{out}"); // npm 裸包名不动
    // 普通字符串里的 `from "…"` 不动（非导入位置——此前行级口径会误伤）
    assert!(
        out.contains(r#"const s = "from \"../_shared/validate\"";"#),
        "{out}"
    );
    // 深层目录：跨目录上溯仍正确
    let out = fix_in(
        &src_root,
        "import { d } from \"../../../_shared/deep\";\n",
        "item/x/y",
        &Default::default(),
    );
    assert!(out.contains("\"../../../_shared/deep.js\""), "{out}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 非 `.ts` 本地目标（`.js`/`.json`）不进产物（collect_module 白名单只收 `.ts`），
/// 此前会静默产出悬空 specifier、release 才炸 → 现构建期 fail-fast。
#[test]
fn non_ts_local_target_fails_the_build() {
    let (src_root, t) = fix_fixture(
        "ext",
        &[
            ("a/manifest.yaml", "name: a\ndesc: d\nversion: 0.1.0\n"),
            ("a/item/plain.js", "export const p = 1;\n"),
            ("a/plain.js", "export const p = 1;\n"),
            ("a/item/d.json", "{}\n"),
        ],
    );
    for spec in ["./plain.js", "./d.json", "#plain.js"] {
        let e = fix_import_specifiers(
            &src_root,
            &format!("import x from \"{spec}\";\n"),
            "a",
            "0.1.0",
            "item",
            &Default::default(),
            None,
        )
        .unwrap_err();
        assert!(e.contains("不会进产物") && e.contains(spec), "{spec}: {e}");
    }
    let _ = std::fs::remove_dir_all(&t);
}

/// 产物自洽断言：漏改写的**本地** specifier（扫描器未覆盖的写法）必须在构建期暴露，
/// 而不是留到 release 运行期才炸。
#[test]
fn dist_consistency_assertion_catches_dangling_local_specifier() {
    let t = std::env::temp_dir().join(format!("oj-dist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    let vdir = t.join("dist/m-0.1.0");
    std::fs::create_dir_all(&vdir).unwrap();
    std::fs::write(
        vdir.join("api.js"),
        "import x from \"./missing.js\";\nimport y from \"pkg\";\n",
    )
    .unwrap();
    // 裸包名不在自洽面（运行期 node_modules 解析）
    assert!(assert_dist_consistent(std::slice::from_ref(&vdir)).is_err());
    std::fs::write(vdir.join("missing.js"), "export const x = 1;\n").unwrap();
    assert!(assert_dist_consistent(std::slice::from_ref(&vdir)).is_ok());
    // 残留别名同样被兜住
    std::fs::write(vdir.join("api.js"), "import x from \"#_shared/a\";\n").unwrap();
    let e = assert_dist_consistent(std::slice::from_ref(&vdir)).unwrap_err();
    assert!(e.contains("别名未实化"), "{e}");
    // 未参与本次构建的目录不扫（陈旧产物不该让本次构建失败）
    assert!(assert_dist_consistent(&[]).is_ok());
    let _ = std::fs::remove_dir_all(&t);
}

/// 回归：目录索引导入（`../_shared/mod` → `_shared/mod/index.ts`）。
/// 修复前构建期只做字符串补后缀 → 产出悬空的 `../_shared/mod.js`（dev 能跑、release 炸）。
#[test]
fn directory_index_import_maps_to_index_js() {
    let (src_root, t) = two_modules("index");
    let out = fix_in(
        &src_root,
        "import { x } from \"../_shared/mod\";\n",
        "item",
        &Default::default(),
    );
    assert!(out.contains("\"../_shared/mod/index.js\""), "{out}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 别名实化：`#` 模块根 / `#/` src 根，产物内只剩相对 specifier。
#[test]
fn alias_imports_materialized_to_product_paths() {
    let (src_root, t) = two_modules("alias");
    let src = "import { v } from \"#_shared/validate\";\nimport { d } from \"#_shared/mod\";\nimport { u } from \"#/b/util\";\n";
    let out = fix_in(&src_root, src, "item/x/y", &view_b());
    // 本模块根锚点：与深度无关地落到同模块版本目录
    assert!(out.contains("\"../../../_shared/validate.js\""), "{out}");
    // 索引目录
    assert!(out.contains("\"../../../_shared/mod/index.js\""), "{out}");
    // src 根锚点 → 跨模块按 view 钉版本目录
    assert!(out.contains("\"../../../../b-0.2.0/util.js\""), "{out}");
    assert!(!out.contains('#'), "{out}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 副作用 import 与动态 import 一并改写（修复前只有行级 `from ` 口径会漏）。
#[test]
fn side_effect_and_dynamic_imports_are_rewritten() {
    let (src_root, t) = two_modules("dyn");
    let src = "import \"#_shared/validate\";\nconst m = await import(\"#_shared/mod\");\nimport(\"../_shared/validate\");\nconst route = `#/dashboard`;\n";
    let out = fix_in(&src_root, src, "item", &Default::default());
    assert!(out.contains("import \"../_shared/validate.js\""), "{out}");
    assert!(out.contains("import(\"../_shared/mod/index.js\")"), "{out}");
    assert!(!out.contains("#_shared"), "{out}");
    // 模板串里的 `#/…` 形状文本不动（非导入位置；hash 路由等业务字符串常见）
    assert!(out.contains("`#/dashboard`"), "{out}");
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn cross_module_import_rewrites_to_versioned_path() {
    let (src_root, t) = two_modules("xmod");
    // ① 模块根出发：../b/util → dist/b-0.2.0/util.js
    let out = fix_in(
        &src_root,
        "import { v } from \"../b/util\";\n",
        "",
        &view_b(),
    );
    assert!(out.contains("\"../b-0.2.0/util.js\""), "{out}");
    // ①' 子目录出发：../../b/util → 同样落到 dist/b-0.2.0/
    let out = fix_in(
        &src_root,
        "import { v } from \"../../b/util\";\n",
        "sub",
        &view_b(),
    );
    assert!(out.contains("\"../../b-0.2.0/util.js\""), "{out}");
    // ③ 嵌套 rel_dir：src/a/x/y/f.ts 导入 ../../../b/util
    let out = fix_in(
        &src_root,
        "import { v } from \"../../../b/util\";\n",
        "x/y",
        &view_b(),
    );
    assert!(out.contains("\"../../../b-0.2.0/util.js\""), "{out}");
    // 显式 .ts 后缀目标 → .js
    let out = fix_in(
        &src_root,
        "export { v } from \"../b/util.ts\";\n",
        "",
        &view_b(),
    );
    assert!(out.contains("\"../b-0.2.0/util.js\""), "{out}");
    // 模块内绕出再绕回（../../a/y/g 从 x/ 出发）→ 产物路径不悬空
    let out = fix_in(
        &src_root,
        "import { v } from \"../../a/y/g\";\n",
        "x",
        &Default::default(),
    );
    assert!(out.contains("\"../y/g.js\""), "{out}");
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn cross_module_import_without_version_fails_fast() {
    let (src_root, t) = two_modules("nover");
    // ② 视图缺 b → Err 报目标模块并提示先构建
    let e = fix_import_specifiers(
        &src_root,
        "import { v } from \"../b/util\";\n",
        "a",
        "0.1.0",
        "",
        &Default::default(),
        None,
    )
    .unwrap_err();
    assert!(e.contains("b") && e.contains("oj build"), "{e}");
    // 别名跨模块同样受版本门禁（`#/b/…` 与相对写法等价）
    let e = fix_import_specifiers(
        &src_root,
        "import { v } from \"#/b/util\";\n",
        "a",
        "0.1.0",
        "",
        &Default::default(),
        None,
    )
    .unwrap_err();
    assert!(e.contains("b") && e.contains("oj build"), "{e}");
    // 目标不存在 → Err 含尝试过的候选（探针口径）
    let e = fix_import_specifiers(
        &src_root,
        "import { v } from \"../b/nope\";\n",
        "a",
        "0.1.0",
        "",
        &view_b(),
        None,
    )
    .unwrap_err();
    assert!(e.contains("tried"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn rel_pattern_rules() {
    // 镜像行：模块名 + 目录段
    assert_eq!(rel_pattern("user", "account", None), "user/account");
    assert_eq!(rel_pattern("user", "", None), "user");
    assert_eq!(
        rel_pattern("user", "profile/detail", None),
        "user/profile/detail"
    );
    // 相对 .route 声明
    assert_eq!(rel_pattern("user", "item", Some("{id}")), "user/item/{id}");
    assert_eq!(rel_pattern("user", "", Some("{id}")), "user/{id}");
    // 根级声明（/ 开头）：剥首斜杠，不加模块段
    assert_eq!(
        rel_pattern("user", "item", Some("/v2/user/{id}")),
        "v2/user/{id}"
    );
    // 空 route 视同未挂
    assert_eq!(rel_pattern("user", "item", Some("")), "user/item");
}

#[test]
fn rel_pattern_converts_underscore_dirs_and_module() {
    // 目录镜像：rel_dir 与模块段逐段转换（v0.1.27 `_name_` → `{name}`）
    assert_eq!(rel_pattern("user", "_id_", None), "user/{id}");
    assert_eq!(rel_pattern("_mod_", "x", None), "{mod}/x");
    assert_eq!(rel_pattern("_mod_", "_id_", None), "{mod}/{id}");
    // 相对 .route 拼在转换后的目录后
    assert_eq!(
        rel_pattern("user", "_id_", Some("{sub}")),
        "user/{id}/{sub}"
    );
    // 根级 .route 不吃目录转换
    assert_eq!(rel_pattern("user", "_id_", Some("/v2/x")), "v2/x");
    // .route 值不转换：`_name_` 在其中是字面段
    assert_eq!(rel_pattern("user", "item", Some("_id_")), "user/item/_id_");
    // 不转换的形态保持字面
    assert_eq!(rel_pattern("user", "_shared", None), "user/_shared");
    assert_eq!(rel_pattern("user", "__x__", None), "user/__x__");
}

#[test]
fn residual_alias_fails_the_build() {
    // 残留断言（扫描器天花板兜底）：产物里还有 `#` specifier → 显式失败
    assert!(assert_no_aliases("import x from \"#_shared/a\";\n", "m/api.js").is_err());
    assert!(assert_no_aliases("import x from \"./a.js\";\n", "m/api.js").is_ok());
    // 注释里的 `#` 不算（非导入位置）
    assert!(assert_no_aliases("// import x from \"#a\"\n", "m/api.js").is_ok());
}

#[test]
fn guard_rejects_api_imports() {
    let files = vec![
        (
            "_shared/util.ts".into(),
            "import { g } from \"../account/api\";\n".into(),
        ),
        (
            "account/api.ts".into(),
            "import { v } from \"../_shared/validate\";\n".into(),
        ),
    ];
    let e = guard_no_api_imports(&files).unwrap_err();
    assert!(
        e.contains("_shared/util.ts") && e.contains("../account/api"),
        "{e}"
    );
    // 无违规
    assert!(guard_no_api_imports(&[("x.ts".into(), "import m from \"pkg\";".into())]).is_ok());
}

/// 别名写法不得绕过 api.ts 禁令（此前守卫只看相对 specifier，换个前缀即可溜过）。
#[test]
fn guard_rejects_api_imports_via_alias() {
    let files = vec![
        (
            "_shared/util.ts".into(),
            "import { g } from \"#item/api\";\n".into(),
        ),
        (
            "account/api.ts".into(),
            "import { g } from \"#/user/item/api\";\n".into(),
        ),
    ];
    let e = guard_no_api_imports(&files).unwrap_err();
    assert!(
        e.contains("_shared/util.ts") && e.contains("#item/api"),
        "{e}"
    );
    assert!(e.contains("#/user/item/api"), "{e}");
    // npm 包内子路径 `pkg/api` 不在守卫面（那是第三方包的入口，不是本项目的 api.ts）
    assert!(guard_no_api_imports(&[("x.ts".into(), "import a from \"pkg/api\";".into())]).is_ok());
    // 名字含 api 但不是 api.ts（api-helper）不误伤
    assert!(
        guard_no_api_imports(&[(
            "x.ts".into(),
            "import a from \"#_shared/api-helper\";".into()
        )])
        .is_ok()
    );
}

/// 测试辅助：目录下唯一文件的文件名（String）。
fn only_file(dir: &std::path::Path) -> String {
    let mut it = std::fs::read_dir(dir).unwrap();
    let name = it.next().unwrap().unwrap().file_name();
    assert!(
        it.next().is_none(),
        "expected exactly one file in {}",
        dir.display()
    );
    name.to_string_lossy().into_owned()
}

/// 测试辅助：摆一个 src（user 带 .route + _shared，other 纯镜像）。
fn src_fixture(t: &std::path::Path) {
    let src = t.join("src");
    for d in ["user/item", "user/_shared", "other/list"] {
        std::fs::create_dir_all(src.join(d)).unwrap();
    }
    std::fs::write(
        src.join("user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        src.join("other/manifest.yaml"),
        "name: other\ndesc: d\nversion: 0.9.0\n",
    )
    .unwrap();
    std::fs::write(
        src.join("user/_shared/validate.ts"),
        "export const v = 1;\n",
    )
    .unwrap();
    std::fs::write(src.join("user/item/api.ts"),
            "import { v } from \"../_shared/validate\";\nfunction get(){ json.ok({v}); }\nget.route = \"{id}\";\nexport default { get };\n").unwrap();
    std::fs::write(
        src.join("other/list/api.ts"),
        "function get(){ json.ok({}); }\nexport default { get };\n",
    )
    .unwrap();
}

fn build_args(t: &std::path::Path, module: Option<&str>) -> BuildArgs {
    BuildArgs {
        module: module.map(str::to_string),
        config: Some(t.join("config.yaml").display().to_string()),
        dir: t.join("src").display().to_string(),
        module_root: None,
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    }
}

/// `--module-root` 形态的 BuildArgs（与 `-d` 互斥，故 dir 字段不再参与）。
fn module_root_args(t: &std::path::Path, module: Option<&str>) -> BuildArgs {
    BuildArgs {
        module: module.map(str::to_string),
        config: Some(t.join("config.yaml").display().to_string()),
        dir: String::new(),
        module_root: Some(t.join("src").display().to_string()),
        out: t.join("dist").display().to_string(),
        minify: true,
        check: false,
        bundle: false,
    }
}

/// 嵌套模块根夹具：模块根在第二层（目录名 ≠ 模块名），m2 声明了 URL 前缀。
/// m1 跨模块引用 m2（走 `#/m2/…` 别名，已声明 deps）。
fn nested_fixture(t: &std::path::Path) {
    let src = t.join("src");
    std::fs::create_dir_all(src.join("foo/api/xxx/yyy")).unwrap();
    std::fs::create_dir_all(src.join("bar/api/shared")).unwrap();
    std::fs::write(
        src.join("foo/api/manifest.yaml"),
        "name: m1\ndesc: d\nversion: 0.1.0\ndeps:\n  m2: \"^0.2.0\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("bar/api/manifest.yaml"),
        "name: m2\ndesc: d\nversion: 0.2.0\nprefix: sys/user\n",
    )
    .unwrap();
    std::fs::write(src.join("bar/api/shared/u.ts"), "export const u = 1;\n").unwrap();
    std::fs::write(
        src.join("bar/api/api.ts"),
        "function get(){ json.ok({}); }\nexport default { get };\n",
    )
    .unwrap();
    std::fs::write(
            src.join("foo/api/xxx/yyy/api.ts"),
            "import { u } from \"#/m2/shared/u\";\nfunction get(){ json.ok({u}); }\nexport default { get };\n",
        )
        .unwrap();
}

/// routes.js 里的 pattern 清单（产物形态：`pattern: "xxx"`）。
fn routes_patterns(vdir: &std::path::Path) -> Vec<String> {
    let txt = std::fs::read_to_string(vdir.join("routes.js")).unwrap();
    txt.match_indices("pattern: \"")
        .map(|(i, _)| {
            let rest = &txt[i + "pattern: \"".len()..];
            rest[..rest.find('"').unwrap()].to_string()
        })
        .collect()
}

/// BDD：模块根可不在首层时，URL 的模块段取 `manifest.yaml` 的 `prefix`
/// （缺省 = name），目录名（`foo/api`）不进 URL；产物路径以模块根为基。
#[tokio::test]
async fn given_nested_module_root_when_build_then_url_uses_prefix_and_dir_names_absent() {
    let t = std::env::temp_dir().join(format!("oj-build-nested-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    nested_fixture(&t);
    run(&module_root_args(&t, None)).await.unwrap();

    // Then ①：URL 前缀 = manifest 的 prefix（缺省 = name），目录名不出现
    let m1 = std::fs::read_to_string(t.join("dist/m1-0.1.0/routes.js")).unwrap();
    assert!(m1.contains("pattern: \"m1/xxx/yyy\""), "{m1}");
    assert!(m1.contains("file: \"xxx/yyy/api.js\""), "{m1}");
    let m2 = std::fs::read_to_string(t.join("dist/m2-0.2.0/routes.js")).unwrap();
    assert!(m2.contains("pattern: \"sys/user\""), "{m2}");

    // Then ②：跨模块别名按**模块名**解析并落到目标版本目录（目录名不进产物路径）
    let api = std::fs::read_to_string(t.join("dist/m1-0.1.0/xxx/yyy/api.js")).unwrap();
    assert!(api.contains("m2-0.2.0/shared/u.js"), "{api}");
    assert!(!api.contains("bar"), "{api}");
    assert!(!api.contains('#'), "{api}");

    // Then ③：位置参数是**模块名**（`oj build m1`），不是目录名
    std::fs::remove_dir_all(t.join("dist")).unwrap();
    // 先出 m1 的依赖（deps 校验口径），再单出 m1
    run(&module_root_args(&t, Some("m2"))).await.unwrap();
    run(&module_root_args(&t, Some("m1"))).await.unwrap();
    assert!(t.join("dist/m1-0.1.0").is_dir());
    // 单模块构建不牵连他人：版本锁保留既有条目（PRD §7-7）
    let lock = std::fs::read_to_string(t.join("dist/manifests.yaml")).unwrap();
    assert!(lock.contains("m2: 0.2.0"), "{lock}");
    assert!(lock.contains("m1: 0.1.0"), "{lock}");
    // 目录名当模块名用不了（索引里只有 m1/m2）
    let e = run(&module_root_args(&t, Some("foo"))).await.unwrap_err();
    assert!(e.contains("foo"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（PRD §7-1 兼容）：**旧布局**下 `--module-root src` 的产物与 `-d src` 逐字节一致
/// （oj 为确定性构建；`.tgz` 含打包时间戳，只比存在性与文件名）。
#[tokio::test]
async fn given_flat_layout_when_module_root_then_dist_is_byte_identical_to_plain_d() {
    let t = std::env::temp_dir().join(format!("oj-build-compat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);

    run(&build_args(&t, None)).await.unwrap();
    let a = dist_snapshot(&t.join("dist"));
    std::fs::remove_dir_all(t.join("dist")).unwrap();

    run(&module_root_args(&t, None)).await.unwrap();
    let b = dist_snapshot(&t.join("dist"));

    assert!(!a.is_empty(), "夹具应产出文件");
    assert_eq!(a, b, "`--module-root` 在旧布局下不得改变产物");
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（PRD §7-6）：`--module-root` 命中 0 个模块 → 非零（Err）+ 可读下一步，
/// 不得像默认口径那样静默成功（静默会产出空 dist 并让下游误判「构建通过」）。
#[tokio::test]
async fn given_module_root_without_any_manifest_when_build_then_fails_with_next_step() {
    let t = std::env::temp_dir().join(format!("oj-build-nohit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/web")).unwrap();
    std::fs::write(t.join("src/web/index.ts"), "export const a = 1;\n").unwrap();

    let e = run(&module_root_args(&t, None)).await.unwrap_err();
    assert!(
        e.contains("no manifest.yaml") && e.contains("--module-root"),
        "{e}"
    );
    assert!(e.contains("下一步"), "{e}");
    // 对照：默认口径（`-d`）对空树仍是今日语义（0 模块亦成功），故不在此断言。
    assert!(!t.join("dist").exists(), "失败不得落盘半成品");
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（评审 R2 / 实测事故）：`--module-root` 指到 **release 产物树** → fail-fast。
/// 产物树的 `manifests.yaml` 是版本锁、子目录是 `<name>-<version>/`，按模块根扫会
/// 静默产出一堆空壳版本目录（0 api file 却「构建成功」）。
#[tokio::test]
async fn given_release_tree_when_module_root_then_fails_fast() {
    let t = std::env::temp_dir().join(format!("oj-build-reltree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    run(&build_args(&t, None)).await.unwrap();

    let mut a = module_root_args(&t, None);
    a.module_root = Some(t.join("dist").display().to_string());
    let e = run(&a).await.unwrap_err();
    assert!(e.contains("--module-root is a source-tree option"), "{e}");
    assert!(e.contains("release tree"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

/// dist 快照：相对路径 → 内容（`.tgz` 只记文件名，内含打包时间戳不可比）。
fn dist_snapshot(dist: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    let mut stack = vec![dist.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(dist).unwrap().display().to_string();
            let v = if rel.ends_with(".tgz") {
                String::from("<tgz>")
            } else {
                std::fs::read_to_string(&p).unwrap_or_default()
            };
            m.insert(rel, v);
        }
    }
    m
}

/// BDD：dev 路由表与 release 产物 URL **逐条一致**（评审发现丙：两者会分叉）。
#[tokio::test]
async fn given_nested_module_root_when_serve_dev_then_urls_match_release_artifacts() {
    let t = std::env::temp_dir().join(format!("oj-build-urls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    nested_fixture(&t);
    // canonicalize：/var → /private/var（macOS），保证与 loader 看到的路径同形态
    let src = t.join("src").canonicalize().unwrap();
    let idx = only_js::bridge::resolve_roots(&src, true, None).unwrap();
    run(&module_root_args(&t, None)).await.unwrap();

    // When：dev 侧按同一份索引建路由表（`oj serve --module-root` / `oj openapi` 同路径）
    let table = crate::openapi_cmd::discover_routes(&src, &t, true, "/v1/api", Some(&idx)).unwrap();
    let mut dev: Vec<String> = table
        .listing()
        .iter()
        .map(|r| r.pattern.trim_start_matches("/v1/api/").to_string())
        .collect();
    dev.sort();

    // Then：与 release 产物的 pattern 逐条一致（去掉前导 base 后比对）
    let mut rel = routes_patterns(&t.join("dist/m1-0.1.0"));
    rel.extend(routes_patterns(&t.join("dist/m2-0.2.0")));
    rel.sort();
    assert_eq!(dev, rel, "dev 与 release 的 URL 必须同源");
    assert!(dev.iter().any(|p| p == "m1/xxx/yyy"), "{dev:?}");
    assert!(dev.iter().any(|p| p == "sys/user"), "{dev:?}");
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（T10，评审 F2）：src/tasks 存在 → 全部 .ts 转译镜像到 dist/tasks/
/// （保目录结构、.ts→.js、相对 import 补 .js 后缀；共享库一并镜像）；
/// 不进 tgz/manifests（任务非版本化模块）。
#[tokio::test]
async fn given_src_with_tasks_when_build_then_dist_tasks_transpiled() {
    let t = std::env::temp_dir().join(format!("oj-build-tasks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("src/tasks/_shared")).unwrap();
    std::fs::write(
            t.join("src/tasks/task_demo.ts"),
            "import { tick } from \"./_shared/tick\";\nimport \"./_shared/side\";\nconst n: number = 1;\nwhile (true) { await Promise.resolve(tick(n)); }\n",
        )
        .unwrap();
    std::fs::write(t.join("src/tasks/_shared/side.ts"), "export {};\n").unwrap();
    std::fs::write(
        t.join("src/tasks/_shared/tick.ts"),
        "export function tick(n: number): number { return n; }\n",
    )
    .unwrap();
    run(&build_args(&t, None)).await.unwrap();
    let demo = std::fs::read_to_string(t.join("dist/tasks/task_demo.js")).unwrap();
    assert!(demo.contains("let n=1"), "{demo}"); // 类型已剥（转译产物，默认 minify）
    assert!(
        demo.contains("\"./_shared/tick.js\"") && demo.contains("\"./_shared/side.js\""),
        "{demo}"
    ); // 具名 + 副作用 import 均已补 .js 后缀（审查 #7）
    assert!(
        std::fs::read_to_string(t.join("dist/tasks/_shared/tick.js"))
            .unwrap()
            .contains("function tick("),
        "shared lib transpiled"
    );
    // 非版本化：不落锁、不打 tgz。
    let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert!(!lock.contains_key("tasks"), "{lock:?}");
    assert!(!t.join("dist/tasks.tgz").exists());
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD（T10）：无 src/tasks → 不产生 dist/tasks。
#[tokio::test]
async fn given_src_without_tasks_when_build_then_no_tasks_dir() {
    let t = std::env::temp_dir().join(format!("oj-build-notasks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    run(&build_args(&t, None)).await.unwrap();
    assert!(!t.join("dist/tasks").exists());
    let _ = std::fs::remove_dir_all(&t);
}

/// v0.1.64 嵌套 handler 组：routes.js 为嵌套 verb 落 `handler` 链；
/// 顶层动词（链 == 方法）不写该字段（旧产物字节形态不变）。
#[tokio::test]
async fn build_emits_handler_chain_for_nested_groups() {
    let t = std::env::temp_dir().join(format!("oj-build-nest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/user/item")).unwrap();
    std::fs::write(
        t.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/user/item/api.ts"),
        "function get(){ json.ok({}); }\n\
             const upload = { post: () => { json.ok({}); } };\n\
             upload.route = \"/x/{anchor}/upload\";\n\
             export default { get, upload };\n",
    )
    .unwrap();
    run(&build_args(&t, Some("user"))).await.unwrap();
    let routes = std::fs::read_to_string(t.join("dist/user-0.1.0/routes.js")).unwrap();
    // 嵌套组 verb → handler 链 + 组 .route 的根级 pattern
    assert!(routes.contains("handler: \"upload.post\""), "{routes}");
    assert!(routes.contains("\"x/{anchor}/upload\""), "{routes}");
    // 顶层动词：链即方法，不写 handler（旧产物兼容）
    assert!(!routes.contains("handler: \"get\""), "{routes}");
    assert!(routes.contains("\"user/item\""), "{routes}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 判别红线：非 verb 键挂函数（此前的静默丢弃陷阱）→ 构建期可操作失败。
#[tokio::test]
async fn build_rejects_non_verb_function_export() {
    let t = std::env::temp_dir().join(format!("oj-build-nonverb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/user")).unwrap();
    std::fs::write(
        t.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/user/api.ts"),
        "function helper(){ json.ok({}); }\n\
             export default { get(){ json.ok({}); }, helper };\n",
    )
    .unwrap();
    let err = run(&build_args(&t, Some("user"))).await.unwrap_err();
    assert!(err.contains("helper"), "{err}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_converts_underscore_dirs_and_validates_patterns() {
    // v0.1.27：`_name_` 目录段 → routes.js pattern `{name}`，file 保留磁盘路径；
    // 非法 pattern 构建期 fail-fast；`.route` 值内 `_name_` 是字面段（不转换）。
    let t = std::env::temp_dir().join(format!("oj-build-us-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/user/_id_")).unwrap();
    std::fs::write(
        t.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/user/_id_/api.ts"),
        "function get(){ json.ok({id: http.param(\"id\")}); }\nexport default { get };\n",
    )
    .unwrap();
    run(&build_args(&t, None)).await.unwrap();
    let routes = std::fs::read_to_string(t.join("dist/user-0.1.0/routes.js")).unwrap();
    assert!(routes.contains("\"user/{id}\""), "{routes}");
    assert!(routes.contains("\"_id_/api.js\""), "{routes}");

    // `.route = "_id_"`：字面段，构建成功且不转换
    std::fs::write(
        t.join("src/user/_id_/api.ts"),
        "function get(){ json.ok({}); }\nget.route = \"_id_\";\nexport default { get };\n",
    )
    .unwrap();
    run(&build_args(&t, None)).await.unwrap();
    let routes = std::fs::read_to_string(t.join("dist/user-0.1.0/routes.js")).unwrap();
    assert!(routes.contains("\"user/{id}/_id_\""), "{routes}");

    // 非法 pattern（matchit 混合段）→ 构建期 fail-fast
    std::fs::write(
        t.join("src/user/_id_/api.ts"),
        "function get(){ json.ok({}); }\nget.route = \"{id}.json\";\nexport default { get };\n",
    )
    .unwrap();
    let err = run(&build_args(&t, None)).await.unwrap_err();
    assert!(err.contains("invalid route pattern"), "{err}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_module_emits_versioned_artifacts() {
    let t = std::env::temp_dir().join(format!("oj-build-art-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    run(&build_args(&t, Some("user"))).await.unwrap();

    let vd = t.join("dist/user-0.1.0");
    assert_eq!(only_file(&vd.join("item")), "api.js"); // 保留原目录结构原名

    let routes = std::fs::read_to_string(vd.join("routes.js")).unwrap();
    assert!(routes.contains("\"user/item/{id}\""), "{routes}"); // pattern 无 base 含模块段
    assert!(routes.contains("\"item/api.js\""), "{routes}"); // file 含目录段
    assert!(!routes.contains("/v1/api"), "{routes}");

    let item_js = std::fs::read_to_string(vd.join("item/api.js")).unwrap();
    assert!(!item_js.contains(".route"), "{item_js}"); // .route 已剥
    assert!(item_js.contains("\"../_shared/validate.js\""), "{item_js}"); // import 后缀已补
    assert!(!item_js.contains('\n'), "{item_js}"); // 默认 minify：单行

    assert!(vd.join("manifest.yaml").is_file()); // 原样复制
    assert!(vd.join("_shared/validate.js").is_file()); // 非 api 原路径
    assert!(!item_js.contains("sourceMappingURL"), "{item_js}"); // minify 剥内联 sourcemap

    let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
    assert!(!lock.contains_key("other")); // 单模块构建不动他人
    assert!(t.join("dist/user-0.1.0.tgz").is_file());
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD：别名端到端——同一份源码跑 `oj build`，产物内**只剩相对 specifier**，
/// 本模块别名落同版本目录、跨模块别名按锁钉到目标版本目录。
#[tokio::test]
async fn alias_build_materializes_to_versioned_relative_paths() {
    let t = std::env::temp_dir().join(format!("oj-build-alias-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("src/other/_shared")).unwrap();
    std::fs::write(t.join("src/other/_shared/util.ts"), "export const u = 1;\n").unwrap();
    // S008：别名跨模块引用必须声明 deps（这正是本用例要顺带钉住的门禁）
    std::fs::write(
        t.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\ndeps:\n  other: \"^0.9.0\"\n",
    )
    .unwrap();
    std::fs::write(
            t.join("src/user/item/api.ts"),
            "import { v } from \"#_shared/validate\";\nimport { u } from \"#/other/_shared/util\";\nfunction get(){ json.ok({v,u}); }\nget.route = \"{id}\";\nexport default { get };\n",
        )
        .unwrap();
    run(&build_args(&t, None)).await.unwrap();

    let item = std::fs::read_to_string(t.join("dist/user-0.1.0/item/api.js")).unwrap();
    assert!(item.contains("\"../_shared/validate.js\""), "{item}");
    assert!(
        item.contains("\"../../other-0.9.0/_shared/util.js\""),
        "{item}"
    );
    assert!(!item.contains('#'), "{item}");
    // 全产物扫描：任何落盘 .js 都不得残留别名
    let mut stack = vec![t.join("dist")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "js") {
                let js = std::fs::read_to_string(&p).unwrap();
                assert!(
                    assert_no_aliases(&js, &p.display().to_string()).is_ok(),
                    "{js}"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD：跨模块别名未声明 deps → S008 拦住构建（fail build，报错给下一步）。
#[tokio::test]
async fn build_rejects_cross_module_alias_without_deps() {
    let t = std::env::temp_dir().join(format!("oj-build-nodeps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("src/other/_shared")).unwrap();
    std::fs::write(t.join("src/other/_shared/util.ts"), "export const u = 1;\n").unwrap();
    std::fs::write(
            t.join("src/user/item/api.ts"),
            "import { u } from \"#/other/_shared/util\";\nfunction get(){ json.ok({u}); }\nexport default { get };\n",
        )
        .unwrap();
    let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
    assert!(
        e.contains("S008") && e.contains("other") && e.contains("deps"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD：tasks 池 import 越过池根（跨模块/池外目标）→ 产物里必然不存在，fail build。
#[tokio::test]
async fn build_rejects_tasks_import_escaping_pool() {
    let t = std::env::temp_dir().join(format!("oj-build-tesc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("src/tasks")).unwrap();
    std::fs::write(
            t.join("src/tasks/task_esc.ts"),
            "import { v } from \"../../../user/_shared/validate\";\nwhile (true) { await Promise.resolve(v); }\n",
        )
        .unwrap();
    let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
    assert!(e.contains("越过任务池根") && e.contains("_shared"), "{e}");
    // 池内互导（含子目录）仍然合法
    std::fs::create_dir_all(t.join("src/tasks/_shared")).unwrap();
    std::fs::write(
        t.join("src/tasks/_shared/tick.ts"),
        "export const tick = 1;\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/tasks/task_esc.ts"),
        "import { tick } from \"./_shared/tick\";\nwhile (true) { await Promise.resolve(tick); }\n",
    )
    .unwrap();
    run(&build_args(&t, None)).await.unwrap();
    let _ = std::fs::remove_dir_all(&t);
}

/// BDD：tasks 池是**非版本化**资产（不进锁 / 不打 tgz）→ 别名一律拒绝，fail build。
#[tokio::test]
async fn build_rejects_alias_in_tasks_pool() {
    let t = std::env::temp_dir().join(format!("oj-build-talias-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("src/tasks")).unwrap();
    std::fs::write(
        t.join("src/tasks/task_alias.ts"),
        "import { v } from \"#_shared/validate\";\nwhile (true) { await Promise.resolve(v); }\n",
    )
    .unwrap();
    let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
    assert!(
        e.contains("别名") && e.contains("非版本化") && e.contains("#_shared/validate"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_is_deterministic_and_wipes_on_change() {
    let t = std::env::temp_dir().join(format!("oj-build-wipe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    run(&build_args(&t, Some("user"))).await.unwrap();
    let snap = |p: &std::path::Path| std::fs::read(p).unwrap();
    let (js1, tgz1) = (
        snap(&t.join("dist/user-0.1.0/item/api.js")),
        snap(&t.join("dist/user-0.1.0.tgz")),
    );
    // 内容未变 → 重建字节一致（转译 + minify 确定性，落点 tgz）
    run(&build_args(&t, Some("user"))).await.unwrap();
    assert_eq!(snap(&t.join("dist/user-0.1.0/item/api.js")), js1);
    assert_eq!(
        snap(&t.join("dist/user-0.1.0.tgz")),
        tgz1,
        "同输入两次构建 tgz 必须字节一致"
    );
    // 内容变更 → 产物更新，目录内仍恰好 1 个 api.js（同版本清场）
    std::fs::write(
        t.join("src/user/item/api.ts"),
        "function get(){ json.ok({v:2}); }\nget.route = \"{id}\";\nexport default { get };\n",
    )
    .unwrap();
    run(&build_args(&t, Some("user"))).await.unwrap();
    let js2 = snap(&t.join("dist/user-0.1.0/item/api.js"));
    assert_ne!(js2, js1);
    assert_eq!(only_file(&t.join("dist/user-0.1.0/item")), "api.js");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn no_minify_keeps_readable_output() {
    let t = std::env::temp_dir().join(format!("oj-build-nomin-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    let mut a = build_args(&t, Some("user"));
    a.minify = false;
    run(&a).await.unwrap();
    let js = std::fs::read_to_string(t.join("dist/user-0.1.0/item/api.js")).unwrap();
    assert!(js.contains('\n'), "{js}"); // 未压缩：多行可读
    assert!(js.contains("function get"), "{js}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_rejects_api_import() {
    let t = std::env::temp_dir().join(format!("oj-build-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(t.join("src/user/_shared")).unwrap();
    std::fs::write(
        t.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/user/_shared/x.ts"),
        "import { g } from \"../item/api\";\n",
    )
    .unwrap();
    let e = run(&build_args(&t, Some("user")))
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("不可被"), "{e}"); // 守卫专属文案（"api" 子串近似恒真）
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_all_modules() {
    let t = std::env::temp_dir().join(format!("oj-build-all-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    run(&build_args(&t, None)).await.unwrap();
    assert!(t.join("dist/user-0.1.0/routes.js").is_file());
    assert!(t.join("dist/other-0.9.0/routes.js").is_file());
    let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert_eq!(lock.len(), 2, "{lock:?}");
    let _ = std::fs::remove_dir_all(&t);
}

/// 测试辅助：单模块 src（name/version 可注入）。
fn one_module(t: &std::path::Path, name: &str, version: &str) {
    std::fs::create_dir_all(t.join("src").join(name)).unwrap();
    std::fs::write(
        t.join("src").join(name).join("manifest.yaml"),
        format!("name: {name}\ndesc: d\nversion: {version}\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn build_all_rejects_illegal_module_dir() {
    // 全量路径的模块名同样是信任边界输入（I-1）：fail-fast 且锁不被污染
    let t = std::env::temp_dir().join(format!("oj-build-illegal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    one_module(&t, "bad module", "0.1.0");
    let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
    assert!(e.contains("illegal module"), "{e}");
    assert!(!t.join("dist/manifests.yaml").is_file());
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_rejects_illegal_version() {
    let t = std::env::temp_dir().join(format!("oj-build-ver-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    one_module(&t, "user", "0..1");
    let e = run(&build_args(&t, Some("user")))
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("illegal version") && e.contains("0..1"), "{e}");
    assert!(!t.join("dist/manifests.yaml").is_file());
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_all_rejects_vdir_collision() {
    // {m}-{v} 不单射：a v1-x 与 a-1 vx 同落 dist/a-1-x（后者构建清场前者）→ 计划期 Err
    let t = std::env::temp_dir().join(format!("oj-build-vdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    one_module(&t, "a", "1-x");
    one_module(&t, "a-1", "x");
    let e = run(&build_args(&t, None)).await.err().unwrap_or_default();
    assert!(e.contains("collision") && e.contains("a-1-x"), "{e}");
    assert!(!t.join("dist/a-1-x").exists());
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_rejects_collision_with_stale_lock_entry() {
    // R-1：撞名比对含锁内条目——锁 {a-1: x} 陈旧残留时，单建 a v1-x 同落 dist/a-1-x → Err
    let t = std::env::temp_dir().join(format!("oj-build-vdir2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    one_module(&t, "a", "1-x");
    std::fs::create_dir_all(t.join("dist")).unwrap();
    std::fs::write(t.join("dist/manifests.yaml"), "a-1: x\n").unwrap();
    let e = run(&build_args(&t, Some("a")))
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("collision") && e.contains("a-1-x"), "{e}");
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn single_build_preserves_other_lock_entries() {
    let t = std::env::temp_dir().join(format!("oj-build-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("dist")).unwrap();
    std::fs::write(t.join("dist/manifests.yaml"), "other: 0.9.0\n").unwrap(); // 预置他模块
    run(&build_args(&t, Some("user"))).await.unwrap();
    let lock = crate::manifest::load_lock(&t.join("dist/manifests.yaml")).unwrap();
    assert_eq!(lock.get("user").map(String::as_str), Some("0.1.0"));
    assert_eq!(lock.get("other").map(String::as_str), Some("0.9.0")); // spec §6：保留
    let _ = std::fs::remove_dir_all(&t);
}

/// P0 白名单：模块 seed.sql / schema.yaml / migrations/*.sql 原样进 dist；
/// fixtures/ 整目录排除（演示数据不随产物发布）。
#[tokio::test]
async fn build_copies_module_sql_and_excludes_fixtures() {
    let t = std::env::temp_dir().join(format!("oj-build-sql-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    one_module(&t, "user", "0.1.0");
    // S005：tables 声明与 schema.yaml 一致（build 内嵌检查）。
    std::fs::write(
        t.join("src/user/manifest.yaml"),
        "name: user\ndesc: d\nversion: 0.1.0\ntables: [account]\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/user/seed.sql"),
        "INSERT OR IGNORE INTO account VALUES (1, 'neo');\n",
    )
    .unwrap();
    std::fs::write(
        t.join("src/user/schema.yaml"),
        "tables:\n  account:\n    pk: id\n    columns:\n      id: { type: integer }\n",
    )
    .unwrap();
    std::fs::create_dir_all(t.join("src/user/migrations")).unwrap();
    std::fs::write(
        t.join("src/user/migrations/0001__init.sql"),
        "CREATE TABLE account (id, name);\n",
    )
    .unwrap();
    std::fs::create_dir_all(t.join("src/user/fixtures")).unwrap();
    std::fs::write(
        t.join("src/user/fixtures/demo.sql"),
        "INSERT INTO account VALUES (9);\n",
    )
    .unwrap();
    run(&build_args(&t, Some("user"))).await.unwrap();
    let vd = t.join("dist/user-0.1.0");
    // 原样 copy（字节一致）
    assert_eq!(
        std::fs::read(vd.join("seed.sql")).unwrap(),
        b"INSERT OR IGNORE INTO account VALUES (1, 'neo');\n"
    );
    assert!(vd.join("schema.yaml").is_file());
    assert!(vd.join("migrations/0001__init.sql").is_file());
    // fixtures 整目录不进产物
    assert!(!vd.join("fixtures").exists());
    let _ = std::fs::remove_dir_all(&t);
}

#[tokio::test]
async fn build_errs_on_corrupt_lock() {
    // 坏锁（非法 YAML）→ Err，不得 unwrap_or_default 当空表静默重置（I-2）
    let t = std::env::temp_dir().join(format!("oj-build-badlock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    src_fixture(&t);
    std::fs::create_dir_all(t.join("dist")).unwrap();
    std::fs::write(t.join("dist/manifests.yaml"), "user: [unclosed\n").unwrap();
    let e = run(&build_args(&t, Some("user")))
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("manifests.yaml"), "{e}");
    assert!(
        std::fs::read_to_string(t.join("dist/manifests.yaml"))
            .unwrap()
            .contains("unclosed")
    );
    let _ = std::fs::remove_dir_all(&t);
}
