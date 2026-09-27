//! ldap 全局对象：`ldap.*` / `LDAP(name)` → oj-ldap 插件（ldap3）。
//! 宿主职责：配置白名单校验 + 调用入参校验 + FfiFuture 驱动；连接、绑定与协议
//! 交互全在插件（凭据不落宿主——`ldap:` 段原样透传，宿主只认键做校验与选单）。
//! 错误模型同 db：除「未配置」外，校验/协议/网络错一律经 op 抛错（Promise reject）；
//! `ldap.bind(dn,pw)` 返回 bool（凭据被 LDAP 拒绝 = false，非错误）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{Value, json};

use super::BridgeResult;

/// 单个实例的白名单校验面（`ldap:` 段的一条值）。
#[derive(Debug, Clone)]
pub struct LdapInstanceCfg {
    pub url: String,
    pub bind_dn: Option<String>,
    pub bind_pw: Option<String>,
    pub timeout_ms: Option<u64>,
    pub start_tls: Option<bool>,
    pub tls_skip_verify: Option<bool>,
}

/// `ldap:` 段宿主校验面：吃 `plugin_cfg(cfg, "ldap")` 的同一份 JSON（单一真相源），
/// 键 = 实例名。未知键/形态错误 → Err（装配期 fail-fast，不静默忽略拼错的配置）。
#[derive(Debug, Default)]
pub struct LdapConfig {
    instances: HashMap<String, LdapInstanceCfg>,
}

impl LdapConfig {
    /// 从段 JSON 构造（白名单：url/bind_dn/bind_pw/timeout_ms/start_tls/tls_skip_verify）。
    pub fn from_value(v: &Value) -> BridgeResult<Self> {
        let obj = v
            .as_object()
            .ok_or("ldap: config section must be an object of instances")?;
        let mut instances = HashMap::new();
        for (name, iv) in obj {
            let io = iv
                .as_object()
                .ok_or_else(|| format!("ldap instance '{name}': must be an object"))?;
            for k in io.keys() {
                if !matches!(
                    k.as_str(),
                    "url" | "bind_dn" | "bind_pw" | "timeout_ms" | "start_tls" | "tls_skip_verify"
                ) {
                    return Err(format!("ldap instance '{name}': unknown key '{k}'").into());
                }
            }
            let url = io
                .get("url")
                .and_then(Value::as_str)
                .filter(|s| s.starts_with("ldap://") || s.starts_with("ldaps://"))
                .ok_or_else(|| {
                    format!("ldap instance '{name}': url is required (ldap:// or ldaps://)")
                })?
                .to_string();
            let str_opt = |k: &str| io.get(k).and_then(Value::as_str).map(str::to_string);
            let u64_opt = |k: &str, lo: u64, hi: u64| -> BridgeResult<Option<u64>> {
                match io.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(n) => {
                        let n = n
                            .as_u64()
                            .ok_or_else(|| format!("ldap instance '{name}': {k} must be a u64"))?;
                        if !(lo..=hi).contains(&n) {
                            return Err(format!(
                                "ldap instance '{name}': {k} must be in {lo}..={hi}"
                            )
                            .into());
                        }
                        Ok(Some(n))
                    }
                }
            };
            let bool_opt = |k: &str| -> BridgeResult<Option<bool>> {
                match io.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(b) => b.as_bool().map(Some).ok_or_else(|| {
                        format!("ldap instance '{name}': {k} must be a bool").into()
                    }),
                }
            };
            instances.insert(
                name.clone(),
                LdapInstanceCfg {
                    url,
                    bind_dn: str_opt("bind_dn"),
                    bind_pw: str_opt("bind_pw"),
                    timeout_ms: u64_opt("timeout_ms", 100, 3_600_000)?,
                    start_tls: bool_opt("start_tls")?,
                    tls_skip_verify: bool_opt("tls_skip_verify")?,
                },
            );
        }
        Ok(Self { instances })
    }

    pub fn has(&self, name: &str) -> bool {
        self.instances.contains_key(name)
    }

    pub fn instance_keys(&self) -> Vec<String> {
        let mut ks: Vec<String> = self.instances.keys().cloned().collect();
        ks.sort();
        ks
    }
}

/// ldap 轴后端契约（依赖倒置：核心只依赖本 trait；FFI 细节在 [`FfiLdapBackend`]）。
#[async_trait]
pub trait LdapBackend: Send + Sync {
    /// 实例表（宿主校验面；`call` 的 `key` 选单依据）。
    fn config(&self) -> &LdapConfig;
    /// 执行一个 LDAP 操作。`req` = 已校验的调用 JSON（`op`/`key`/op 参数）。
    async fn call(&self, req: Value) -> BridgeResult<Value>;
}

/// `LdapVtable` → 核心 [`LdapBackend`]。
pub struct FfiLdapBackend {
    vtable: &'static oj_plugin_ffi::LdapVtable,
    config: LdapConfig,
}

impl FfiLdapBackend {
    pub fn new(vtable: &'static oj_plugin_ffi::LdapVtable, config: LdapConfig) -> Self {
        Self { vtable, config }
    }
}

#[async_trait]
impl LdapBackend for FfiLdapBackend {
    fn config(&self) -> &LdapConfig {
        &self.config
    }

    async fn call(&self, req: Value) -> BridgeResult<Value> {
        let s = serde_json::to_string(&req).map_err(|e| format!("ldap: req encode: {e}"))?;
        let fut = (self.vtable.call)(oj_plugin_ffi::RString::from(s.as_str()));
        // LDAP 往返毫秒~秒级：用 mail 同款退避轮询（2ms），不空转烧核。
        let bytes = super::ffi::await_ffi_poll(fut, super::ffi::FFI_POLL_BACKOFF)
            .await
            .map_err(|e| format!("ffi ldap: {e}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("ffi ldap: result decode: {e}").into())
    }
}

// ---------- 调用入参校验（权威层：JS 输入不可信，逐 op 白名单） ----------

fn need_str(o: &serde_json::Map<String, Value>, k: &str, op: &str) -> Result<String, String> {
    o.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("ldap.{op}: '{k}' must be a non-empty string"))
        .and_then(|s| {
            if s.is_empty() {
                Err(format!("ldap.{op}: '{k}' must not be empty"))
            } else {
                Ok(s)
            }
        })
}

/// 校验并重建一个调用 req。返回的 JSON 只含白名单字段（插件侧的解析仍各自再校验）。
fn validate_call(v: &Value, cfg: &LdapConfig) -> Result<Value, String> {
    let o = v.as_object().ok_or("ldap: request must be a JSON object")?;
    let op = o
        .get("op")
        .and_then(Value::as_str)
        .ok_or("ldap: 'op' must be a string (bind|search|search_paged|whoami|compare)")?;
    let key = match o.get("key") {
        None | Some(Value::Null) => "default".to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => return Err("ldap: 'key' must be a string".into()),
    };
    if !cfg.has(&key) {
        return Err(format!(
            "ldap: unknown instance '{key}'（known: {}）",
            cfg.instance_keys().join(", ")
        ));
    }
    let base_out = |extra: serde_json::Map<String, Value>| {
        let mut m = serde_json::Map::new();
        m.insert("op".into(), Value::String(op.to_string()));
        m.insert("key".into(), Value::String(key.clone()));
        m.extend(extra);
        Value::Object(m)
    };
    match op {
        "bind" => {
            let dn = need_str(o, "dn", op)?;
            let pw = match o.get("pw") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err("ldap.bind: 'pw' must be a string".into()),
            };
            Ok(base_out(serde_json::Map::from_iter([
                ("dn".into(), Value::String(dn)),
                ("pw".into(), Value::String(pw)),
            ])))
        }
        "search" | "search_paged" => {
            let base = need_str(o, "base", op)?;
            let scope = match o.get("scope") {
                None | Some(Value::Null) => "sub".to_string(),
                Some(Value::String(s)) if matches!(s.as_str(), "base" | "one" | "sub") => s.clone(),
                Some(Value::String(s)) => {
                    return Err(format!(
                        "ldap.{op}: scope must be 'base'|'one'|'sub' (got '{s}')"
                    ));
                }
                _ => return Err(format!("ldap.{op}: 'scope' must be a string")),
            };
            let filter = match o.get("filter") {
                None | Some(Value::Null) => "(objectClass=*)".to_string(),
                Some(Value::String(s)) => s.clone(),
                _ => return Err(format!("ldap.{op}: 'filter' must be a string")),
            };
            let attrs = match o.get("attrs") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| format!("ldap.{op}: 'attrs' must be string[]"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err(format!("ldap.{op}: 'attrs' must be an array")),
            };
            let mut m = serde_json::Map::from_iter([
                ("base".into(), Value::String(base)),
                ("scope".into(), Value::String(scope)),
                ("filter".into(), Value::String(filter)),
                (
                    "attrs".into(),
                    Value::Array(attrs.into_iter().map(Value::String).collect()),
                ),
            ]);
            if op == "search_paged" {
                let ps = match o.get("page_size") {
                    None | Some(Value::Null) => 500u64,
                    Some(Value::Number(n)) => n
                        .as_u64()
                        .filter(|n| (1..=10_000).contains(n))
                        .ok_or_else(|| {
                            "ldap.search_paged: page_size must be 1..=10000".to_string()
                        })?,
                    _ => return Err("ldap.search_paged: 'page_size' must be a number".into()),
                };
                m.insert("page_size".into(), json!(ps));
            }
            Ok(base_out(m))
        }
        "whoami" => Ok(base_out(serde_json::Map::new())),
        "compare" => {
            let dn = need_str(o, "dn", op)?;
            let attr = need_str(o, "attr", op)?;
            let val = match o.get("val") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err("ldap.compare: 'val' must be a string".into()),
            };
            Ok(base_out(serde_json::Map::from_iter([
                ("dn".into(), Value::String(dn)),
                ("attr".into(), Value::String(attr)),
                ("val".into(), Value::String(val)),
            ])))
        }
        _ => Err(format!(
            "ldap: unknown op '{op}'（known: bind|search|search_paged|whoami|compare）"
        )),
    }
}

fn ldap_backend(state: &OpState) -> Result<Arc<dyn LdapBackend>, JsErrorBox> {
    let st = state.borrow::<Arc<super::StableState>>();
    st.ldap.clone().ok_or_else(|| {
        JsErrorBox::generic(
            "ldap not configured (config ldap: section missing, or oj-ldap plugin not loaded)",
        )
    })
}

/// ldap.*：统一调用入口（bootstrap 装配 5 个方法，全部经此 op）。
/// `req` = 调用 JSON。未配置 → 抛；校验/协议/网络错 → 抛；成功 → op 结果 JSON。
#[op2]
#[serde]
pub async fn op_ldap_call(
    state: Rc<RefCell<OpState>>,
    #[string] req_json: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let backend = {
        let g = state.borrow();
        ldap_backend(&g)?
    };
    let v: Value = serde_json::from_str(&req_json)
        .map_err(|e| JsErrorBox::generic(format!("ldap: request is not valid JSON: {e}")))?;
    let req = validate_call(&v, backend.config()).map_err(JsErrorBox::generic)?;
    backend
        .call(req)
        .await
        .map_err(|e| JsErrorBox::generic(format!("ldap: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LdapConfig {
        LdapConfig::from_value(&json!({
            "default": { "url": "ldap://dc.example.com:389", "bind_dn": "cn=svc,dc=example,dc=com", "bind_pw": "s3cret", "timeout_ms": 3000 },
            "ad": { "url": "ldaps://ad.internal:636", "start_tls": false, "tls_skip_verify": true },
        }))
        .unwrap()
    }

    #[test]
    fn config_whitelist_and_url_gate() {
        let c = cfg();
        assert!(c.has("default") && c.has("ad"));
        assert_eq!(
            c.instance_keys(),
            vec!["ad".to_string(), "default".to_string()]
        );
        // 未知键 → Err（拼错的配置不静默忽略）
        assert!(
            LdapConfig::from_value(&json!({"default": {"url": "ldap://h", "passsword": "x"}}))
                .is_err()
        );
        // url 缺失 / 非 ldap scheme → Err
        assert!(LdapConfig::from_value(&json!({"default": {"bind_dn": "x"}})).is_err());
        assert!(LdapConfig::from_value(&json!({"default": {"url": "http://h"}})).is_err());
        // 类型错误 → Err
        assert!(
            LdapConfig::from_value(&json!({"default": {"url": "ldap://h", "timeout_ms": "3s"}}))
                .is_err()
        );
        assert!(
            LdapConfig::from_value(&json!({"default": {"url": "ldap://h", "start_tls": "yes"}}))
                .is_err()
        );
    }

    #[test]
    fn validate_call_binds_and_searches() {
        let c = cfg();
        // bind：缺 dn / pw → Err；全量 → 透传
        assert!(validate_call(&json!({"op": "bind", "dn": ""}), &c).is_err());
        let v = validate_call(&json!({"op": "bind", "dn": "uid=eve", "pw": "x"}), &c).unwrap();
        assert_eq!(v["op"], "bind");
        assert_eq!(v["key"], "default");
        assert_eq!(v["dn"], "uid=eve");
        // search：默认 scope/filter/attrs；scope 枚举门禁
        let v = validate_call(
            &json!({"op": "search", "base": "ou=users,dc=example,dc=com"}),
            &c,
        )
        .unwrap();
        assert_eq!(v["scope"], "sub");
        assert_eq!(v["filter"], "(objectClass=*)");
        assert_eq!(v["attrs"], json!([]));
        assert!(
            validate_call(
                &json!({"op": "search", "base": "dc=x", "scope": "tree"}),
                &c
            )
            .is_err()
        );
        assert!(
            validate_call(
                &json!({"op": "search", "base": "dc=x", "attrs": ["uid", 1]}),
                &c
            )
            .is_err()
        );
        // search_paged：page_size 门禁
        assert!(
            validate_call(
                &json!({"op": "search_paged", "base": "dc=x", "page_size": 0}),
                &c
            )
            .is_err()
        );
        let v = validate_call(
            &json!({"op": "search_paged", "base": "dc=x", "page_size": 250}),
            &c,
        )
        .unwrap();
        assert_eq!(v["page_size"], 250);
        // compare / whoami
        assert!(validate_call(&json!({"op": "compare", "dn": "x", "attr": "uid"}), &c).is_err());
        assert!(validate_call(&json!({"op": "whoami", "key": "ad"}), &c).is_ok());
        // 未知实例 / 未知 op
        assert!(validate_call(&json!({"op": "whoami", "key": "nope"}), &c).is_err());
        assert!(validate_call(&json!({"op": "delete"}), &c).is_err());
    }

    /// 假后端：验证 op 层（校验→转发→错误收敛）与「未配置」语义。
    struct FakeLdap(Arc<std::sync::Mutex<Vec<Value>>>);
    #[async_trait]
    impl LdapBackend for FakeLdap {
        fn config(&self) -> &LdapConfig {
            static C: std::sync::OnceLock<LdapConfig> = std::sync::OnceLock::new();
            C.get_or_init(cfg)
        }

        async fn call(&self, req: Value) -> BridgeResult<Value> {
            self.0.lock().unwrap().push(req.clone());
            match req["op"].as_str() {
                Some("bind") => Ok(Value::Bool(true)),
                Some("search") => {
                    Ok(json!([{"dn": "uid=eve", "attrs": {"uid": ["eve"]}, "bin": {}}]))
                }
                Some("whoami") => Ok(Value::String("dn:cn=svc".into())),
                _ => Ok(Value::Bool(false)),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn op_layer_validates_and_dispatches() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let b = crate::bridge::Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(crate::bridge::InMemoryKV::new()),
            crate::bridge::SchemaRegistry::new(),
            false,
            None,
            crate::bridge::Extras {
                ldap: Some(Arc::new(FakeLdap(calls.clone()))),
                ..Default::default()
            },
        );
        let cap = b
            .run_with(
                r#"(async () => {
                    const ok = await ldap.bind("uid=eve,dc=example,dc=com", "pw");
                    const rows = await ldap.search("ou=users,dc=example,dc=com", { scope: "one", attrs: ["uid"] });
                    const me = await new LDAP("ad").whoami();
                    json.ok({ ok, n: rows.length, me });
                })().catch((e) => json.fail(500, String(e)));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"], json!({"ok": true, "n": 1, "me": "dn:cn=svc"}));
        let got: Vec<Value> = calls.lock().unwrap().clone();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0]["op"], "bind");
        assert_eq!(got[1]["scope"], "one");
        assert_eq!(got[2]["key"], "ad");
        // 校验失败 → reject（fail 500 捕获）
        let cap = b
            .run_with(
                r#"ldap.search("dc=x", { scope: "bad" }).then(() => json.fail(500, "no"))
                    .catch((e) => json.ok({ err: String(e) }));"#,
                crate::bridge::RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("scope must be"),
            "{v}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ldap_not_configured_throws() {
        let b = crate::bridge::Bridge::new(
            Arc::new(crate::bridge::InMemoryAccessor::new()),
            Arc::new(crate::bridge::InMemoryKV::new()),
        );
        let e = b
            .run_with(r#"ldap.whoami();"#, crate::bridge::RequestInfo::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("ldap not configured"), "{e}");
    }
}
