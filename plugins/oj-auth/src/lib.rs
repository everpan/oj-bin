//! oj-auth：auth 轴守卫 cdylib 插件（auth 解耦）。只含守卫（验签 + 匿名匹配）；
//! login/refresh/logout 端点已 JS 化（sample/src/auth/），本插件无 db/kv 依赖。
//! cfg 契约：init cfg = {"jwt_secret","signing_method","anonymous_paths":[...]} JSON。

use oj_plugin_ffi::{AuthGuardVtable, HostContext, PluginDescriptor, RArc, RResult, RString};
use std::sync::OnceLock;

/// access token 载荷（与 core bridge crypto.rs Claims 同形）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Claims {
    sub: String,
    roles: Vec<String>,
    iat: u64,
    exp: u64,
}

#[derive(serde::Deserialize)]
struct GuardCfg {
    /// 扫描模式 init cfg = "{}"（auth 未声明）→ 空 secret 守卫仍可建（fail-closed：
    /// 任何 Bearer 都验签失败）；装配层注入真实 cfg（清单/扫描同一路径）。
    #[serde(default)]
    jwt_secret: String,
    #[serde(default = "default_alg")]
    signing_method: String,
    #[serde(default)]
    anonymous_paths: Vec<String>,
}

fn default_alg() -> String {
    "HS256".into()
}

struct Guard {
    dec: jsonwebtoken::DecodingKey,
    alg: jsonwebtoken::Algorithm,
    anon: Vec<String>,
}

static GUARD: OnceLock<Guard> = OnceLock::new();

impl Guard {
    fn new(cfg: &GuardCfg) -> Result<Self, String> {
        let alg = match cfg.signing_method.as_str() {
            "HS256" => jsonwebtoken::Algorithm::HS256,
            "HS384" => jsonwebtoken::Algorithm::HS384,
            "HS512" => jsonwebtoken::Algorithm::HS512,
            other => return Err(format!("signing_method '{other}' not supported")),
        };
        Ok(Self {
            dec: jsonwebtoken::DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
            alg,
            anon: cfg.anonymous_paths.clone(),
        })
    }

    /// 路径通配匹配：**与 `server::path_matches` 同语义的两份实现之一**（插件不能依赖
    /// server crate，两处各自持有、注释互指；改一侧必须同步另一侧与两侧单测矩阵）。
    ///
    /// v0.1.20 统一语义：字面 / `*` 恰好一段（尾 `/*` 仍是严格一层，**不**再像旧版那样
    /// 任意深度）/ `**` 跨段（≥0 段）。旧版尾 `*` 是 `starts_with` 任意深度，与自身注释
    /// 和文档矛盾 —— 收紧后深路径请显式写 `/x/**`（迁移提示见 CHANGELIST v0.1.20）。
    fn is_anonymous(&self, path: &str) -> bool {
        let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        self.anon.iter().any(|p| {
            let pat: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
            segments_match(&pat, &seg)
        })
    }

    /// 验签 + exp（leeway 0）；null = 匿名放行；对象 = {"id","roles","claims"}。
    fn verify(&self, path: &str, authorization: &str) -> Result<serde_json::Value, String> {
        if self.is_anonymous(path) {
            return Ok(serde_json::Value::Null);
        }
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or("missing or invalid bearer token")?;
        let mut v = jsonwebtoken::Validation::new(self.alg);
        v.leeway = 0;
        v.validate_exp = true;
        v.validate_aud = false;
        let claims = jsonwebtoken::decode::<Claims>(token, &self.dec, &v)
            .map(|d| d.claims)
            .map_err(|_| "missing or invalid bearer token".to_string())?;
        Ok(serde_json::json!({
            "id": claims.sub,
            "roles": claims.roles,
            "claims": claims,
        }))
    }
}

static VTABLE: AuthGuardVtable = AuthGuardVtable { verify };

extern "C" fn verify(path: RString, authorization: RString) -> RResult<RString, RString> {
    oj_plugin_ffi::catch_value(
        || {
            let Some(g) = GUARD.get() else {
                return RResult::Err(RString::from("oj-auth: init not called"));
            };
            match g.verify(&path[..], &authorization[..]) {
                Ok(v) => RResult::Ok(RString::from(v.to_string())),
                Err(msg) => RResult::Err(RString::from(msg.as_str())),
            }
        },
        RResult::Err(RString::from("panic in oj-auth verify")),
    )
}

fn init(_host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    let parsed: GuardCfg = match serde_json::from_str(&cfg[..]) {
        Ok(c) => c,
        Err(e) => return RResult::Err(RString::from(format!("oj-auth cfg: {e}"))),
    };
    let guard = match Guard::new(&parsed) {
        Ok(g) => g,
        Err(e) => return RResult::Err(RString::from(e)),
    };
    let _ = GUARD.set(guard);
    RResult::Ok(PluginDescriptor {
        name: RString::from("auth"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from(
            "auth 轴守卫 cdylib 插件：JWT 验签 + 匿名路径匹配（迁自 server/auth.rs）",
        ),
    })
}

/// 段序列匹配（`*` 恰好一段 / `**` 跨段 ≥0 段；与 server 侧 `segments_match` 同形）。
///
/// `**` 用回溯试 0..=n 段：模式与路径都很短（路径段数 < 20），开销可忽略。
fn segments_match(pat: &[&str], seg: &[&str]) -> bool {
    match (pat.first(), seg.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(p), _) if *p == "**" => (0..=seg.len()).any(|k| segments_match(&pat[1..], &seg[k..])),
        (Some(_), None) => false,
        (Some(p), Some(s)) => (*p == *s || *p == "*") && segments_match(&pat[1..], &seg[1..]),
    }
}

oj_plugin_ffi::oj_plugin_entry!(init, auth => &VTABLE);

#[cfg(test)]
mod tests {
    use super::*;
    use oj_plugin_ffi::RBytes;

    fn guard() -> Guard {
        Guard::new(&GuardCfg {
            jwt_secret: "s3cret".into(),
            signing_method: "HS256".into(),
            anonymous_paths: vec!["/health".into(), "/auth/*".into()],
        })
        .unwrap()
    }

    fn sign(g: &Guard, sub: &str, roles: &[&str], exp_offset: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = serde_json::json!({
            "sub": sub, "roles": roles,
            "iat": now, "exp": now + exp_offset,
        });
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(g.alg),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"s3cret"),
        )
        .unwrap()
    }

    /// 与 `server::path_matches` 同语义矩阵（防两处实现漂移；v0.1.20）。
    #[test]
    fn anonymous_matching() {
        let g = guard();
        assert!(g.is_anonymous("/health") && g.is_anonymous("/auth/login"));
        assert!(!g.is_anonymous("/auth") && !g.is_anonymous("/me"));
        // 尾 `/*` 严格一层：不命中两层（旧版 starts_with 会命中，v0.1.20 收紧）。
        assert!(!g.is_anonymous("/auth/a/b"));
    }

    #[test]
    fn anonymous_matching_glob_forms() {
        let g = Guard::new(&GuardCfg {
            jwt_secret: "s3cret".into(),
            signing_method: "HS256".into(),
            anonymous_paths: vec![
                "/public/**".into(),
                "/pub/anchor/*/states".into(),
                "/a/*/b/*".into(),
            ],
        })
        .unwrap();
        // `**` 跨段（≥0 段）
        assert!(g.is_anonymous("/public"));
        assert!(g.is_anonymous("/public/a"));
        assert!(g.is_anonymous("/public/a/b/c"));
        assert!(!g.is_anonymous("/publik/a"));
        // 中段 `*`：恰好一段
        assert!(g.is_anonymous("/pub/anchor/v1c/states"));
        assert!(!g.is_anonymous("/pub/anchor/v1c/x/states"));
        assert!(!g.is_anonymous("/pub/anchor/v1c/states/x"));
        // 单模式多 `*`
        assert!(g.is_anonymous("/a/1/b/2"));
        assert!(!g.is_anonymous("/a/1/b/2/3"));
    }

    #[test]
    fn verify_anonymous_valid_tampered_expired() {
        let g = guard();
        assert_eq!(g.verify("/health", "").unwrap(), serde_json::Value::Null);
        let t = sign(&g, "1", &["admin"], 60);
        let u = g.verify("/me", &format!("Bearer {t}")).unwrap();
        assert_eq!(u["id"], "1");
        assert_eq!(u["roles"][0], "admin");
        assert!(g.verify("/me", &format!("Bearer {t}x")).is_err());
        assert!(g.verify("/me", "no-bearer").is_err());
        assert!(g.verify("/me", "").is_err());
        let past = sign(&g, "1", &[], -60);
        assert!(g.verify("/me", &format!("Bearer {past}")).is_err());
    }

    extern "C" fn nl(_level: u8, _msg: RString) {}
    extern "C" fn nd(_topic: RString, _payload: RBytes) {}
    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: nl,
            deliver: nd,
        })
    }

    #[test]
    fn init_rejects_bad_cfg_and_bad_alg_then_describes() {
        // init 门禁语义：坏 cfg JSON → 点名 oj-auth cfg；不支持的方法 → not supported。
        let Err(m) = std::result::Result::from(init(host(), RString::from("{bad json"))) else {
            panic!("bad cfg must fail")
        };
        assert!(m[..].contains("oj-auth cfg"), "{}", &m[..]);
        let Err(m) =
            std::result::Result::from(init(host(), RString::from(r#"{"signing_method":"RS256"}"#)))
        else {
            panic!("unsupported alg must fail")
        };
        assert!(m[..].contains("not supported"), "{}", &m[..]);
        // 合法 init（缺省 HS256）→ 自描述 descriptor。
        let d = std::result::Result::from(init(host(), RString::from(r#"{"jwt_secret":"k"}"#)))
            .unwrap();
        assert_eq!(&d.name[..], "auth");
    }

    #[test]
    fn hs384_and_hs512_signing_methods_accepted() {
        for alg in ["HS384", "HS512"] {
            assert!(
                Guard::new(&GuardCfg {
                    jwt_secret: "k".into(),
                    signing_method: alg.into(),
                    anonymous_paths: vec![],
                })
                .is_ok()
            );
        }
    }
}
