//! LDAP 操作引擎：req 解析 → op 分派 → ldap3 执行 → JSON 编码 → FfiFuture 回传。
//!
//! 连接模型：每调用独立 connect → （服务账号绑定）→ op → unbind。不做连接池
//! （见 lib.rs 模块文档）；用户鉴权 bind(dn,pw) 的凭据绝不落到共享连接上。

use std::collections::HashMap;

use base64::Engine as _;
use ldap3::controls::{Control, ControlType, PagedResults, RawControl};
use ldap3::exop::WhoAmI;
use ldap3::{Ldap, LdapConnAsync, LdapConnSettings, ResultEntry, Scope, SearchEntry, SearchResult};
use oj_plugin_ffi::{FfiFuture, ready_err, spawn_ffi_future};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::config::InstanceCfg;

pub struct Engine {
    rt: Runtime,
    instances: HashMap<String, InstanceCfg>,
}

/// 调用 req（宿主已校验；这里纵深防御——缺字段/坏形态一律 Err，不 panic）。
#[derive(Debug, Deserialize)]
struct Call {
    op: String,
    key: String,
    #[serde(default)]
    dn: Option<String>,
    #[serde(default)]
    pw: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    attrs: Option<Vec<String>>,
    #[serde(default)]
    page_size: Option<u64>,
    #[serde(default)]
    attr: Option<String>,
    #[serde(default)]
    val: Option<String>,
}

impl Engine {
    pub fn new(cfg: crate::config::PluginCfg) -> Result<Self, String> {
        for (name, ic) in &cfg.instances {
            ic.validate()
                .map_err(|e| format!("ldap instance '{name}': {e}"))?;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("ldap: runtime: {e}"))?;
        Ok(Self {
            rt,
            instances: cfg.instances,
        })
    }

    /// 解析 req 并分派。解析失败/未知实例 → ready_err（同步）；业务 op → spawn future。
    pub fn call(&self, req: &str) -> FfiFuture {
        let call: Call = match serde_json::from_str(req) {
            Ok(c) => c,
            Err(e) => return ready_err(format!("ldap: req parse: {e}")),
        };
        let Some(inst) = self.instances.get(&call.key) else {
            return ready_err(format!(
                "ldap: unknown instance '{}'（known: {}）",
                call.key,
                known_keys(&self.instances),
            ));
        };
        let inst = inst.clone();
        match call.op.as_str() {
            "bind" => {
                let (Some(dn), Some(pw)) = (call.dn, call.pw) else {
                    return ready_err("ldap.bind: dn/pw required");
                };
                spawn_ffi_future(&self.rt, async move { bind_op(&inst, &dn, &pw).await })
            }
            "search" => match search_args(&call, false) {
                Ok((base, scope, filter, attrs)) => spawn_ffi_future(&self.rt, async move {
                    search_op(&inst, &base, scope, &filter, &attrs).await
                }),
                Err(e) => ready_err(e),
            },
            "search_paged" => match search_args(&call, true) {
                Ok((base, scope, filter, attrs)) => {
                    let page_size = call.page_size.unwrap_or(500);
                    spawn_ffi_future(&self.rt, async move {
                        search_paged_op(&inst, &base, scope, &filter, &attrs, page_size).await
                    })
                }
                Err(e) => ready_err(e),
            },
            "whoami" => spawn_ffi_future(&self.rt, async move { whoami_op(&inst).await }),
            "compare" => {
                let (Some(dn), Some(attr), Some(val)) = (call.dn, call.attr, call.val) else {
                    return ready_err("ldap.compare: dn/attr/val required");
                };
                spawn_ffi_future(&self.rt, async move {
                    compare_op(&inst, &dn, &attr, &val).await
                })
            }
            other => ready_err(format!(
                "ldap: unknown op '{other}'（known: bind|search|search_paged|whoami|compare）"
            )),
        }
    }
}

fn known_keys(instances: &HashMap<String, InstanceCfg>) -> String {
    let mut ks: Vec<&str> = instances.keys().map(String::as_str).collect();
    ks.sort_unstable();
    ks.join(", ")
}

fn parse_scope(s: Option<&str>) -> Result<Scope, String> {
    match s.unwrap_or("sub") {
        "base" => Ok(Scope::Base),
        "one" => Ok(Scope::OneLevel),
        "sub" => Ok(Scope::Subtree),
        other => Err(format!(
            "ldap: scope must be 'base'|'one'|'sub' (got '{other}')"
        )),
    }
}

fn search_args(call: &Call, paged: bool) -> Result<(String, Scope, String, Vec<String>), String> {
    let op = if paged { "search_paged" } else { "search" };
    let base = call
        .base
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("ldap.{op}: base required"))?
        .to_string();
    let scope = parse_scope(call.scope.as_deref())?;
    let filter = call
        .filter
        .clone()
        .unwrap_or_else(|| "(objectClass=*)".to_string());
    let attrs = call.attrs.clone().unwrap_or_default();
    Ok((base, scope, filter, attrs))
}

/// connect + 应用连接/操作超时。连接元素（driver）spawn 到引擎 runtime。
async fn connect(cfg: &InstanceCfg) -> Result<Ldap, String> {
    let timeout = cfg.timeout();
    let settings = LdapConnSettings::new()
        .set_conn_timeout(timeout)
        .set_starttls(cfg.start_tls.unwrap_or(false))
        .set_no_tls_verify(cfg.tls_skip_verify.unwrap_or(false));
    let (conn, mut ldap) = LdapConnAsync::with_settings(settings, &cfg.url)
        .await
        .map_err(|e| format!("ldap: connect {}: {e}", cfg.url))?;
    tokio::spawn(conn.drive());
    ldap.with_timeout(timeout);
    Ok(ldap)
}

/// 服务账号绑定（配置了 bind_dn 才发生）；rc != 0 → Err（fail-loud）。
async fn service_bind(ldap: &mut Ldap, cfg: &InstanceCfg) -> Result<(), String> {
    if let (Some(dn), Some(pw)) = (&cfg.bind_dn, &cfg.bind_pw) {
        let res = ldap
            .simple_bind(dn, pw)
            .await
            .map_err(|e| format!("ldap: service bind: {e}"))?;
        if res.rc != 0 {
            return Err(format!(
                "ldap: service bind {dn}: rc={} {}",
                res.rc, res.text
            ));
        }
    }
    Ok(())
}

/// 鉴权绑定：rc == 0 → true；其余 rc（含 49 invalidCredentials）→ false。
/// 连接/协议错误才是 Err。
async fn bind_op(cfg: &InstanceCfg, dn: &str, pw: &str) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    let res = ldap
        .simple_bind(dn, pw)
        .await
        .map_err(|e| format!("ldap: bind: {e}"))?;
    let ok = res.rc == 0;
    let _ = ldap.unbind().await;
    serde_json::to_vec(&json!(ok)).map_err(|e| e.to_string())
}

async fn search_op(
    cfg: &InstanceCfg,
    base: &str,
    scope: Scope,
    filter: &str,
    attrs: &[String],
) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    service_bind(&mut ldap, cfg).await?;
    let res: SearchResult = ldap
        .search(base, scope, filter, attrs.to_vec())
        .await
        .map_err(|e| format!("ldap: search: {e}"))?;
    let (entries, _r) = res.success().map_err(|e| format!("ldap: search: {e}"))?;
    let _ = ldap.unbind().await;
    encode_entries(entries)
}

/// Paged 聚合：cookie 循环直到服务端清空（RFC 2696）。单页失败即整体 Err。
async fn search_paged_op(
    cfg: &InstanceCfg,
    base: &str,
    scope: Scope,
    filter: &str,
    attrs: &[String],
    page_size: u64,
) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    service_bind(&mut ldap, cfg).await?;
    let mut cookie = Vec::new();
    let mut all: Vec<ResultEntry> = Vec::new();
    loop {
        let pr = PagedResults {
            size: page_size as i32,
            cookie: cookie.clone(),
        };
        let ctrls: Vec<RawControl> = vec![pr.into()];
        let res: SearchResult = ldap
            .with_controls(ctrls)
            .search(base, scope, filter, attrs.to_vec())
            .await
            .map_err(|e| format!("ldap: search_paged: {e}"))?;
        let (entries, r) = res
            .success()
            .map_err(|e| format!("ldap: search_paged: {e}"))?;
        all.extend(entries);
        let next = r
            .ctrls
            .iter()
            .find_map(|c| match c {
                Control(Some(ControlType::PagedResults), raw) => {
                    Some(raw.parse::<PagedResults>().cookie)
                }
                _ => None,
            })
            .unwrap_or_default();
        if next.is_empty() {
            break;
        }
        cookie = next;
    }
    let _ = ldap.unbind().await;
    encode_entries(all)
}

async fn whoami_op(cfg: &InstanceCfg) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    service_bind(&mut ldap, cfg).await?;
    let res = ldap
        .extended(WhoAmI)
        .await
        .map_err(|e| format!("ldap: whoami: {e}"))?;
    let (exop, _r) = res.success().map_err(|e| format!("ldap: whoami: {e}"))?;
    let authzid = exop
        .val
        .as_deref()
        .and_then(|v| String::from_utf8(v.to_vec()).ok())
        .ok_or_else(|| "ldap: whoami: empty authzid".to_string())?;
    let _ = ldap.unbind().await;
    serde_json::to_vec(&json!(authzid)).map_err(|e| e.to_string())
}

async fn compare_op(cfg: &InstanceCfg, dn: &str, attr: &str, val: &str) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    service_bind(&mut ldap, cfg).await?;
    let equal = ldap
        .compare(dn, attr, val.as_bytes())
        .await
        .map_err(|e| format!("ldap: compare: {e}"))?
        .equal()
        .map_err(|e| format!("ldap: compare: {e}"))?;
    let _ = ldap.unbind().await;
    serde_json::to_vec(&json!(equal)).map_err(|e| e.to_string())
}

/// SearchEntry 列表 → JSON：`[{dn, attrs:{k:[v]}, bin:{k:[base64]}}]`。
/// `search()` 已把 referral 收进结果 refs（不自动跟随）；单条目的 BER 解析错误
/// （`construct` 会 panic）收敛为整体 Err，不跨界展开。
fn encode_entries(entries: Vec<ResultEntry>) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(entries.len());
    for re in entries {
        let se =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| SearchEntry::construct(re)))
                .map_err(|_| "ldap: entry parse failed (malformed server data)".to_string())?;
        let attrs: serde_json::Map<String, Value> = se
            .attrs
            .into_iter()
            .map(|(k, vs)| (k, Value::Array(vs.into_iter().map(Value::String).collect())))
            .collect();
        let bin: serde_json::Map<String, Value> = se
            .bin_attrs
            .into_iter()
            .map(|(k, vs)| {
                (
                    k,
                    Value::Array(
                        vs.into_iter()
                            .map(|b| {
                                Value::String(base64::engine::general_purpose::STANDARD.encode(b))
                            })
                            .collect(),
                    ),
                )
            })
            .collect();
        out.push(json!({ "dn": se.dn, "attrs": attrs, "bin": bin }));
    }
    serde_json::to_vec(&Value::Array(out)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(
            serde_json::from_str::<crate::config::PluginCfg>(
                r#"{"default":{"url":"ldap://dc.example.com:389","bind_dn":"cn=svc,dc=example,dc=com","bind_pw":"s"}}"#,
            )
            .unwrap(),
        )
        .unwrap()
    }

    /// req 解析失败/未知实例/未知 op/缺字段 → ready future（poll 一次即 -1）。
    #[test]
    fn malformed_calls_are_ready_errors() {
        let e = engine();
        // 坏 JSON
        let f = e.call("not json");
        assert_eq!((f.poll)(f.state), -1);
        (f.free)(f.state);
        // 未知实例
        let f = e.call(r#"{"op":"whoami","key":"nope"}"#);
        assert_eq!((f.poll)(f.state), -1);
        (f.free)(f.state);
        // 未知 op
        let f = e.call(r#"{"op":"delete","key":"default"}"#);
        assert_eq!((f.poll)(f.state), -1);
        (f.free)(f.state);
        // 缺 dn
        let f = e.call(r#"{"op":"bind","key":"default","pw":"x"}"#);
        assert_eq!((f.poll)(f.state), -1);
        (f.free)(f.state);
        // search 缺 base
        let f = e.call(r#"{"op":"search","key":"default"}"#);
        assert_eq!((f.poll)(f.state), -1);
        (f.free)(f.state);
    }

    #[test]
    fn search_args_defaults_and_scope_gate() {
        let call: Call = serde_json::from_str(
            r#"{"op":"search","key":"default","base":"dc=x","scope":"one","filter":"(uid=e)","attrs":["uid"]}"#,
        )
        .unwrap();
        let (base, scope, filter, attrs) = search_args(&call, false).unwrap();
        assert_eq!(base, "dc=x");
        assert_eq!(scope, Scope::OneLevel);
        assert_eq!(filter, "(uid=e)");
        assert_eq!(attrs, vec!["uid".to_string()]);
        let call: Call =
            serde_json::from_str(r#"{"op":"search","key":"default","base":"dc=x"}"#).unwrap();
        let (_, scope, filter, attrs) = search_args(&call, false).unwrap();
        assert_eq!(scope, Scope::Subtree);
        assert_eq!(filter, "(objectClass=*)");
        assert!(attrs.is_empty());
        let call: Call =
            serde_json::from_str(r#"{"op":"search","key":"default","base":"dc=x","scope":"tree"}"#)
                .unwrap();
        assert!(search_args(&call, false).is_err());
    }
}
