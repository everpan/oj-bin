//! oj-auth：auth 轴守卫 cdylib 插件（auth 解耦）。只含守卫（验签 + 匿名匹配）；
//! login/refresh/logout 端点已 JS 化（sample/src/auth/），本插件无 db/kv 依赖。
//! cfg 契约：init cfg = {"jwt_secret","signing_method","anonymous_paths":[...]} JSON。

use oj_plugin_ffi::{
    AuthGuardVtable, HostContext, PluginDescriptor, PluginRegistrations, RArc, RResult, RString,
};
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

    /// 精确匹配或尾 "/*" 一层前缀通配（"/pub/*" 命中 "/pub/x"，不命中 "/pub"）。
    fn is_anonymous(&self, path: &str) -> bool {
        self.anon.iter().any(|p| {
            if let Some(prefix) = p.strip_suffix("/*") {
                path.starts_with(prefix) && path.len() > prefix.len()
            } else {
                path == p
            }
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

extern "C" fn register() -> PluginRegistrations {
    oj_plugin_ffi::catch_value(
        || PluginRegistrations {
            es: std::ptr::null(),
            db: std::ptr::null(),
            blob: std::ptr::null(),
            bus: std::ptr::null(),
            kv: std::ptr::null(),
            auth: &VTABLE,
        },
        PluginRegistrations::none(),
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
        register,
    })
}

oj_plugin_ffi::oj_plugin_entry!(init);

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn anonymous_matching() {
        let g = guard();
        assert!(g.is_anonymous("/health") && g.is_anonymous("/auth/login"));
        assert!(!g.is_anonymous("/auth") && !g.is_anonymous("/me"));
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
}
