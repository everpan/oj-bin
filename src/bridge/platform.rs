//! platform 地基（ojm 批次，设计稿 §1.1/§1.2）：config `platform:` 段的运行时裁决面。
//!
//! 装配期（oj/src/app.rs `assemble_backend`）把 `platform:` 段解析为两个只读 grant
//! 冻结进 StableState（对齐 `FsGrant` 惯例：跨请求只读、不可变）：
//! - [`PlatformGrant`]：特权模块名集合 + canonical 授权路径集合（fs.root ∪ trusted_paths）；
//! - [`ExecGrant`]：模块 × argv 模板 × env 白名单（阶段 3 `exec` 全局消费）。
//!
//! 裁决收敛为两个单点函数（阶段 3/5 复用）：
//! - [`resolve_allowed`]：路径校验单点——相对路径按 fs.root 折叠 + canonicalize
//!   （复用 `fs::join_jailed`/`fs::resolve_existing`，与 fs 轴同一实现，杜绝两套语义漂移），
//!   解析结果必须落在 `fs.root ∪ trusted_paths` 内；
//! - [`assert_platform`]：平台 API 调用资格——从 `ReqState.module` 查特权名单
//!   （归属是请求级、URL/JS 侧不可伪造，设计稿 §1.2）。

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use deno_core::OpState;

use super::ReqState;
use super::StableState;

/// 平台特权授权（config `platform:` 段；装配期构建、冻结进 StableState，跨请求只读）。
#[derive(Debug)]
pub struct PlatformGrant {
    /// 特权模块名集合（config 白名单认定特权；未列入 = NotCapable，fail-closed）。
    pub modules: HashSet<String>,
    /// canonical 后的授权路径根集合：fs.root ∪ trusted_paths（fs.root 由装配期自动
    /// 并入；`fs:` 未配置时仅 trusted_paths——fs 轴本身未配置仍全 deny，见 fs.rs）。
    pub trusted: Vec<PathBuf>,
}

impl PlatformGrant {
    /// 装配期构建：trusted_paths 逐条 canonicalize（不存在 → Err 点名 fail-fast，
    /// 对齐 `FsGrant::new` 风格）；fs.root 自动并入并去重。
    pub fn new(
        modules: Vec<String>,
        trusted_raw: &[std::path::PathBuf],
        fs_root: Option<&std::path::Path>,
    ) -> Result<Self, String> {
        let mut trusted: Vec<PathBuf> = Vec::new();
        if let Some(r) = fs_root {
            trusted.push(r.to_path_buf());
        }
        for p in trusted_raw {
            let c = p.canonicalize().map_err(|e| {
                format!(
                    "platform.trusted_paths canonicalize failed ({}): {e}",
                    p.display()
                )
            })?;
            if !trusted.contains(&c) {
                trusted.push(c);
            }
        }
        Ok(Self {
            modules: modules.into_iter().collect(),
            trusted,
        })
    }
}

/// 命令解析器（DIP，设计稿 §1.1「模板首 token 宿主侧解析」）：模板首 token →
/// 绝对路径。默认实现 [`crate::bridge::exec::default_resolve_command`]
/// （`"oj"` → `current_exe()`，其余父进程 PATH 解析）；测试注入哨兵解析器。
pub type CommandResolver = std::sync::Arc<dyn Fn(&str) -> Result<PathBuf, String> + Send + Sync>;

/// exec 授权（config `platform.exec:` 段；阶段 3 `exec.run` 消费）。
/// resolver 字段不参与 Debug 输出（函数对象），手写实现。
pub struct ExecGrant {
    /// 模块名 → 授权（列出该模块 = 授权其 exec）。
    pub per_module: BTreeMap<String, ModuleExecGrant>,
    /// 命令解析器（生产默认 + 测试注入，见 [`CommandResolver`]）。
    pub resolver: CommandResolver,
}

impl std::fmt::Debug for ExecGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecGrant")
            .field("per_module", &self.per_module)
            .finish_non_exhaustive()
    }
}

/// 单模块 exec 授权：argv 模板原样保存（阶段 3 解析匹配），env 透传白名单。
#[derive(Debug)]
pub struct ModuleExecGrant {
    pub templates: Vec<String>,
    pub env: Vec<String>,
}

impl ExecGrant {
    /// 装配期构建：本阶段只搬运形状，不校验模板语法（阶段 3 解析期报错点名）。
    /// 解析器取生产默认（`"oj"` → current_exe，其余 PATH 解析）。
    pub fn new(per_module: BTreeMap<String, (Vec<String>, Vec<String>)>) -> Self {
        Self::with_resolver(
            per_module,
            std::sync::Arc::new(crate::bridge::exec::default_resolve_command),
        )
    }

    /// 同 [`ExecGrant::new`]，但显式指定命令解析器（测试注入哨兵路径）。
    pub fn with_resolver(
        per_module: BTreeMap<String, (Vec<String>, Vec<String>)>,
        resolver: CommandResolver,
    ) -> Self {
        Self {
            per_module: per_module
                .into_iter()
                .map(|(m, (templates, env))| (m, ModuleExecGrant { templates, env }))
                .collect(),
            resolver,
        }
    }
}

/// 路径校验单点（DRY，阶段 3 exec `${path:}` 占位符与 fs 轴特权扩展共用）：
/// 相对路径按 fs.root 折叠（[`super::fs::join_jailed`]）+ best-effort canonicalize
/// （[`super::fs::resolve_existing`]，与 `op_fs_resolve` 同一实现），解析结果必须落在
/// `fs.root ∪ trusted` 内；绝对路径原样进入同一管道，canonical 结果落在集合内才放行
/// （「绝对路径须恰为集合内 canonical 结果」，设计稿 §1.1）。否则 Err 点名。
pub fn resolve_allowed(
    fs_root: &std::path::Path,
    trusted: &[PathBuf],
    p: &str,
) -> Result<PathBuf, String> {
    let joined = super::fs::join_jailed(fs_root, std::path::Path::new(p));
    let resolved =
        super::fs::resolve_existing(&joined).map_err(|e| format!("path resolve failed: {e}"))?;
    if resolved.starts_with(fs_root) || trusted.iter().any(|t| resolved.starts_with(t)) {
        return Ok(resolved);
    }
    Err(format!(
        "path outside fs.root and trusted_paths: {p} (resolved {})",
        resolved.display()
    ))
}

/// 平台 API 裁决单点（设计稿 §1.2：一个函数收敛全部平台 API 资格判定）。
/// 本阶段先以直接函数调用交付（无 op 接线）；阶段 3（exec）/阶段 5（reloadConfig）
/// 的 op 各自前置调用。tasks/WS 上下文 `ReqState.module = None` → 恒 NotCapable。
pub fn assert_platform(state: &OpState) -> Result<(), deno_error::JsErrorBox> {
    let platform = state
        .borrow::<std::sync::Arc<StableState>>()
        .platform
        .clone();
    let Some(grant) = platform.as_deref() else {
        return Err(deno_error::JsErrorBox::new(
            "NotCapable",
            "platform not configured (config platform: section missing)",
        ));
    };
    let module = state.borrow::<ReqState>().module.clone();
    match module.as_deref() {
        Some(m) if grant.modules.contains(m) => Ok(()),
        Some(m) => Err(deno_error::JsErrorBox::new(
            "NotCapable",
            format!("module '{m}' is not platform-privileged"),
        )),
        None => Err(deno_error::JsErrorBox::new(
            "NotCapable",
            "platform APIs require module ownership (request has no module context)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Extras;
    use crate::bridge::InMemoryKV;
    use crate::bridge::SchemaRegistry;

    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;

    /// 独立临时目录（pid + 测试名防撞）。
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-plattest-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 最小 StableState（platform/exec 注入；其余字段全空）。
    fn stable_with(
        platform: Option<Arc<PlatformGrant>>,
        exec: Option<Arc<ExecGrant>>,
    ) -> Arc<StableState> {
        Arc::new(StableState {
            kv: Arc::new(InMemoryKV::new()),
            dbs: HashMap::new(),
            registry: Arc::new(SchemaRegistry::new()),
            input_contracts: None,
            seq_ensured: std::sync::Mutex::new(HashSet::new()),
            loader: None,
            module_index: None,
            blobs: Arc::new(crate::bridge::blob::BlobRegistry::new()),
            bus: Arc::new(crate::bridge::bus::Bus::new()),
            es: None,
            plugins: Vec::new(),
            modules: Arc::new(HashMap::new()),
            ownership_deny: false,
            sql_guard: crate::bridge::SqlGuard::Off,
            allow_as_tenant: false,
            db_override: None,
            query_limits: crate::bridge::query::QueryLimits::default(),
            sql_trace: crate::bridge::sql_trace::SqlTraceConfig::default(),
            boot: None,
            jwt: None,
            oidc: None,
            sso: None,
            kafkas: Arc::new(crate::bridge::NamedRegistry::new()),
            rabbits: Arc::new(crate::bridge::NamedRegistry::new()),
            tasks_flag: None,
            sql_memo: std::sync::Mutex::new(HashMap::new()),
            mail: None,
            ldap: None,
            db_access: Arc::new(Default::default()),
            db_client: Arc::new(Default::default()),
            db_session: Arc::new(Default::default()),
            client_dbs: Arc::new(crate::bridge::client_db::ClientDbRegistry::new(16)),
            tx_sessions: Arc::new(crate::bridge::db_session::TxSessionRegistry::disabled()),
            audit_section: Arc::new(Default::default()),
            audit: Arc::new(crate::bridge::audit::AuditStore::new(
                &crate::bridge::db_policy::AuditSection::default(),
            )),
            exec_sessions: Arc::new(crate::bridge::db_exec::ExecRegistry::default()),
            db_access_rt: Arc::new(std::sync::Mutex::new(HashMap::new())),
            db_backend: Arc::new(crate::bridge::db_backend::DbBackendRegistry::builtin()),
            config_dir: Arc::new(std::path::PathBuf::from(".")),
            vars: Arc::new(HashMap::new()),
            js_heap_limit: None,
            fs: None,
            platform,
            exec,
            reload: Arc::new(std::sync::OnceLock::new()),
            generic_axes: crate::bridge::generic_axis::GenericAxisRegistry::default(),
            oj_info: Arc::new(serde_json::json!({})),
        })
    }

    /// 构造带指定模块归属的 OpState（BDD S1/S2 用直接函数调用，不接 op）。
    fn opstate(stable: Arc<StableState>, module: Option<&str>) -> OpState {
        let mut st = OpState::new(None);
        st.put(stable);
        st.put(ReqState {
            module: module.map(|s| s.to_string()),
            ..Default::default()
        });
        st
    }

    /// JsErrorBox 的 Display 只含 message；class（NotCapable）经 JsErrorClass 单查。
    fn assert_not_capable(e: &deno_error::JsErrorBox, msg_part: &str) {
        use deno_error::JsErrorClass;
        assert_eq!(e.get_class(), "NotCapable", "class of {e}");
        assert!(e.to_string().contains(msg_part), "{e}");
    }

    // ----- BDD S1：未配 platform → platform not configured -----

    #[test]
    fn s1_platform_not_configured_denies() {
        let st = opstate(stable_with(None, None), Some("a"));
        let e = assert_platform(&st).unwrap_err();
        assert_not_capable(&e, "platform not configured");
    }

    // ----- BDD S2：名单含 "a" → a 通过 / b 点名拒绝 / 无归属拒绝 -----

    #[test]
    fn s2_privilege_matrix() {
        let grant_of = || Arc::new(PlatformGrant::new(vec!["a".into()], &[], None).unwrap());
        let st = opstate(stable_with(Some(grant_of()), None), Some("a"));
        assert_platform(&st).expect("privileged module passes");

        let st_b = opstate(stable_with(Some(grant_of()), None), Some("b"));
        let e = assert_platform(&st_b).unwrap_err();
        assert_not_capable(&e, "module 'b' is not platform-privileged");

        // tasks/WS 上下文无归属（module=None）→ 恒 NotCapable（设计稿 §1.2）。
        let st_none = opstate(stable_with(Some(grant_of()), None), None);
        let e = assert_platform(&st_none).unwrap_err();
        assert_not_capable(&e, "module ownership");
    }

    // ----- BDD S3：路径校验三态（jail 内相对 OK / ../ 折叠越界拒 / trusted 文件 OK）-----

    #[test]
    fn s3_resolve_allowed_three_states() {
        // macOS 下 temp_dir 经 symlink（/var → /private/var）：统一 canonical，
        // 否则 starts_with 前缀比对被符号链接形态打穿。
        let root = tmp("s3-root").canonicalize().unwrap();
        let other = tmp("s3-trusted").canonicalize().unwrap();
        let outside = tmp("s3-outside").canonicalize().unwrap();
        std::fs::write(root.join("in.txt"), b"1").unwrap();
        std::fs::write(other.join("config.yaml"), b"cfg").unwrap();
        std::fs::write(outside.join("secret.txt"), b"s").unwrap();
        let trusted = vec![other.clone()];

        // ① jail 内相对路径 → Ok（canonical 绝对路径返回）。
        let r = resolve_allowed(&root, &trusted, "in.txt").unwrap();
        assert_eq!(r, root.join("in.txt"));

        // ② `../` 折叠后越界（root 与 trusted 皆不在内）→ Err 点名。
        let e = resolve_allowed(&root, &trusted, "../oj-plattest-nope/x").unwrap_err();
        assert!(e.contains("path outside fs.root and trusted_paths"), "{e}");
        let e = resolve_allowed(&root, &trusted, "../s3-outside/secret.txt").unwrap_err();
        assert!(e.contains("path outside fs.root and trusted_paths"), "{e}");

        // ③ trusted_paths 内文件（集合外根）：绝对 canonical 形态放行；集合外绝对路径拒。
        let cfg = other.join("config.yaml");
        let r = resolve_allowed(&root, &trusted, &cfg.to_string_lossy()).unwrap();
        assert_eq!(r, cfg);
        let e = resolve_allowed(
            &root,
            &trusted,
            &outside.join("secret.txt").to_string_lossy(),
        )
        .unwrap_err();
        assert!(e.contains("path outside fs.root and trusted_paths"), "{e}");

        // 补充：相对路径折叠后恰好落在 trusted 根内也放行（../config.yaml 形态——
        // fs.root 折叠是唯一相对基准，落点按集合裁决）。
        let rel = format!("../{}", other.file_name().unwrap().to_string_lossy());
        let r = resolve_allowed(&root, &trusted, &format!("{rel}/config.yaml")).unwrap();
        assert_eq!(r, cfg);
    }

    // ----- grant 构建行为：canonicalize fail-fast + fs.root 并入去重 -----

    #[test]
    fn grant_new_canonicalizes_and_unions_fs_root() {
        let root = tmp("grant-root").canonicalize().unwrap();
        let other = tmp("grant-trusted").canonicalize().unwrap();
        let missing = root.join("no-such-dir");
        // 不存在 → Err 点名（fail-fast，对齐 FsGrant::new）。
        let e =
            PlatformGrant::new(vec![], std::slice::from_ref(&missing), Some(&root)).unwrap_err();
        assert!(
            e.contains("platform.trusted_paths canonicalize failed"),
            "{e}"
        );
        // fs.root 自动并入；重复项去重。
        let g = PlatformGrant::new(
            vec!["m".into()],
            &[root.clone(), other.clone()],
            Some(&root),
        )
        .unwrap();
        assert_eq!(g.trusted.len(), 2);
        assert!(g.trusted.contains(&root));
        assert!(g.trusted.contains(&other));
        assert!(g.modules.contains("m"));
        // fs: 未配置 → 仅 trusted_paths。
        let g = PlatformGrant::new(vec![], std::slice::from_ref(&other), None).unwrap();
        assert_eq!(g.trusted, vec![other]);
    }

    #[test]
    fn exec_grant_carries_templates_as_is() {
        let g = ExecGrant::new(
            [(
                "ojm-installer".to_string(),
                (
                    vec!["tar -xzf ${path:archive} -C ${path:dest}".to_string()],
                    vec!["OJ_SECRET_KEY".to_string()],
                ),
            )]
            .into(),
        );
        let mg = g.per_module.get("ojm-installer").unwrap();
        assert_eq!(
            mg.templates,
            vec!["tar -xzf ${path:archive} -C ${path:dest}"]
        );
        assert_eq!(mg.env, vec!["OJ_SECRET_KEY"]);
        // 模板原样保存，本阶段不做语法校验（阶段 3 解析期点名）。
        let g2 = ExecGrant::new([("m".to_string(), (vec!["??? bad".to_string()], vec![]))].into());
        assert_eq!(g2.per_module["m"].templates, vec!["??? bad"]);
    }

    /// Extras → StableState 注入回路（Bridge 构造期拷贝，同 FsGrant 惯例）。
    #[test]
    fn extras_platform_reaches_stable_state() {
        let root = tmp("extras-root");
        let grant = Arc::new(
            PlatformGrant::new(vec!["x".into()], std::slice::from_ref(&root), None).unwrap(),
        );
        let b = crate::bridge::Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                platform: Some(grant.clone()),
                exec: Some(Arc::new(ExecGrant::new(
                    [("x".to_string(), (vec!["t".to_string()], vec![]))].into(),
                ))),
                ..Default::default()
            },
        );
        let stable = b.stable_for_test();
        assert!(stable.platform.is_some());
        assert!(stable.platform.as_deref().unwrap().modules.contains("x"));
        assert!(stable.exec.is_some());
        assert!(Path::new(&stable.platform.as_deref().unwrap().trusted[0]).is_absolute());
    }
}
