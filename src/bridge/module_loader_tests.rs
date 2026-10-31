use super::*;

fn fx(files: &[(&str, &str)]) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let base = std::env::temp_dir().join(format!(
        "oj-ldr-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&base).unwrap();
    for (rel, content) in files {
        let p = base.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    base
}

#[test]
fn relative_resolution_completes_extensions() {
    let root = fx(&[
        ("user/_shared/validate.ts", "export function f() {}\n"),
        ("user/_shared/mod/index.ts", "export const x = 1;\n"),
        ("user/plain.js", "export const y = 2;\n"),
    ]);
    let dir = root.join("user/account");
    let ts = true;
    assert!(
        resolve_relative(&dir, "../_shared/validate", ts)
            .unwrap()
            .ends_with("validate.ts")
    );
    assert!(
        resolve_relative(&dir, "../_shared/mod", ts)
            .unwrap()
            .ends_with("mod/index.ts")
    );
    assert!(
        resolve_relative(&dir, "../plain", ts)
            .unwrap()
            .ends_with("plain.js")
    );
    let err = resolve_relative(&dir, "../nope", ts).unwrap_err();
    assert!(err.contains("tried"), "{err}");
}

/// 别名夹具：两个模块（m1 / user），m1 下挖 8 层深目录模拟真实 handler。
fn alias_fx(tag: &str) -> (PathBuf, PathBuf) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "oj-alias-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mf = |n: &str| format!("name: {n}\ndesc: d\nversion: 0.1.0\n");
    let files: Vec<(String, String)> = vec![
        ("src/m1/manifest.yaml".into(), mf("m1")),
        (
            "src/m1/_shared/validate.ts".into(),
            "export const v = 1;\n".into(),
        ),
        (
            "src/m1/_shared/mod/index.ts".into(),
            "export const x = 1;\n".into(),
        ),
        ("src/user/manifest.yaml".into(), mf("user")),
        (
            "src/user/_shared/util.ts".into(),
            "export const u = 1;\n".into(),
        ),
    ];
    for (rel, content) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    let deep = root.join("src/m1/a/b/c/d/e/f/g");
    std::fs::create_dir_all(&deep).unwrap();
    (root, deep)
}

#[test]
fn alias_resolves_module_root_and_src_root() {
    let (root, deep) = alias_fx("ok");
    // 本模块根锚点：与目录深度无关。
    assert!(
        resolve_alias("#_shared/validate", &deep, &root, true, None)
            .unwrap()
            .ends_with("src/m1/_shared/validate.ts")
    );
    // 显式后缀（用户原话的 `import 'xxx.ts'` 形态）。
    assert!(
        resolve_alias("#_shared/validate.ts", &deep, &root, true, None)
            .unwrap()
            .ends_with("validate.ts")
    );
    // 目录索引。
    assert!(
        resolve_alias("#_shared/mod", &deep, &root, true, None)
            .unwrap()
            .ends_with("mod/index.ts")
    );
    // src 根锚点（跨模块）。
    assert!(
        resolve_alias("#/user/_shared/util", &deep, &root, true, None)
            .unwrap()
            .ends_with("src/user/_shared/util.ts")
    );
    // 深目录与模块根等价（同模块）。
    assert_eq!(
        resolve_alias("#_shared/validate", &deep, &root, true, None).unwrap(),
        resolve_alias("#_shared/validate", &root.join("src/m1"), &root, true, None).unwrap()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// 嵌套布局夹具：`src/foo/api`（name=m1）、`src/bar/api`（name=m2）。
/// 返回 (src 根, m1 内的深层目录)。
fn alias_fx_nested(tag: &str) -> (PathBuf, PathBuf) {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "oj-alias-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let files: Vec<(String, String)> = vec![
        (
            "src/foo/api/manifest.yaml".into(),
            "name: m1\ndesc: d\nversion: 0.1.0\n".into(),
        ),
        (
            "src/bar/api/manifest.yaml".into(),
            "name: m2\ndesc: d\nversion: 0.1.0\n".into(),
        ),
        (
            "src/bar/api/_shared/util.ts".into(),
            "export const u = 1;\n".into(),
        ),
    ];
    for (rel, content) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    let deep = root.join("src/foo/api/a/b/c");
    std::fs::create_dir_all(&deep).unwrap();
    (root, deep)
}

/// BDD：模块根可不在首层（目录名 ≠ 模块名）时，`#/m/x` 仍按 `manifest.name` 解析——
/// 旧实现取 `module_root.parent()` 当 src 根，在嵌套布局下会算出 `src/apps` 而非
/// `src`，跨模块别名必然落空。
#[test]
fn given_nested_module_roots_when_alias_cross_module_then_resolves_by_manifest_name() {
    // Given: 模块根在第二层，目录名与模块名无关
    let (root, deep) = alias_fx_nested("nested");
    let idx = crate::bridge::resolve_roots(&root.join("src"), true, None).unwrap();

    // When: 从 m1 深层目录跨模块引用 m2（按 manifest.name，不是目录名）
    let got = resolve_alias("#/m2/_shared/util", &deep, &root, true, Some(&idx)).unwrap();

    // Then: 命中 m2 的模块根（目录名 foo/bar 不参与解析）
    assert!(got.ends_with("bar/api/_shared/util.ts"), "{got:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Given 同上一例，但**不给索引**（今日布局）：`src/m1` 首段即模块名，行为不变。
#[test]
fn given_flat_tree_without_index_when_alias_cross_module_then_today_semantics() {
    let (root, deep) = alias_fx("flat");
    let got = resolve_alias("#/user/_shared/util", &deep, &root, true, None).unwrap();
    assert!(got.ends_with("src/user/_shared/util.ts"), "{got:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn alias_errors_are_actionable() {
    let (root, deep) = alias_fx("err");
    // 未知模块名 → 列出实际存在的模块 + 下一步。
    let e = resolve_alias("#/nope/x", &deep, &root, true, None).unwrap_err();
    assert!(
        e.contains("nope") && e.contains("m1") && e.contains("下一步"),
        "{e}"
    );
    // 目标不存在 → 列出尝试过的候选（resolve_relative 口径）。
    let e = resolve_alias("#_shared/nope", &deep, &root, true, None).unwrap_err();
    assert!(e.contains("tried"), "{e}");
    // 别名路径不得含空段 / `.` / `..` / `\`（纵深防御）。
    for bad in ["#../x", "#/user/../x", "#//x", "#_shared//x", "#a\\b", "#."] {
        let e = resolve_alias(bad, &deep, &root, true, None).unwrap_err();
        assert!(e.contains("不得含"), "{bad}: {e}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn alias_requires_module_ancestor() {
    // 模块外（tests 目录 / tasks 池）无 manifest.yaml 祖先 → 明确报错并给下一步。
    let root = fx(&[("tests/x.test.ts", "export const x = 1;\n")]);
    let outside = root.join("tests");
    assert!(module_root_of(&outside, &root).is_none());
    let e = resolve_alias("#_shared/validate", &outside, &root, true, None).unwrap_err();
    assert!(e.contains("manifest.yaml") && e.contains("相对路径"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

/// `strip_verbatim` 是**纯字符串函数**，此前只被下面的 `#[cfg(windows)]` 用例
/// 覆盖（本机编不到也跑不了）。这里补一组平台无关断言：unix 上也编译运行，
/// 让该函数的语义不再只能靠 Windows CI 兜底。
///
/// UNC 专项（v0.1.85 销账）：`\\?\UNC\server\share` 曾盲目剥成
/// `UNC\server\share`（无根锚的坏路径，此前只在本用例的文档注释里登记为已知
/// 缺口）。`module_root_of` 双侧 canonicalize 后该形态走上主路径（UNC 部署 +
/// `#` 别名必踩），故本期修掉。
#[test]
fn strip_verbatim_is_prefix_only_and_platform_agnostic() {
    // 剥 verbatim 前缀（`\\?\`）。
    assert_eq!(
        strip_verbatim(Path::new(r"\\?\C:\a\b")),
        PathBuf::from(r"C:\a\b")
    );
    // UNC verbatim → 还原成标准 UNC（`\\server\share`），不是 `UNC\server\share`。
    assert_eq!(
        strip_verbatim(Path::new(r"\\?\UNC\server\share\proj\src")),
        PathBuf::from(r"\\server\share\proj\src")
    );
    // 已是标准 UNC → 原样（不递归剥）。
    assert_eq!(
        strip_verbatim(Path::new(r"\\server\share\a")),
        PathBuf::from(r"\\server\share\a")
    );
    // 无前缀 / unix 形态 → 原样返回。
    assert_eq!(
        strip_verbatim(Path::new(r"C:\a\b")),
        PathBuf::from(r"C:\a\b")
    );
    assert_eq!(strip_verbatim(Path::new("/tmp/a")), PathBuf::from("/tmp/a"));
    // 幂等：只剥一层，剥过之后再调用不变（不递归剥、不剥中段）。
    let once = strip_verbatim(Path::new(r"\\?\C:\a"));
    assert_eq!(once, PathBuf::from(r"C:\a"));
    assert_eq!(strip_verbatim(&once), once);
}

/// 回归（Windows）：`canonicalize` 给 referrer 目录加 `\\?\` 前缀，而
/// `ModuleSpecifier::to_file_path` 还原时剥掉——同一条长名路径仅差此前缀。
/// `module_root_of` 此前用词法 `starts_with`，前缀不一致即误判「未找到模块根」
/// （见 build_cmd 的 alias_build_materializes_to_versioned_relative_paths 在 CI 失败）。
/// 本用例仅 Windows 编译运行（macOS/Linux canonicalize 不带前缀，无此问题）。
#[test]
#[cfg(windows)]
fn module_root_of_tolerates_verbatim_prefix_mismatch() {
    let (root, _deep) = alias_fx("verbatim");
    let canon = root.canonicalize().unwrap(); // 项目根（长名 + `\\?\`）
    let proj_root_prefix = canon.clone(); // project_root 带前缀
    let proj_root_plain = strip_verbatim(&canon); // project_root 无前缀
    let with_prefix = canon.join("src/m1/a/b"); // referrer（canonical，带前缀）
    let no_prefix = strip_verbatim(&with_prefix); // referrer 无前缀（模拟 to_file_path）
    let want = strip_verbatim(&canon.join("src/m1")); // 模块根（归一后必然无前缀）

    // 方向一：referrer 无前缀 vs project_root 带前缀。
    assert_eq!(
        module_root_of(&no_prefix, &proj_root_prefix),
        Some(want.clone()),
        "no-prefix from_dir vs \\?\\-prefixed root 应命中模块根"
    );
    // 方向二：referrer 带前缀 vs project_root 无前缀。
    // 函数对两者均 `strip_verbatim` 再比，故返回同样是无前缀的模块根。
    assert_eq!(
        module_root_of(&with_prefix, &proj_root_plain),
        Some(want),
        "\\?\\-prefixed from_dir vs no-prefix root 应命中模块根"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn module_root_of_tolerates_symlinked_project_root() {
    // 生产形态：referrer 来自 file URL（deno_core 已 realpath → /private/var/…），
    // 而 project_root 是原样 CLI/config 路径（macOS 上含 /var → /private/var symlink，
    // Windows 上可能是 subst/ junctions）——词法前缀比较会把「明明在模块内」误判成
    // 未找到模块根（oj openapi --module-root 实测复现）。双侧 canonicalize 修复。
    let (root, _deep) = alias_fx("symlink-root");
    let canon = root.canonicalize().unwrap();
    // 经符号链接访问项目根（链接名 ≠ 真实路径的最后一段）。
    let link = std::env::temp_dir().join(format!(
        "oj-alias-symlinked-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    if !try_symlink(&canon, &link, true) {
        eprintln!("skip: symlink unsupported on this host");
        return;
    }
    let referrer_dir = canon.join("src/m1/a/b"); // deno_core 侧：realpath 形态
    let project_root = link.clone(); // CLI/config 侧：经 symlink 的形态
    // Windows 上 canonicalize 给 verbatim（`\\?\`）路径，而 module_root_of 按契约
    // 返回剥前缀的归一形态（见 module_root_of_tolerates_verbatim_prefix_mismatch），
    // 期望值须同口径，否则仅在 Windows CI 红。
    let want = strip_verbatim(&canon.join("src/m1"));

    assert_eq!(
        module_root_of(&referrer_dir, &project_root),
        Some(want),
        "symlinked project_root vs realpath referrer 应命中模块根"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&link);
}

#[test]
fn module_root_of_tolerates_virtual_dir_under_symlinked_root() {
    // 纯路径推演：referrer 目录**可以不存在**（`oj build` 的导入改写按目录串计算，
    // 不落盘）——canonicalize(from_dir) 失败时不能直接把词法路径与已 canonical 化的
    // root 混比（形态不一致 → 误判未找到）。按词法前缀换根，两侧保持同形态。
    let (root, _deep) = alias_fx("virtual");
    let canon = root.canonicalize().unwrap();
    let link = std::env::temp_dir().join(format!(
        "oj-alias-virtual-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    if !try_symlink(&canon, &link, true) {
        eprintln!("skip: symlink unsupported on this host");
        return;
    }
    // 经 symlink 的、且**不存在**的深层目录。
    let virtual_dir = link.join("src/m1/a/b/not/created/yet");
    // 同上：期望值与 module_root_of 的返回口径一致（剥 verbatim 前缀）。
    assert_eq!(
        module_root_of(&virtual_dir, &link),
        Some(strip_verbatim(&canon.join("src/m1"))),
        "虚拟目录 + symlink 根应仍命中模块根"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&link);
}

#[test]
fn resolve_inner_enables_alias_only_outside_node_modules() {
    let (root, deep) = alias_fx("nm");
    let loader = OjModuleLoader {
        module_index: None,
        inner: Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: true,
        }),
    };
    let referrer = |p: &Path| ModuleSpecifier::from_file_path(p).unwrap().to_string();
    // 模块内文件：`#` 走别名，且经 ensure_within + ?v= 版本化。
    let url = loader
        .resolve_inner("#_shared/validate", &referrer(&deep.join("api.ts")))
        .unwrap();
    assert!(
        url.as_str().contains("m1/_shared/validate.ts") && url.as_str().contains("?v="),
        "{url}"
    );
    // node_modules 内的文件不启用别名（第三方包自己的 package.json#imports 语义）
    // → 回落裸 specifier 解析，报错文案指向 node_modules 而非别名。
    let nm = root.join("node_modules/pkg");
    std::fs::create_dir_all(&nm).unwrap();
    let e = loader
        .resolve_inner("#foo", &referrer(&nm.join("index.js")))
        .unwrap_err();
    assert!(e.contains("node_modules"), "{e}");
    assert!(!e.contains("逐级上溯未找到模块根"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

/// release（ts=false）下别名一律拒绝：`oj build` 已实化，走到这里说明产物不干净或
/// 用法越界（tests/ 与任务池）。半可解析（同模块命中、跨模块悬空）比直接报错更坏。
#[test]
fn alias_rejected_in_release_mode() {
    let (root, deep) = alias_fx("rel");
    let e = resolve_alias("#_shared/validate", &deep, &root, false, None).unwrap_err();
    assert!(e.contains("release") && e.contains("oj build"), "{e}");
    assert!(e.contains("相对路径"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn versioned_specifier_roundtrip() {
    let root = fx(&[("a.ts", "export default 1;\n")]);
    let p = root.join("a.ts");
    let url = versioned_specifier(&p).unwrap();
    assert!(url.as_str().starts_with("file://"), "{url}");
    assert!(url.as_str().contains("?v="), "{url}");
}

#[test]
fn bare_resolves_node_modules() {
    let root = fx(&[
        ("node_modules/escape-goat/index.js", "export const x = 1;\n"),
        (
            "node_modules/escape-goat/package.json",
            r#"{"name":"escape-goat","version":"4.0.0","type":"module"}"#,
        ),
        (
            "node_modules/cjspkg/main.js",
            "module.exports = { n: 1 };\n",
        ),
        (
            "node_modules/cjspkg/package.json",
            r#"{"name":"cjspkg","version":"1.0.0","main":"main.js"}"#,
        ),
        (
            "node_modules/withmod/pkg/lib/util.js",
            "export const u = 1;\n",
        ),
        (
            "node_modules/withmod/pkg/package.json",
            r#"{"name":"withmod"}"#,
        ),
    ]);
    let from = root.join("src/user");
    // ESM 包：type:module → index.js。
    assert!(
        resolve_bare("escape-goat", &from, &root)
            .unwrap()
            .ends_with("escape-goat/index.js")
    );
    // CJS 包：main 字段。
    assert!(
        resolve_bare("cjspkg", &from, &root)
            .unwrap()
            .ends_with("cjspkg/main.js")
    );
    // subpath 直映射。
    assert!(
        resolve_bare("withmod/pkg/lib/util.js", &from, &root)
            .unwrap()
            .ends_with("lib/util.js")
    );
    // 不存在 → 错误含提示。
    let e = resolve_bare("nope-pkg", &from, &root).unwrap_err();
    assert!(e.contains("node_modules"), "{e}");
    // 回溯：src/user/feat 深处也能找到根 node_modules。
    assert!(resolve_bare("escape-goat", &root.join("src/user/feat"), &root).is_ok());
}

#[test]
fn resolve_rejects_paths_escaping_project_root() {
    // base 下 proj 是项目根；escape.js 在根外（不依赖 /etc 等系统文件）。
    let base = fx(&[
        ("escape.js", "export const e = 1;\n"),
        ("proj/src/user/mod.js", "export const m = 1;\n"),
    ]);
    let root = base.join("proj");
    let loader = OjModuleLoader {
        module_index: None,
        inner: Arc::new(LoaderShared {
            project_root: root.clone(),
            ts: true,
        }),
    };
    let referrer = ModuleSpecifier::from_file_path(root.join("src/user/mod.js"))
        .unwrap()
        .to_string();
    // 根内相对导入不受影响。
    assert!(loader.resolve_inner("./mod.js", &referrer).is_ok());
    // lexical `..` 越过项目根 → 钳制报错（而非解析成功）。
    let e = loader
        .resolve_inner("../../../escape.js", &referrer)
        .unwrap_err();
    assert!(e.contains("escapes project root"), "{e}");
}

/// `ensure_within`（mail 附件 `{path}` 钳制复用）：根内路径放行且**返回 canonical 句柄**
/// （调用方按它读盘，规避「校验用的路径 ≠ 读盘用的路径」的 TOCTOU 面）；
/// 词法 `..` 越界 / 根外绝对路径一律拒绝。
#[test]
fn ensure_within_returns_canonical_and_rejects_escape() {
    let (root, _deep) = alias_fx("within");
    let inside = root.join("src/m1/_shared/validate.ts");
    let got = ensure_within(&inside, &root).unwrap();
    assert!(got.is_absolute(), "{}", got.display());
    assert_eq!(got, inside.canonicalize().unwrap());
    // 根外（上跳 + 兄弟文件）→ 拒绝。
    let outside = root
        .parent()
        .unwrap()
        .join(format!("oj-within-escape-{}.txt", std::process::id()));
    std::fs::write(&outside, b"x").unwrap();
    let e = ensure_within(&outside, &root).unwrap_err();
    assert!(e.contains("escapes project root"), "{e}");
    // 不存在的路径同样拒绝（canonicalize 失败即 Err，不会放行）。
    assert!(ensure_within(&root.join("nope.txt"), &root).is_err());
    assert!(ensure_within(std::path::Path::new("/etc/hosts"), &root).is_err());
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&root);
}

/// design §9/§11「路径穿越：`ensure_within` **双侧 canonicalize**，符号链接在覆盖内」——
/// 词法 `..` 之外的第二条逃逸面：根**内**的符号链接指向根**外**文件（路径字符串完全在
/// 根内，只有 canonicalize 能识破）。附件 `{path}` 与模块加载共用本函数。
/// Windows 无符号链接特权（未开 Developer Mode）时跳过。
#[test]
fn ensure_within_rejects_symlink_escape() {
    let base = fx(&[("outside.txt", "secret\n"), ("proj/inside.txt", "ok\n")]);
    let root = base.join("proj");
    // 根内文件：放行（对照组，防「一律拒绝」的变异蒙混）。
    assert!(ensure_within(&root.join("inside.txt"), &root).is_ok());

    // 根内符号链接 → 根外文件。
    let file_link = root.join("link.txt");
    if !try_symlink(&base.join("outside.txt"), &file_link, false) {
        eprintln!("skip ensure_within_rejects_symlink_escape: symlink unavailable");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }
    let e = ensure_within(&file_link, &root).unwrap_err();
    assert!(e.contains("escapes project root"), "{e}");

    // 根内符号链接 → 根外目录，再穿透一层。
    let dir_link = root.join("dirlink");
    assert!(try_symlink(&base, &dir_link, true), "symlink_dir failed");
    assert!(
        ensure_within(&dir_link.join("outside.txt"), &root).is_err(),
        "经目录符号链接穿透到根外也必须拒"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn cjs_detection_and_wrap() {
    assert!(looks_cjs("module.exports = { a: 1 };\n"));
    assert!(!looks_cjs("export default 1;\n"));
    assert!(!looks_cjs("import x from 'y';\nmodule.exports = x;\n"));
    let wrapped = wrap_cjs("module.exports = { a: 1 };\n", "/nm/p/main.js");
    assert!(wrapped.contains("__oj_cjs_module"), "{wrapped}");
    assert!(
        wrapped.contains("export default __oj_cjs_module.exports"),
        "{wrapped}"
    );
    // require 绑定模块自身路径（嵌套 require 的 referrer）。
    assert!(
        wrapped.contains(r#"__ojRequire(n, "/nm/p/main.js")"#),
        "{wrapped}"
    );
}

// ---------- exports / conditions（oj-1） ----------

#[test]
fn bare_resolves_exports_variants() {
    let root = fx(&[
        // 字符串形态 + 无 main 的现代包。
        (
            "node_modules/strpkg/package.json",
            r#"{"name":"strpkg","exports":"./index.js"}"#,
        ),
        ("node_modules/strpkg/index.js", "export const x = 1;\n"),
        // 条件对象：import/require 指向不同文件。
        (
            "node_modules/condpkg/package.json",
            r#"{
                "name":"condpkg",
                "exports":{".":{"import":"./esm.js","require":"./cjs.js","default":"./fallback.js"}}
            }"#,
        ),
        ("node_modules/condpkg/esm.js", "export const m = 'esm';\n"),
        ("node_modules/condpkg/cjs.js", "module.exports = 'cjs';\n"),
        (
            "node_modules/condpkg/fallback.js",
            "export const m = 'fb';\n",
        ),
        // 子路径键 + "./x/*" 模式。
        (
            "node_modules/subpkg/package.json",
            r#"{
                "name":"subpkg",
                "exports":{
                    "./feat": "./lib/feat.js",
                    "./deep/*": "./dist/*",
                    "./arr": ["./nope.js", "./arr.js"]
                }
            }"#,
        ),
        ("node_modules/subpkg/lib/feat.js", "export const f = 1;\n"),
        ("node_modules/subpkg/dist/a/b.js", "export const d = 1;\n"),
        ("node_modules/subpkg/arr.js", "export const a = 1;\n"),
    ]);
    let from = root.join("src/user");
    // 字符串形态（exports 无 "." 键，整体即 "." 目标）。
    assert!(
        resolve_bare("strpkg", &from, &root)
            .unwrap()
            .ends_with("strpkg/index.js")
    );
    // 条件：ESM 走 import，CJS require 走 require。
    assert!(
        resolve_bare("condpkg", &from, &root)
            .unwrap()
            .ends_with("condpkg/esm.js")
    );
    assert!(
        resolve_bare_mode("condpkg", &from, &root, true)
            .unwrap()
            .ends_with("condpkg/cjs.js")
    );
    // 子路径键与模式。
    assert!(
        resolve_bare("subpkg/feat", &from, &root)
            .unwrap()
            .ends_with("lib/feat.js")
    );
    assert!(
        resolve_bare("subpkg/deep/a/b.js", &from, &root)
            .unwrap()
            .ends_with("dist/a/b.js")
    );
    // 数组 fallback：首个不可解析项跳过，命中第二项。
    assert!(
        resolve_bare("subpkg/arr", &from, &root)
            .unwrap()
            .ends_with("subpkg/arr.js")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bare_exports_unexported_subpath_errors_with_keys() {
    let root = fx(&[
        (
            "node_modules/expkg/package.json",
            r#"{
                "name":"expkg",
                "exports":{".":"./index.js","./ok":"./ok.js"}
            }"#,
        ),
        ("node_modules/expkg/index.js", "export const x = 1;\n"),
        ("node_modules/expkg/ok.js", "export const o = 1;\n"),
        // 显式 exports 之外的文件存在也不可见（Node 封闭语义）。
        ("node_modules/expkg/internal.js", "export const i = 1;\n"),
    ]);
    let from = root.join("src/user");
    assert!(resolve_bare("expkg/ok", &from, &root).is_ok());
    for spec in ["expkg/internal", "expkg/nope"] {
        let e = resolve_bare(spec, &from, &root).unwrap_err();
        assert!(e.contains("not defined by \"exports\""), "{spec}: {e}");
        assert!(e.contains("./ok"), "{spec}: {e}");
    }
    // 目标文件缺失：命中映射但文件不在 → 报错（而非回落 legacy）。
    let root2 = fx(&[(
        "node_modules/misspkg/package.json",
        r#"{"exports":{".":"./gone.js"}}"#,
    )]);
    let from2 = root2.join("src/user");
    let e = resolve_bare("misspkg", &from2, &root2).unwrap_err();
    assert!(e.contains("not found"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&root2);
}

/// pnpm 布局：node_modules/<pkg> 是指向 .pnpm/<pkg>@<v>/node_modules/<pkg> 的
/// 符号链接；包的真实位置旁有同伴依赖（peer/dep 链接在 .pnpm/<pkg>@<v>/
/// node_modules/ 下）。解析沿符号链接工作、且不得 realpath 中断相对回溯。
/// Windows 无符号链接特权（未开 Developer Mode）时跳过。
#[test]
fn bare_resolves_pnpm_symlink_layout() {
    let root = fx(&[
        (
            "node_modules/.pnpm/pkg@1.0.0/node_modules/pkg/package.json",
            r#"{"name":"pkg","version":"1.0.0","exports":{".":"./index.js"}}"#,
        ),
        (
            "node_modules/.pnpm/pkg@1.0.0/node_modules/pkg/index.js",
            "export const x = 1;\n",
        ),
        (
            "node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA/package.json",
            r#"{"name":"pkgA","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA/index.js",
            "module.exports = 1;\n",
        ),
        (
            "node_modules/.pnpm/pkgA@1.0.0/node_modules/depB/package.json",
            r#"{"name":"depB","version":"2.0.0","main":"index.js"}"#,
        ),
        (
            "node_modules/.pnpm/pkgA@1.0.0/node_modules/depB/index.js",
            "module.exports = 2;\n",
        ),
    ]);
    let nm = root.join("node_modules");
    // pnpm 风格的平铺链接视图。
    if !try_symlink(
        &root.join("node_modules/.pnpm/pkg@1.0.0/node_modules/pkg"),
        &nm.join("pkg"),
        true,
    ) {
        eprintln!("skip bare_resolves_pnpm_symlink_layout: symlink unavailable");
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    assert!(
        try_symlink(
            &root.join("node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA"),
            &nm.join("pkgA"),
            true
        ),
        "symlink_dir failed"
    );
    let from = root.join("src/user");
    // 平铺视图：经符号链接命中 exports。
    assert!(
        resolve_bare("pkg", &from, &root)
            .unwrap()
            .ends_with("pkg/index.js")
    );
    // 真实路径深处（versioned_specifier canonicalize 后的 referrer 形态）：
    // 向上回溯命中 .pnpm/<pkgA>@<v>/node_modules/ 下的同伴依赖。
    let deep = root.join("node_modules/.pnpm/pkgA@1.0.0/node_modules/pkgA/sub");
    assert!(
        resolve_bare_mode("depB", &deep, &root, true)
            .unwrap()
            .ends_with("depB/index.js")
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---------- CJS 相对 require / JSON / 循环（oj-2） ----------

#[test]
fn cjs_relative_probes_js_json_and_index() {
    let root = fx(&[
        ("src/user/sib.js", "module.exports = 1;\n"),
        ("src/user/data.json", r#"{"k": 1}"#),
        ("src/user/mod/index.js", "module.exports = 2;\n"),
    ]);
    let dir = root.join("src/user");
    assert!(
        resolve_cjs_relative(&dir, "./sib")
            .unwrap()
            .ends_with("sib.js")
    );
    assert!(
        resolve_cjs_relative(&dir, "./data")
            .unwrap()
            .ends_with("data.json")
    );
    assert!(
        resolve_cjs_relative(&dir, "./mod")
            .unwrap()
            .ends_with("mod/index.js")
    );
    let e = resolve_cjs_relative(&dir, "./nope").unwrap_err();
    assert!(e.contains("tried"), "{e}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cjs_registry_wrap_is_idempotent_and_builtin_named() {
    // 注册表守卫：重复 eval 只执行一次（环上第二次进入跳过 src 执行）。
    let w = cjs_registry_wrap("module.exports.a = 1;\n", "/nm/p/a.js");
    assert!(w.contains("globalThis.__ojCjsRun"), "{w}");
    assert!(w.contains("if (!__r[\"/nm/p/a.js\"])"), "{w}");
    // 嵌套 require 的 referrer = 本模块绝对路径。
    assert!(
        w.contains(r#"globalThis.__ojRequire(n, "/nm/p/a.js")"#),
        "{w}"
    );
    // 最终 exports 恒绑注册表（外层 m 拿到的是注册表 exports 引用）。
    assert!(
        w.contains("module.exports = __r[\"/nm/p/a.js\"].exports"),
        "{w}"
    );
    // node: 内建识别（首段匹配，含 node: 前缀形态）。
    assert!(is_node_builtin("path"));
    assert!(is_node_builtin("node:fs"));
    assert!(is_node_builtin("fs/promises"));
    assert!(!is_node_builtin("fsx"));
}

#[test]
fn cjs_json_module_code_validates_and_wraps() {
    let ok = json_module_code(r#"{"k": 1}"#).unwrap();
    assert_eq!(ok, r#"module.exports = {"k": 1};"#);
    let e = json_module_code("{not json").unwrap_err();
    assert!(e.contains("invalid JSON module"), "{e}");
}
