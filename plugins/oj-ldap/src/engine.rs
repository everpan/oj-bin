//! LDAP 操作引擎：op + args 解析 → 分派 → ldap3 执行 → JSON 编码 → FfiFuture 回传。
//!
//! 泛型轴协议（权威文档见 crate 根 `lib.rs`）：`args` 为位置参数 JSON 数组，
//! 末位可选 opts 对象 `{key, scope, filter, attrs, bindDn, bindPw, pageSize}`。
//! 连接模型：每调用独立 connect → （服务账号绑定）→ op → unbind。不做连接池
//! （见 lib.rs 模块文档）；用户鉴权 bind(dn,pw) 的凭据绝不落到共享连接上。

use std::collections::HashMap;

use base64::Engine as _;
use ldap3::controls::{Control, ControlType, PagedResults, RawControl};
use ldap3::exop::WhoAmI;
use ldap3::{Ldap, LdapConnAsync, LdapConnSettings, ResultEntry, Scope, SearchEntry, SearchResult};
use oj_plugin_ffi::{FfiFuture, ready_err, spawn_ffi_future};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

use crate::config::InstanceCfg;

pub struct Engine {
    rt: Runtime,
    instances: HashMap<String, InstanceCfg>,
}

/// 调用 opts（泛型通道末位可选对象；字段缺省值与原 typed req 语义一致）。
#[derive(Debug, Default)]
struct Opts {
    key: String,
    scope: Option<String>,
    filter: Option<String>,
    attrs: Option<Vec<String>>,
    bind_dn: Option<String>,
    bind_pw: Option<String>,
    page_size: Option<Value>,
}

impl Opts {
    fn new() -> Self {
        Self {
            key: "default".to_string(),
            ..Default::default()
        }
    }
}

impl Engine {
    pub fn new(cfg: crate::config::PluginCfg) -> Result<Self, String> {
        for (name, ic) in &cfg.instances {
            ic.validate()
                .map_err(|e| format!("ldap instance '{name}': {e}"))?;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            // worker 只跑 IO 转发（ldap 连接自管），2 足够；缺省 = num_cpus 全核白占线程。
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| format!("ldap: runtime: {e}"))?;
        Ok(Self {
            rt,
            instances: cfg.instances,
        })
    }

    /// 解析 op + args 并分派。解析失败/未知实例 → ready_err（同步）；业务 op → spawn future。
    /// 泛型通道无宿主校验层（旧 typed 路径的 host validate_call 职责收编到此处），
    /// 入参校验文案沿用旧宿主层文本，保持端到端用户可见语义不变。
    pub fn call(&self, op: &str, args: &str) -> FfiFuture {
        let arr: Vec<Value> = match serde_json::from_str(args) {
            Ok(Value::Array(a)) => a,
            Ok(_) => return ready_err("ldap: args must be a JSON array"),
            Err(e) => return ready_err(format!("ldap: args parse: {e}")),
        };
        // opts 恒在最后一位（缺省 = 空对象）；其前的位置参数个数按 op 固定。
        let opts_at = match op {
            "bind" => 2,
            "search" | "search_paged" => 1,
            "whoami" => 0,
            "compare" => 3,
            other => {
                return ready_err(format!(
                    "ldap: unknown op '{other}'（known: bind|search|search_paged|whoami|compare）"
                ));
            }
        };
        let opts = match opts_of(&arr, opts_at, op) {
            Ok(o) => o,
            Err(e) => return ready_err(e),
        };
        let Some(inst) = self.instances.get(&opts.key) else {
            return ready_err(format!(
                "ldap: unknown instance '{}'（known: {}）",
                opts.key,
                known_keys(&self.instances),
            ));
        };
        let inst = inst.clone();
        match op {
            "bind" => {
                let dn = match arg_str(&arr, 0, op, "dn", true) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                let pw = match arg_str(&arr, 1, op, "pw", false) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                spawn_ffi_future(&self.rt, async move { bind_op(&inst, &dn, &pw).await })
            }
            "search" | "search_paged" => {
                let base = match arg_str(&arr, 0, op, "base", true) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                let scope = match parse_scope(opts.scope.as_deref(), op) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                let filter = opts.filter.unwrap_or_else(|| "(objectClass=*)".to_string());
                let attrs = opts.attrs.unwrap_or_default();
                let bind = effective_bind(opts.bind_dn, opts.bind_pw, &inst);
                if op == "search" {
                    spawn_ffi_future(&self.rt, async move {
                        search_op(&inst, bind, &base, scope, &filter, &attrs).await
                    })
                } else {
                    let page_size = match page_size(&opts.page_size) {
                        Ok(n) => n,
                        Err(e) => return ready_err(e),
                    };
                    spawn_ffi_future(&self.rt, async move {
                        search_paged_op(&inst, bind, &base, scope, &filter, &attrs, page_size).await
                    })
                }
            }
            "whoami" => spawn_ffi_future(&self.rt, async move { whoami_op(&inst).await }),
            "compare" => {
                let dn = match arg_str(&arr, 0, op, "dn", true) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                let attr = match arg_str(&arr, 1, op, "attr", true) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                let val = match arg_str(&arr, 2, op, "val", false) {
                    Ok(s) => s,
                    Err(e) => return ready_err(e),
                };
                spawn_ffi_future(&self.rt, async move {
                    compare_op(&inst, &dn, &attr, &val).await
                })
            }
            _ => unreachable!("op 已在上方 match 门禁"),
        }
    }
}

/// 取位置参数：必须为字符串；`nonempty` 时空串视同缺失（对齐旧宿主 need_str）。
fn arg_str(
    arr: &[Value],
    i: usize,
    op: &str,
    name: &str,
    nonempty: bool,
) -> Result<String, String> {
    match arr.get(i) {
        Some(Value::String(s)) if !nonempty || !s.is_empty() => Ok(s.clone()),
        _ if nonempty => Err(format!("ldap.{op}: '{name}' must be a non-empty string")),
        _ => Err(format!("ldap.{op}: '{name}' must be a string")),
    }
}

/// 取末位 opts 对象：缺省/Null = 空 opts；非对象 → Err；未知键忽略
/// （对齐旧宿主 validate_call 的白名单重建语义）。
fn opts_of(arr: &[Value], at: usize, op: &str) -> Result<Opts, String> {
    let mut o = Opts::new();
    let Some(v) = arr.get(at) else {
        return Ok(o);
    };
    let Value::Object(m) = v else {
        return Err(format!("ldap.{op}: opts must be an object"));
    };
    for (k, v) in m {
        match (k.as_str(), v) {
            ("key", Value::String(s)) => o.key = s.clone(),
            ("key", _) => return Err(format!("ldap.{op}: 'key' must be a string")),
            ("scope", Value::String(s)) => o.scope = Some(s.clone()),
            ("scope", Value::Null) => {}
            ("scope", _) => return Err(format!("ldap.{op}: 'scope' must be a string")),
            ("filter", Value::String(s)) => o.filter = Some(s.clone()),
            ("filter", Value::Null) => {}
            ("filter", _) => return Err(format!("ldap.{op}: 'filter' must be a string")),
            ("attrs", Value::Array(a)) => {
                let mut vs = Vec::with_capacity(a.len());
                for x in a {
                    let Some(s) = x.as_str() else {
                        return Err(format!("ldap.{op}: 'attrs' must be string[]"));
                    };
                    vs.push(s.to_string());
                }
                o.attrs = Some(vs);
            }
            ("attrs", Value::Null) => {}
            ("attrs", _) => return Err(format!("ldap.{op}: 'attrs' must be string[]")),
            ("bindDn", Value::String(s)) => o.bind_dn = Some(s.clone()),
            ("bindDn", Value::Null) => {}
            ("bindDn", _) => return Err(format!("ldap.{op}: 'bindDn' must be a string")),
            ("bindPw", Value::String(s)) => o.bind_pw = Some(s.clone()),
            ("bindPw", Value::Null) => {}
            ("bindPw", _) => return Err(format!("ldap.{op}: 'bindPw' must be a string")),
            // pageSize 仅 search_paged 消费（与旧宿主一致：search 上携带即忽略）。
            ("pageSize", _) if op == "search_paged" => o.page_size = Some(v.clone()),
            _ => {} // 未知键 / search 上的 pageSize：忽略（白名单语义）
        }
    }
    Ok(o)
}

/// search_paged 的 page_size 门禁（旧宿主层职责收编）：缺省 500，范围 1..=10000。
fn page_size(v: &Option<Value>) -> Result<u64, String> {
    match v {
        None => Ok(500),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|n| (1..=10_000).contains(n))
            .ok_or_else(|| "ldap.search_paged: page_size must be 1..=10000".to_string()),
        Some(_) => Err("ldap.search_paged: 'page_size' must be a number".to_string()),
    }
}

fn known_keys(instances: &HashMap<String, InstanceCfg>) -> String {
    let mut ks: Vec<&str> = instances.keys().map(String::as_str).collect();
    ks.sort_unstable();
    ks.join(", ")
}

fn parse_scope(s: Option<&str>, op: &str) -> Result<Scope, String> {
    match s.unwrap_or("sub") {
        "base" => Ok(Scope::Base),
        "one" => Ok(Scope::OneLevel),
        "sub" => Ok(Scope::Subtree),
        other => Err(format!(
            "ldap.{op}: scope must be 'base'|'one'|'sub' (got '{other}')"
        )),
    }
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

/// 计算 search/searchPaged 的有效绑定凭据：opts 中的 bindDn/bindPw 与 config 服务账号
/// 合并（取一即可，另一回落 config）；二者皆无 → None（匿名绑定）。
fn effective_bind(
    bind_dn: Option<String>,
    bind_pw: Option<String>,
    cfg: &InstanceCfg,
) -> Option<(String, String)> {
    let dn = bind_dn.or_else(|| cfg.bind_dn.clone());
    let pw = bind_pw.or_else(|| cfg.bind_pw.clone());
    match (dn, pw) {
        (Some(dn), Some(pw)) => Some((dn, pw)),
        _ => None,
    }
}

/// 服务账号/显式凭据绑定（bind = Some((dn,pw)) 才发生）；rc != 0 → Err（fail-loud）。
/// bind = None → 匿名绑定（多数目录默认拒绝匿名读）。
async fn service_bind(ldap: &mut Ldap, bind: Option<(String, String)>) -> Result<(), String> {
    if let Some((dn, pw)) = bind {
        let res = ldap
            .simple_bind(&dn, &pw)
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
    bind: Option<(String, String)>,
    base: &str,
    scope: Scope,
    filter: &str,
    attrs: &[String],
) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    service_bind(&mut ldap, bind).await?;
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
    bind: Option<(String, String)>,
    base: &str,
    scope: Scope,
    filter: &str,
    attrs: &[String],
    page_size: u64,
) -> Result<Vec<u8>, String> {
    let mut ldap = connect(cfg).await?;
    service_bind(&mut ldap, bind).await?;
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
    let bind = cfg.bind_dn.clone().zip(cfg.bind_pw.clone());
    service_bind(&mut ldap, bind).await?;
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
    let bind = cfg.bind_dn.clone().zip(cfg.bind_pw.clone());
    service_bind(&mut ldap, bind).await?;
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

    /// 取 ready future 的错误文本（poll 一次即 -1 = Err 哨兵；take 一次后 free）。
    fn ready_err_text(f: FfiFuture) -> String {
        assert_eq!((f.poll)(f.state), -1, "expected ready error");
        let r = (f.take)(f.state);
        (f.free)(f.state);
        match std::result::Result::from(r) {
            Ok(_) => panic!("expected ready Err, got Ok"),
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    /// 参数/分派错误（解析期，无网络）→ ready_err，且文案与旧 typed 端到端语义一致。
    #[test]
    fn malformed_calls_are_ready_errors() {
        let e = engine();
        // 坏 JSON / 非数组
        assert!(ready_err_text(e.call("whoami", "not json")).contains("args parse"));
        assert!(ready_err_text(e.call("whoami", "{}")).contains("args must be a JSON array"));
        // 未知 op
        assert!(ready_err_text(e.call("delete", "[]")).contains("unknown op 'delete'"));
        // 未知实例（opts.key）
        assert!(
            ready_err_text(e.call("whoami", r##"[{"key":"nope"}]"##))
                .contains("unknown instance 'nope'")
        );
        // bind 缺 dn / pw
        assert!(
            ready_err_text(e.call("bind", r##"["uid=eve"]"##))
                .contains("ldap.bind: 'pw' must be a string")
        );
        assert!(
            ready_err_text(e.call("bind", r##"["uid=eve","pw","x"]"##))
                .contains("opts must be an object")
        );
        // search 缺 base / 坏 scope / 坏 attrs
        assert!(
            ready_err_text(e.call("search", "[]")).contains("'base' must be a non-empty string")
        );
        assert!(
            ready_err_text(e.call("search", r##"["dc=x",{"scope":"tree"}]"##))
                .contains("scope must be 'base'|'one'|'sub'")
        );
        assert!(
            ready_err_text(e.call("search", r##"["dc=x",{"attrs":["uid",1]}]"##))
                .contains("'attrs' must be string[]")
        );
        // search_paged 的 pageSize 门禁
        assert!(
            ready_err_text(e.call("search_paged", r##"["dc=x",{"pageSize":0}]"##))
                .contains("page_size must be 1..=10000")
        );
        // search 上的 pageSize 被忽略（旧宿主白名单语义）→ 落到实例检查/网络前不报错；
        // 位置参数正确时该调用不再是 ready 错误（已 spawn 网络 future）。
        let f = e.call("search", r##"["dc=x",{"pageSize":0}]"##);
        assert_ne!((f.poll)(f.state), -1, "search 忽略 pageSize");
        (f.free)(f.state);
    }

    /// opts 解析：缺省/键类型/未知键忽略/key 选实例。
    #[test]
    fn opts_parsing_semantics() {
        let arr: Vec<Value> = serde_json::from_str(
            r##"[{"key":"ad","scope":"one","filter":"(uid=e)","attrs":["uid"],"bindDn":"cn=x","bindPw":"p","pageSize":250,"unknown":1}]"##,
        )
        .unwrap();
        let o = opts_of(&arr, 0, "search_paged").unwrap();
        assert_eq!(o.key, "ad");
        assert_eq!(o.scope.as_deref(), Some("one"));
        assert_eq!(o.filter.as_deref(), Some("(uid=e)"));
        assert_eq!(o.attrs, Some(vec!["uid".to_string()]));
        assert_eq!(o.bind_dn.as_deref(), Some("cn=x"));
        assert_eq!(o.bind_pw.as_deref(), Some("p"));
        assert_eq!(page_size(&o.page_size).unwrap(), 250);
        // 缺省：key=default，其余 None；page_size 缺省 500。
        let o = opts_of(&[], 0, "search_paged").unwrap();
        assert_eq!(o.key, "default");
        assert!(o.scope.is_none() && o.filter.is_none() && o.attrs.is_none());
        assert_eq!(page_size(&o.page_size).unwrap(), 500);
        // Null 字段视同缺省。
        let arr: Vec<Value> = serde_json::from_str(r##"[{"scope":null,"bindDn":null}]"##).unwrap();
        let o = opts_of(&arr, 0, "search").unwrap();
        assert!(o.scope.is_none() && o.bind_dn.is_none());
        // search 忽略 pageSize。
        let arr: Vec<Value> = serde_json::from_str(r##"[{"pageSize":250}]"##).unwrap();
        let o = opts_of(&arr, 0, "search").unwrap();
        assert!(o.page_size.is_none());
        // 非对象 opts → Err。
        assert!(
            opts_of(
                &serde_json::from_str::<Vec<Value>>(r##"[42]"##).unwrap(),
                0,
                "whoami"
            )
            .is_err()
        );
    }

    /// effective_bind 合并语义：opts 与 config 取一合并；皆无 → None（匿名）。
    #[test]
    fn effective_bind_merges_opts_and_config() {
        let inst = InstanceCfg {
            url: "ldap://h:389".into(),
            bind_dn: Some("cn=cfg".into()),
            bind_pw: Some("pw-cfg".into()),
            timeout_ms: None,
            start_tls: None,
            tls_skip_verify: None,
        };
        // 全走 config
        let b = effective_bind(None, None, &inst);
        assert_eq!(b, Some(("cn=cfg".to_string(), "pw-cfg".to_string())));
        // opts 覆盖单侧，另一侧回落 config
        let b = effective_bind(Some("cn=o".into()), None, &inst);
        assert_eq!(b, Some(("cn=o".to_string(), "pw-cfg".to_string())));
        let b = effective_bind(None, Some("pw-o".into()), &inst);
        assert_eq!(b, Some(("cn=cfg".to_string(), "pw-o".to_string())));
        // config 无凭据、opts 单侧不全 → None
        let bare = InstanceCfg {
            bind_dn: None,
            bind_pw: None,
            ..inst.clone()
        };
        assert_eq!(effective_bind(None, None, &bare), None);
        assert_eq!(effective_bind(Some("cn=o".into()), None, &bare), None);
    }
}
