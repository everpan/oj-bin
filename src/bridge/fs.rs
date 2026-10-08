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
    let parser = RuntimePermissionDescriptorParser::new(sys_traits::impls::RealSys);
    let root_str = |g: &FsGrant| g.root.to_string_lossy().into_owned();
    let opts = PermissionsOptions {
        allow_env: None,
        deny_env: None,
        ignore_env: None,
        allow_net: None,
        deny_net: None,
        allow_ffi: None,
        deny_ffi: None,
        allow_read: grant.map(|g| vec![root_str(g)]),
        deny_read: None,
        ignore_read: None,
        allow_run: None,
        deny_run: None,
        allow_sys: None,
        deny_sys: None,
        // readonly 时 write 轴不给任何 allow → 整体 deny。
        allow_write: grant.filter(|g| !g.readonly).map(|g| vec![root_str(g)]),
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

/// 暴露给 bootstrap 面面的 jail 根（None = 未配置）：相对路径在门面层解析到
/// root 之下（deno 原生语义是解析到进程 cwd，与 jail 模型不符）。
#[op2]
#[string]
pub fn op_fs_root(state: &mut OpState) -> Option<String> {
    state
        .borrow::<Arc<crate::bridge::StableState>>()
        .fs
        .as_ref()
        .map(|g| g.root.to_string_lossy().into_owned())
}

/// 门面层的 jail 裁决（v1 安全关键）：对（已拼上 root 的）路径做 best-effort
/// canonicalize——整体不存在（写新文件）时逐级上溯到最近存在的祖先再拼回后缀——
/// 结果落在 root 外，或 fs: 未配置，一律 `NotCapable`；落在 root 内则返回
/// canonical 绝对路径供 deno op 使用，op 内的权限检查成为第二道防线。
/// ponytail: 检查与使用之间存在 TOCTOU 窗口（symbolic link 交换），对
/// 「防误触/防越界默认值」的威胁模型可接受；要堵则需 openat2 族，代价不值。
#[op2]
#[string]
pub fn op_fs_resolve(
    state: &mut OpState,
    #[string] path: String,
) -> Result<String, deno_error::JsErrorBox> {
    let grant = state.borrow::<Arc<crate::bridge::StableState>>().fs.clone();
    let grant = grant.ok_or_else(|| {
        deno_error::JsErrorBox::new(
            "NotCapable",
            "fs not configured (config fs: section missing)",
        )
    })?;
    let p = Path::new(&path);
    if !p.is_absolute() {
        return Err(deno_error::JsErrorBox::new(
            "NotCapable",
            format!("path is not absolute: {path}"),
        ));
    }
    let resolved = resolve_existing(p)?;
    if !resolved.starts_with(&grant.root) {
        return Err(deno_error::JsErrorBox::new(
            "NotCapable",
            format!("path escapes fs.root: {path}"),
        ));
    }
    Ok(resolved.to_string_lossy().into_owned())
}

/// best-effort canonicalize：逐级上溯到最近存在的祖先，canonicalize 后拼回后缀。
fn resolve_existing(p: &Path) -> Result<PathBuf, deno_error::JsErrorBox> {
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
        let abs = format!(
            r#"(async () => {{ await fs.readTextFile("{}"); }})();"#,
            outside.join("secret.txt").display()
        );
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

    /// root 内 symlink 指向 root 外 → 读仍被拒（canonicalize 双侧封逃逸）。
    #[tokio::test(flavor = "current_thread")]
    async fn fs_symlink_escape_denied() {
        let root = jail("symlink");
        let outside = jail("symlink-outside");
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
        #[cfg(not(unix))]
        std::os::windows::fs::symlink_file(outside.join("secret.txt"), root.join("link.txt"))
            .unwrap();
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
