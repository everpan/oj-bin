//! JWT 鉴权核心（OJ-4）：签发/验签、匿名路径匹配、login/refresh/logout（bcrypt + session 轮换）。
//! session 存 KV（v0.1 内存；Phase 6 换 RedisKV 单点替换），键 = "AUTH-SESSION:" + sha256(refresh_token)。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mdm_base_rust::bridge::{DataAccessor, KVStore};
use mdm_base_rust::config::AuthCfg;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// access token 载荷（refresh 是不透明随机串，无 JWT claims）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Claims {
    pub sub: String,
    pub roles: Vec<String>,
    pub iat: u64,
    pub exp: u64,
}

pub struct Auth {
    enc: jsonwebtoken::EncodingKey,
    dec: jsonwebtoken::DecodingKey,
    alg: jsonwebtoken::Algorithm,
    access: Duration,
    refresh: Duration,
    anon: Vec<String>,
    pub user_table: String,
    db: Arc<dyn DataAccessor>,
    kv: Arc<dyn KVStore>,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// 32 随机字节 hex = 不透明 refresh token。
fn refresh_token() -> String {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).expect("system rng");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn session_key(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("AUTH-SESSION:{:x}", h.finalize())
}

impl Auth {
    /// 构造：alg 解析 + user_table 标识符白名单 + duration 解析（fail-fast）。
    pub fn new(cfg: &AuthCfg, db: Arc<dyn DataAccessor>, kv: Arc<dyn KVStore>) -> Result<Self, String> {
        let alg = match cfg.signing_method.as_str() {
            "HS256" => jsonwebtoken::Algorithm::HS256,
            "HS384" => jsonwebtoken::Algorithm::HS384,
            "HS512" => jsonwebtoken::Algorithm::HS512,
            other => return Err(format!("auth.signing_method '{other}' not supported (HS256|HS384|HS512)")),
        };
        if cfg.user_table.is_empty()
            || cfg.user_table.len() > 64
            || !cfg.user_table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("auth.user_table '{}' invalid ([A-Za-z0-9_]{{1,64}})", cfg.user_table));
        }
        let access = mdm_base_rust::config::parse_duration(&cfg.access_token_duration)
            .map_err(|e| format!("auth.access_token_duration: {e}"))?;
        let refresh = mdm_base_rust::config::parse_duration(&cfg.refresh_token_duration)
            .map_err(|e| format!("auth.refresh_token_duration: {e}"))?;
        Ok(Self {
            enc: jsonwebtoken::EncodingKey::from_secret(cfg.jwt_secret.as_bytes()),
            dec: jsonwebtoken::DecodingKey::from_secret(cfg.jwt_secret.as_bytes()),
            alg,
            access,
            refresh,
            anon: cfg.anonymous_paths.clone(),
            user_table: cfg.user_table.clone(),
            db,
            kv,
        })
    }

    /// 签 access token（iat/exp = now/now+access）。
    pub fn sign_access(&self, user_id: &str, roles: &[String]) -> String {
        let now = now_unix();
        let claims = Claims {
            sub: user_id.to_string(),
            roles: roles.to_vec(),
            iat: now,
            exp: now + self.access.as_secs(),
        };
        jsonwebtoken::encode(&jsonwebtoken::Header::new(self.alg), &claims, &self.enc)
            .expect("jwt encode")
    }

    /// 验签 + exp（leeway 0；篡改/过期/算法不符均 Err）。
    pub fn verify_access(&self, token: &str) -> Result<Claims, String> {
        let mut v = jsonwebtoken::Validation::new(self.alg);
        v.leeway = 0;
        v.validate_exp = true;
        v.validate_aud = false;
        jsonwebtoken::decode::<Claims>(token, &self.dec, &v)
            .map(|d| d.claims)
            .map_err(|e| e.to_string())
    }

    /// 免鉴权路径：精确匹配或尾 "/*" 一层前缀通配（"/pub/*" 命中 "/pub/x"，不命中 "/pub"）。
    pub fn is_anonymous(&self, path_no_base: &str) -> bool {
        self.anon.iter().any(|p| {
            if let Some(prefix) = p.strip_suffix("/*") {
                path_no_base.starts_with(prefix) && path_no_base.len() > prefix.len()
            } else {
                path_no_base == p
            }
        })
    }

    /// 登录：users 表 bcrypt 校验 → {access_token, refresh_token, expires_in, user}。
    /// 用户不存在与密码错同报 "invalid credentials"（不泄露用户存在性）。
    pub async fn login(&self, username: &str, password: &str) -> Result<Value, String> {
        let rows = self
            .db
            .query_with_params(
                &format!(
                    "select id, password_hash, roles from {} where username = ?",
                    self.user_table
                ),
                &[json!(username)],
            )
            .await
            .map_err(|e| format!("auth login query: {e}"))?;
        let Some(row) = rows.into_iter().next() else {
            return Err("invalid credentials".into());
        };
        let hash = row["password_hash"].as_str().unwrap_or_default();
        if !bcrypt::verify(password, hash).unwrap_or(false) {
            return Err("invalid credentials".into());
        }
        // roles 列按 JSON 数组串解析，失败回落空。
        let roles: Vec<String> = row["roles"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let uid = match &row["id"] {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => return Err("auth login: user id not representable".into()),
        };
        Ok(self.token_pair(&uid, &roles).await)
    }

    /// refresh 轮换：session 未过期 → 签新对 + 删旧 session（旧 refresh 立即失效）。
    pub async fn refresh(&self, refresh_token: &str) -> Result<Value, String> {
        let Some(uid) = self.session_get(refresh_token).await else {
            return Err("invalid or expired refresh token".into());
        };
        // roles 从 KV 里拿不到（session 只存 uid）——重查库取最新。
        let rows = self
            .db
            .query_with_params(
                &format!("select roles from {} where id = ?", self.user_table),
                &[json!(uid)],
            )
            .await
            .map_err(|e| format!("auth refresh query: {e}"))?;
        let roles: Vec<String> = rows
            .first()
            .and_then(|r| r["roles"].as_str())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        self.session_del(refresh_token).await;
        Ok(self.token_pair(&uid, &roles).await)
    }

    /// 登出：删 session（access 到期前仍有效——JWT 无服务端吊销，属已知取舍）。
    pub async fn logout(&self, refresh_token: &str) -> Result<(), String> {
        self.session_del(refresh_token).await;
        Ok(())
    }

    /// 签 access + refresh 并落 session（exp = now + refresh 时长，session_get 惰性判定）。
    async fn token_pair(&self, uid: &str, roles: &[String]) -> Value {
        let rt = refresh_token();
        self.session_put(&rt, uid).await;
        json!({
            "access_token": self.sign_access(uid, roles),
            "refresh_token": rt,
            "expires_in": self.access.as_secs(),
            "user": { "id": uid, "roles": roles },
        })
    }

    async fn session_put(&self, token: &str, uid: &str) {
        let exp = now_unix() + self.refresh.as_secs();
        let _ = self.kv.set(&session_key(token), &json!({ "uid": uid, "exp": exp }).to_string()).await;
    }

    async fn session_get(&self, token: &str) -> Option<String> {
        let raw = self.kv.get(&session_key(token)).await.ok().flatten()?;
        let v: Value = serde_json::from_str(&raw).ok()?;
        (v["exp"].as_u64().unwrap_or(0) > now_unix())
            .then(|| v["uid"].as_str().map(|s| s.to_string()))?

    }

    async fn session_del(&self, token: &str) {
        let _ = self.kv.del(&session_key(token)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdm_base_rust::bridge::{InMemoryAccessor, InMemoryKV};

    fn auth_cfg() -> AuthCfg {
        AuthCfg {
            jwt_secret: "test-secret".into(),
            ..Default::default()
        }
    }

    fn test_auth() -> Auth {
        Auth::new(
            &auth_cfg(),
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()),
        )
        .unwrap()
    }

    #[test]
    fn sign_verify_roundtrip_and_tamper() {
        let a = test_auth();
        let t = a.sign_access("7", &["admin".to_string()]);
        let c = a.verify_access(&t).unwrap();
        assert_eq!((c.sub.as_str(), c.roles.len()), ("7", 1));
        assert!(a.verify_access(&format!("{t}x")).is_err());
        // 过期：手工签一个 exp 在过去的 token
        let claims = Claims { sub: "7".into(), roles: vec![], iat: 100, exp: 101 };
        let past = jsonwebtoken::encode(&jsonwebtoken::Header::new(a.alg), &claims, &a.enc).unwrap();
        assert!(a.verify_access(&past).is_err());
    }

    #[test]
    fn anonymous_path_matching() {
        let mut a = test_auth();
        a.anon = vec!["/health".into(), "/pub/*".into()];
        assert!(a.is_anonymous("/health") && a.is_anonymous("/pub/x"));
        assert!(!a.is_anonymous("/health/x") && !a.is_anonymous("/pub") && !a.is_anonymous("/other"));
    }

    // ----- Task 4.3：login/refresh/logout（真实 sqlite）-----

    fn auth_cfg_db() -> AuthCfg {
        AuthCfg { jwt_secret: "test-secret".into(), ..Default::default() }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn login_refresh_logout_flow() {
        let db = mdm_base_rust::bridge::SqlxAccessor::arc("sqlite::memory:").await.unwrap();
        let hash = bcrypt::hash("pw123", 4).unwrap();
        db.exec_with_params(
            "create table users (id integer primary key, username text, password_hash text, roles text)",
            &[],
        )
        .await
        .unwrap();
        db.exec_with_params(
            "insert into users (username, password_hash, roles) values (?, ?, ?)",
            &[json!("u"), json!(hash), json!(r#"["admin"]"#)],
        )
        .await
        .unwrap();
        let a = Auth::new(&auth_cfg_db(), db, Arc::new(InMemoryKV::new())).unwrap();
        // 密码错 → 401 语义（Err）
        assert!(a.login("u", "wrong").await.is_err());
        // 用户不存在 → 同报（不区分）
        assert_eq!(a.login("nope", "x").await.unwrap_err(), "invalid credentials");
        // 成功 → 双 token + user
        let v = a.login("u", "pw123").await.unwrap();
        assert!(v["access_token"].is_string() && v["refresh_token"].is_string());
        assert_eq!(v["user"]["roles"][0], "admin");
        assert_eq!(v["expires_in"], 60);
        // access token 可验签
        assert_eq!(a.verify_access(v["access_token"].as_str().unwrap()).unwrap().sub, "1");
        // refresh 轮换：旧 refresh 二次使用失败
        let r1 = v["refresh_token"].as_str().unwrap().to_string();
        let v2 = a.refresh(&r1).await.unwrap();
        assert!(a.refresh(&r1).await.is_err(), "old refresh must rotate out");
        // logout 删 session
        let r2 = v2["refresh_token"].as_str().unwrap().to_string();
        a.logout(&r2).await.unwrap();
        assert!(a.refresh(&r2).await.is_err());
    }
}
