//! 本地文件系统轴（v0.1.53）：经 deno_fs 扩展为 JS 提供 `fs` 全局（一次性读写 API，
//! 不暴露 fd 句柄——op 内自开自关，无跨请求泄漏面）。
//!
//! 安全模型（jail）：read/write 两轴收窄到 config `fs.root` 目录（双侧 canonicalize，
//! symlink/`..` 逃逸由 deno_permissions 路径校验封死）；`readonly: true` 时 write 轴
//! 整体 deny；未配置 `fs:` 段 = 双轴全 deny（调用抛 NotCapable）。
//! net/env/sys 等轴不受影响——fetch/WebSocket 出站与 fs 共用同一 PermissionsContainer，
//! 只覆写 read/write（见 [`permissions_for`]）。

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use deno_core::OpState;
use deno_core::op2;
use deno_permissions::Permissions;
use deno_permissions::PermissionsContainer;
use deno_permissions::PermissionsOptions;
use deno_permissions::RuntimePermissionDescriptorParser;

/// fs: 段的运行时授权（装配期构建、冻结进 StableState，跨请求只读）。
pub struct FsGrant {
    /// 已 canonicalize 的 jail 根目录。
    pub root: PathBuf,
    /// 只读模式：write 轴整体 deny（read 轴仍放行 root 内）。
    pub readonly: bool,
}

impl FsGrant {
    /// 装配期构建：root 不存在 / 非目录 → Err（serve 启动 fail-fast，不静默降级）。
    pub fn new(root: &Path, readonly: bool) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("fs.root canonicalize failed ({}): {e}", root.display()))?;
        if !root.is_dir() {
            return Err(format!("fs.root is not a directory: {}", root.display()));
        }
        Ok(Self { root, readonly })
    }
}

/// 由可选授权构建 deno 权限集。net/env/sys/run/ffi/import 恒放行（既有出站行为），
/// 仅 read/write 随授权收窄：None = 双 deny；Some(readonly) = read 限定 root；
/// Some(rw) = 双轴限定 root。每个 JsRuntime 的 bridge_ext state 闭包各调一次。
pub fn permissions_for(grant: Option<&FsGrant>) -> Permissions {
    let roots = grant.map(|g| vec![g.root.to_string_lossy().into_owned()]);
    let readonly = grant.is_some_and(|g| g.readonly);
    permissions_for_roots(roots, readonly)
}

/// read/write 授权根集合 → deno 权限集（构建单点：非特权 [`permissions_for`] 与
/// 特权 [`container_for_module`] 共用，readonly 语义一致——readonly 时 write 轴
/// 不给任何 allow = 整体 deny，对 trusted_paths 同样生效）。
fn permissions_for_roots(read_roots: Option<Vec<String>>, readonly: bool) -> Permissions {
    let parser = RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys);
    let opts = PermissionsOptions {
        allow_env: None,
        deny_env: None,
        ignore_env: None,
        allow_net: None,
        deny_net: None,
        allow_ffi: None,
        deny_ffi: None,
        allow_read: read_roots.clone(),
        deny_read: None,
        ignore_read: None,
        allow_run: None,
        deny_run: None,
        allow_sys: None,
        deny_sys: None,
        // readonly 时 write 轴不给任何 allow → 整体 deny。
        allow_write: read_roots.filter(|_| !readonly),
        deny_write: None,
        allow_import: None,
        deny_import: None,
        prompt: false,
    };
    let limited =
        Permissions::from_options(&parser, &opts).expect("fs grant permission options are valid");
    let mut perms = Permissions::allow_all();
    perms.read = limited.read;
    perms.write = limited.write;
    perms
}

/// 每个 JsRuntime 注入的容器：fetch/WS 依赖的 net 轴放行 + fs 的 jail 收窄。
pub fn container_for(grant: Option<&FsGrant>) -> PermissionsContainer {
    let parser = RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys);
    PermissionsContainer::new(Arc::new(parser), permissions_for(grant))
}

/// 按模块特权级构建容器（ojm 批次，设计稿 §1.6）：特权模块
/// （`module ∈ platform.modules`）的 read/write 授权面 = fs.root ∪ trusted_paths；
/// 其余一律走既有 [`container_for`]（非特权行为零变化）。
///
/// 调用点：`Bridge::checkout_reset` 每请求换装（deno fs op 的权限检查是第二道
/// 防线， trusted 路径可及性缺这步会被 deno_permissions 拒掉）。`fs:` 未配置时
/// 恒非特权（fail-closed：trusted_paths 不在没有 jail 的前提下放行任何路径）。
pub fn container_for_module(
    grant: Option<&FsGrant>,
    platform: Option<&crate::bridge::platform::PlatformGrant>,
    module: Option<&str>,
) -> PermissionsContainer {
    let privileged =
        grant.is_some() && module.is_some_and(|m| platform.is_some_and(|p| p.modules.contains(m)));
    if !privileged {
        return container_for(grant);
    }
    let g = grant.expect("privileged implies fs grant (checked above)");
    let p = platform.expect("privileged implies platform grant (checked above)");
    // PlatformGrant.trusted 首位即 fs.root（grant 构建期并入去重）；此处仍防御性
    // 去重，防手构 grant 或重复根把同一 allowlist 条目灌两遍。
    let mut roots = vec![g.root.to_string_lossy().into_owned()];
    for t in &p.trusted {
        let s = t.to_string_lossy().into_owned();
        if !roots.contains(&s) {
            roots.push(s);
        }
    }
    let parser = RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys);
    PermissionsContainer::new(
        Arc::new(parser),
        permissions_for_roots(Some(roots), g.readonly),
    )
}

/// 门面层的 jail 裁决（v1 安全关键）：相对路径先按 jail 根 lexical 折叠成绝对
/// 路径（`.` 丢弃、`..` 对 root 逐级上托——Windows verbatim 路径不归一 `..`，
/// 必须先折平再 canonicalize；此前的 JS 侧字符串拼接在 verbatim 根上产生
/// `/` 混合分隔符，canonicalize 必败、叶级 symlink 从未被解析，构成 Windows
/// 逃逸面），随后 best-effort canonicalize——整体不存在（写新文件）时逐级
/// 上溯到最近存在的祖先再拼回后缀——结果落在 root 外，或 fs: 未配置，一律
/// `NotCapable`；落在 root 内则返回 canonical 绝对路径供 deno op 使用，op 内
/// 的权限检查成为第二道防线。
/// ponytail: 检查与使用之间存在 TOCTOU 窗口（symbolic link 交换），对
/// 「防误触/防越界默认值」的威胁模型可接受；要堵则需 openat2 族，代价不值。
#[op2]
#[string]
pub fn op_fs_resolve(
    state: &mut OpState,
    #[string] path: String,
) -> Result<String, deno_error::JsErrorBox> {
    let (grant, platform) = {
        let stable = state.borrow::<Arc<crate::bridge::StableState>>();
        (stable.fs.clone(), stable.platform.clone())
    };
    let grant = grant.ok_or_else(|| {
        deno_error::JsErrorBox::new(
            "NotCapable",
            "fs not configured (config fs: section missing)",
        )
    })?;
    // 特权双查（ojm 批次，设计稿 §1.6）：ReqState.module ∈ platform.modules →
    // 判定集合 = fs.root ∪ trusted_paths（resolve_allowed 同一集合语义）；否则
    // 行为完全不变（仅 fs.root）。
    let module = state.borrow::<crate::bridge::ReqState>().module.clone();
    let trusted: &[PathBuf] = match (&platform, &module) {
        (Some(p), Some(m)) if p.modules.contains(m) => &p.trusted,
        _ => &[],
    };
    let joined = join_jailed(&grant.root, Path::new(&path));
    let resolved = resolve_existing(&joined)?;
    if !resolved.starts_with(&grant.root) && !trusted.iter().any(|t| resolved.starts_with(t)) {
        return Err(deno_error::JsErrorBox::new(
            "NotCapable",
            format!("path escapes fs.root: {path}"),
        ));
    }
    Ok(resolved.to_string_lossy().into_owned())
}

/// 相对 → jail 内绝对（lexical 折叠，不触盘）；绝对路径原样返回。折叠对
/// `..` 上托越过 root 时得到的短路径交给 canonicalize 后的 starts_with 拒绝。
/// `pub(crate)`：platform.rs 的 [`crate::bridge::platform::resolve_allowed`]
/// 复用同一折叠实现（路径校验单点，DRY——两处语义漂移即逃逸面）。
pub(crate) fn join_jailed(root: &Path, p: &Path) -> PathBuf {
    use std::path::Component;
    if p.is_absolute() {
        return p.to_path_buf();
    }
    let mut out = root.to_path_buf();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            // Prefix/RootDir：绝对性已被上方 is_absolute 分流，残余按忽略处理
            // （如 Windows `\x` 落回 root 下，安全方向）。
            _ => {}
        }
    }
    out
}

/// best-effort canonicalize：逐级上溯到最近存在的祖先，canonicalize 后拼回后缀。
/// `pub(crate)`：同 [`join_jailed`]，platform.rs 路径校验单点复用。
pub(crate) fn resolve_existing(p: &Path) -> Result<PathBuf, deno_error::JsErrorBox> {
    let mut ancestor = p;
    let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(base) = ancestor.canonicalize() {
            let mut out = base;
            for comp in suffix.iter().rev() {
                out.push(comp);
            }
            return Ok(out);
        }
        match ancestor.file_name() {
            Some(name) => suffix.push(name),
            None => {
                return Err(deno_error::JsErrorBox::new(
                    "NotCapable",
                    format!("path does not resolve: {}", p.display()),
                ));
            }
        }
        ancestor = ancestor
            .parent()
            .expect("parent of a path without file_name is unreachable");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Bridge;
    use crate::bridge::Extras;
    use crate::bridge::InMemoryKV;
    use crate::bridge::SchemaRegistry;

    use std::collections::HashMap;

    /// 独立临时 jail 目录（pid + 测试名防撞）。
    fn jail(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-fstest-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 带 fs 授权（jail = 给定目录）的 Bridge。
    fn bridge_with_fs(root: &Path, readonly: bool) -> Bridge {
        let grant = FsGrant::new(root, readonly).unwrap();
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                fs: Some(Arc::new(grant)),
                ..Default::default()
            },
        )
    }

    #[test]
    fn grant_new_rejects_missing_and_non_dir() {
        let missing =
            std::env::temp_dir().join(format!("oj-fstest-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(FsGrant::new(&missing, false).is_err());
        let f = jail("grantfile").join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        assert!(FsGrant::new(&f, false).is_err());
    }

    /// 未配置 fs: 段（Extras 缺省）：fs.* 抛 NotCapable（fail-closed 门禁默认关）。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_not_configured_denies_access() {
        let b = Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras::default(),
        );
        let e = b
            .run_with(
                r#"(async () => { await fs.readTextFile("a.txt"); })();"#,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("NotCapable"), "{e}");
    }

    /// 授权后 9 个 API 往返：mkdir/writeFile/readTextFile/stat/readdir/rename/remove。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_roundtrip_covers_surface() {
        let root = jail("roundtrip");
        let b = bridge_with_fs(&root, false);
        let cap = b
            .run_with(
                r#"
                (async () => {
                    await fs.mkdir("sub");
                    await fs.writeFile("sub/a.txt", new TextEncoder().encode("hello"));
                    const text = await fs.readTextFile("sub/a.txt");
                    const st = await fs.stat("sub/a.txt");
                    const names = [];
                    for await (const e of fs.readDir("sub")) names.push(e.name);
                    await fs.rename("sub/a.txt", "sub/b.txt");
                    const moved = await fs.readTextFile("sub/b.txt");
                    await fs.remove("sub/b.txt");
                    json.ok({ text, size: st.size, names, moved });
                })();
                "#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["text"], "hello");
        assert_eq!(v["data"]["moved"], "hello");
        assert_eq!(v["data"]["size"], 5);
        assert_eq!(v["data"]["names"], serde_json::json!(["a.txt"]));
    }

    /// `..` 相对逃逸与 root 外绝对路径一律 NotCapable。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_escape_root_denied() {
        let root = jail("escape");
        let outside = jail("escape-outside");
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let b = bridge_with_fs(&root, false);
        let e = b
            .run_with(
                r#"(async () => { await fs.readTextFile("../escape-outside/secret.txt"); })();"#,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("NotCapable"), "{e}");
        // 正向斜杠嵌入 JS：反斜杠会被当转义吃掉（\r→CR、\U→U），路径被毁后
        // 落回 jail 内，断言拿到的就是 deno 的 invalid_argument 而非 NotCapable。
        let abs_path = outside
            .join("secret.txt")
            .display()
            .to_string()
            .replace('\\', "/");
        let abs = format!(r#"(async () => {{ await fs.readTextFile("{abs_path}"); }})();"#);
        let e = b.run_with(&abs, Default::default()).await.unwrap_err();
        assert!(e.to_string().contains("NotCapable"), "{e}");
    }

    /// readonly：read 放行、write 三件套（writeFile/mkdir/remove）全 deny。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_readonly_denies_writes() {
        let root = jail("readonly");
        std::fs::write(root.join("ro.txt"), b"ro").unwrap();
        let b = bridge_with_fs(&root, true);
        let cap = b
            .run_with(
                r#"(async () => { json.ok({ t: await fs.readTextFile("ro.txt") }); })();"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"]["t"], "ro");
        for code in [
            r#"(async () => { await fs.writeFile("x.txt", new Uint8Array([1])); })();"#,
            r#"(async () => { await fs.mkdir("d"); })();"#,
            r#"(async () => { await fs.remove("ro.txt"); })();"#,
        ] {
            let e = b.run_with(code, Default::default()).await.unwrap_err();
            assert!(e.to_string().contains("NotCapable"), "{code} -> {e}");
        }
    }

    /// 特权模块（ReqState.module ∈ platform.modules）fs 轴扩展（BDD S5，ojm 批次）：
    /// trusted_paths 内文件读写可及（门面裁决 + deno 第二道防线同口径放行）；
    /// 非特权模块同路径仍 NotCapable（既有 jail 行为零变化）。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_trusted_paths_reachable_for_privileged_module_only() {
        use crate::bridge::ModuleCtx;
        use crate::bridge::module_loader::LoaderShared;
        use crate::bridge::platform::PlatformGrant;
        // macOS 下 temp_dir 经 symlink：统一 canonical，防前缀比对被形态打穿。
        let root = jail("platfs").canonicalize().unwrap();
        let trusted_dir = jail("platfs-trusted").canonicalize().unwrap();
        std::fs::write(trusted_dir.join("config.yaml"), b"cfg").unwrap();
        let grant = FsGrant::new(&root, false).unwrap();
        let platform = Arc::new(
            PlatformGrant::new(
                vec!["plat".into()],
                std::slice::from_ref(&trusted_dir),
                Some(&grant.root),
            )
            .unwrap(),
        );
        // 模块树：plat 模块（api.js），归属键 = 模块目录绝对路径（§5.3 装配形态）。
        let base = jail("platfs-mod").canonicalize().unwrap();
        std::fs::create_dir_all(base.join("plat")).unwrap();
        let cfg_path = trusted_dir.join("config.yaml");
        let out_path = trusted_dir.join("out.txt");
        std::fs::write(
            base.join("plat/api.js"),
            format!(
                "export async function readTrusted() {{\n\
                 \x20 const t = await fs.readTextFile(\"{cfg}\");\n\
                 \x20 json.ok({{ t }});\n\
                 }}\n\
                 export async function writeTrusted() {{\n\
                 \x20 await fs.writeTextFile(\"{out}\", \"hi\");\n\
                 \x20 json.ok({{ done: true }});\n\
                 }}\n\
                 export default {{ readTrusted, writeTrusted }};\n",
                cfg = cfg_path.display(),
                out = out_path.display(),
            ),
        )
        .unwrap();
        let mut modules = HashMap::new();
        modules.insert(
            base.join("plat").to_string_lossy().into_owned(),
            ModuleCtx {
                name: "plat".into(),
                version: "0.1.0".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: None,
            },
        );
        let b = Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: base.clone(),
                ts: false,
            })),
            Extras {
                fs: Some(Arc::new(grant)),
                platform: Some(platform),
                modules: Arc::new(modules),
                ..Default::default()
            },
        );
        let t = std::time::Duration::from_secs(5);
        let api = base.join("plat/api.js");

        // ① 特权模块：trusted 文件读 + 写都可达（写的是新文件——resolve_existing
        //    逐级上溯到 trusted 根拼回后缀，落点仍在集合内）。
        let cap = b
            .run_module(&api, "readTrusted", Default::default(), t)
            .await
            .unwrap();
        assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
        let cap = b
            .run_module(&api, "writeTrusted", Default::default(), t)
            .await
            .unwrap();
        assert_eq!(cap.status, 200, "{}", String::from_utf8_lossy(&cap.body));
        assert_eq!(std::fs::read_to_string(&out_path).unwrap(), "hi");

        // ② 非特权（run_with 无模块归属）：同一路径两态皆拒（门面 NotCapable）。
        for code in [
            format!(
                r#"(async () => {{ await fs.readTextFile("{}"); }})();"#,
                cfg_path.display()
            ),
            format!(
                r#"(async () => {{ await fs.writeTextFile("{}", "x"); }})();"#,
                out_path.display()
            ),
        ] {
            let e = b.run_with(&code, Default::default()).await.unwrap_err();
            assert!(e.to_string().contains("NotCapable"), "{code} -> {e}");
        }
        // ③ 非特权模块（归属模块不在 platform.modules）：同样拒。
        let mut modules2 = HashMap::new();
        modules2.insert(
            base.join("plat").to_string_lossy().into_owned(),
            ModuleCtx {
                name: "plain".into(),
                version: "0.1.0".into(),
                deps: Arc::new(std::collections::HashSet::new()),
                db: None,
            },
        );
        let b2 = Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            Some(Arc::new(LoaderShared {
                project_root: base.clone(),
                ts: false,
            })),
            Extras {
                fs: b.stable_for_test().fs.clone(),
                platform: b.stable_for_test().platform.clone(),
                modules: Arc::new(modules2),
                ..Default::default()
            },
        );
        let e = b2
            .run_module(&api, "readTrusted", Default::default(), t)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("NotCapable"), "{e}");
    }

    /// root 内 symlink 指向 root 外 → 读仍被拒（canonicalize 双侧封逃逸）。
    /// Windows 无符号链接特权（未开 Developer Mode）时跳过。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_symlink_escape_denied() {
        let root = jail("symlink");
        let outside = jail("symlink-outside");
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        if !crate::bridge::module_loader::try_symlink(
            &outside.join("secret.txt"),
            &root.join("link.txt"),
            false,
        ) {
            eprintln!("skip fs_symlink_escape_denied: symlink unavailable");
            return;
        }
        let b = bridge_with_fs(&root, false);
        let e = b
            .run_with(
                r#"(async () => { await fs.readTextFile("link.txt"); })();"#,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("NotCapable"), "{e}");
    }

    /// 池复用语义不串：同一 Bridge 两次串行请求，jail 内外行为各自稳定。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_pool_reuse_keeps_semantics() {
        let root = jail("pool");
        let outside = jail("pool-outside");
        std::fs::write(outside.join("s.txt"), b"s").unwrap();
        let b = bridge_with_fs(&root, false);
        b.run_with(
            r#"(async () => { await fs.writeFile("in.txt", new Uint8Array([7])); })();"#,
            Default::default(),
        )
        .await
        .unwrap();
        let cap = b
            .run_with(
                r#"(async () => { const u8 = await fs.readFile("in.txt"); json.ok({ n: u8.length, b: u8[0] }); })();"#,
                Default::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["data"], serde_json::json!({ "n": 1, "b": 7 }));
        let e = b
            .run_with(
                r#"(async () => { await fs.readTextFile("../pool-outside/s.txt"); })();"#,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("NotCapable"), "{e}");
    }
}
