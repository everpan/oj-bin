//! U-15 管理面 connector 轴（M1 宿主面，v0.1.100+）。
//!
//! # 为什么不进 `axis()` 公开面
//!
//! 管理操作（KILL / 配置写 / 集群变更）是特权面。connector 占用的泛型轴在**装配期收编**
//! （`mgmt:` 段 `axes` 列表）——`serve_cmd` 不把它们注册进 `GenericAxisRegistry`，而是放进
//! [`StableState.mgmt_axes`]；`axis()` 因此不可达（特权旁路封堵，评审阻断①）。
//!
//! # 宿主侧约定（架在泛型传输之上的管理面契约）
//!
//! connector 的 `GenericVtable.call` 必须实现三个保留 op（与 db 插件的 vtable 纪律同 philosophy：
//! 传输零 ABI，语义靠约定 + fail-loud）：
//! - `__ops__` → `[{"op":…,"readOnly":bool}, …]`（能力表；缺 op 即 `OJ_MGMT_UNSUPPORTED`）
//! - `__connect__`（args `{"dsn":…}`）→ `{"handle":…}`（登记时调用；返回句柄随每次业务调用注入）
//! - `__disconnect__`（args `{"handle":…}`）（删除时尽力而为）
//!
//! 业务调用：宿主把 `__handle` 注入 args 顶层（args 必须是 JSON object）。
//!
//! # 红线
//!
//! - **readOnly 是插件自报、宿主不可验证**（评审成文）：宿主据此分档判权，connector 须第一方。
//! - 审计 `MGMT_AUDIT`/`MGMT_DENY` 走 [`audit::emit`] 单一出口，**全字段零凭据**
//!   （连接串入库即 `secret::redact`，args 永不入审计）。
//! - 容量：`max_entries` / `max_per_owner` 锁内重判（U-7 同款）；per-connector 并发闸
//!   满即拒不排队（U-7 口径）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::{Value, json};

use super::StableState;
use super::audit;
use super::db_policy;
use super::generic_axis::call_axis;
use super::handle_core::Owner;

/// 每 connector 并发闸（宿主持有；满即拒不排队）。固定常量——真需要分连接器调优时再配置化。
const CALL_CONCURRENCY: usize = 4;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 一个已登记 connector 的运行期条目。
pub struct MgmtEntry {
    pub registered_by: Option<String>,
    pub registered_at_ms: u64,
    /// 脱敏连接串（`secret::redact` 后；list 透出、审计不入）。
    pub dsn_redacted: String,
    /// `__connect__` 返回的连接句柄（业务调用注入 args）。
    pub handle: Value,
    /// `__ops__` 能力表缓存：op → read_only（登记时探一次；宿主不可验证，见模块红线）。
    pub ops: HashMap<String, bool>,
}

/// 管理面注册表（壳 Arc 不可变、内部 Mutex 变动；进程级，热更存活）。
#[derive(Default)]
pub struct MgmtRegistry {
    inner: Mutex<HashMap<String, Arc<MgmtEntry>>>,
    max_entries: usize,
    max_per_owner: usize,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
}

impl MgmtRegistry {
    pub fn new(max_entries: usize, max_per_owner: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_entries,
            max_per_owner,
            gates: Mutex::new(HashMap::new()),
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        lock(&self.inner).contains_key(name)
    }

    pub fn entry(&self, name: &str) -> Option<Arc<MgmtEntry>> {
        lock(&self.inner).get(name).cloned()
    }

    pub fn registered_by_of(&self, name: &str) -> Option<String> {
        lock(&self.inner).get(name)?.registered_by.clone()
    }

    /// 登记（重名 / 容量 / per-owner 上限 → Err）。锁内重判（U-7 同款）。
    fn insert(&self, name: String, entry: MgmtEntry) -> Result<(), String> {
        let mut g = lock(&self.inner);
        if g.contains_key(&name) {
            return Err(format!("mgmt connector '{name}' already registered"));
        }
        if g.len() >= self.max_entries {
            return Err(format!(
                "mgmt registry full (mgmt.max_entries {})",
                self.max_entries
            ));
        }
        let owned = g
            .values()
            .filter(|e| e.registered_by == entry.registered_by)
            .count();
        if owned >= self.max_per_owner {
            return Err(format!(
                "mgmt registry per-owner cap {} reached (owner {:?}); delete a connector first",
                self.max_per_owner,
                entry.registered_by.as_deref().unwrap_or("-")
            ));
        }
        g.insert(name, Arc::new(entry));
        Ok(())
    }

    fn remove(&self, name: &str) -> Option<Arc<MgmtEntry>> {
        lock(&self.inner).remove(name)
    }

    /// 已登记 connector 名（list 用；快照）。
    fn names(&self) -> Vec<String> {
        lock(&self.inner).keys().cloned().collect()
    }

    /// per-connector 并发闸（满即 Err，不排队）。
    fn try_gate(&self, name: &str) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
        let sem = {
            let mut g = lock(&self.gates);
            g.entry(name.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(CALL_CONCURRENCY)))
                .clone()
        };
        sem.try_acquire_owned().map_err(|_| {
            format!(
                "mgmt connector '{name}' busy ({CALL_CONCURRENCY} concurrent calls); retry later"
            )
        })
    }
}

// ---------------------------------------------------------------------------
// 门与审计
// ---------------------------------------------------------------------------

/// 事件上下文（与 client_db::audit_ctx 同形态；独立实现避免轴间耦合）。
fn audit_ctx(state: &OpState) -> (Owner, Arc<StableState>) {
    let owner = super::db_session::owner_of(state);
    let st = state.borrow::<Arc<StableState>>().clone();
    (owner, st)
}

fn mgmt_emit(state: &OpState, kind: &str, connector: &str, detail: &str) {
    let (owner, st) = audit_ctx(state);
    audit::emit(&st.audit, kind, &owner, connector, detail);
}

/// 启用门 + 分档授权（每 op 重判）。`write` = 写档（`allow`）；`read` = 读档
/// （`allow_read` 缺省复用 `allow`——产品路：通常同主体）。
fn require_gate(state: &OpState, write: bool) -> Result<Arc<StableState>, JsErrorBox> {
    let st = state.borrow::<Arc<StableState>>().clone();
    let sec = &st.mgmt_section;
    if !sec.enabled {
        return Err(JsErrorBox::generic(
            "mgmt is disabled (set mgmt.enabled: true and declare mgmt.axes / mgmt.allow)",
        ));
    }
    let allow = if write {
        &sec.allow
    } else {
        sec.allow_read.as_ref().unwrap_or(&sec.allow)
    };
    let (module, roles, tenant, source) = db_policy::identity_of(state);
    let ctx = db_policy::AccessCtx {
        module: module.as_deref(),
        roles,
        tenant: tenant.as_deref(),
        source,
    };
    if !db_policy::granted(allow, &ctx) {
        let tier = if write { "write" } else { "read" };
        mgmt_emit(state, "MGMT_DENY", "-", &format!("action=gate tier={tier}"));
        return Err(JsErrorBox::generic(format!(
            "mgmt {tier} access not granted to this caller; ask admin to add this caller to mgmt.{}",
            if write { "allow" } else { "allow_read" }
        )));
    }
    Ok(st)
}

// ---------------------------------------------------------------------------
// 保留 op 调用（__ops__ / __connect__ / __disconnect__）
// ---------------------------------------------------------------------------

fn mgmt_axis<'a>(
    st: &'a StableState,
    name: &str,
) -> Result<&'a super::generic_axis::GenericAxisHandle, JsErrorBox> {
    st.mgmt_axes.get(name).ok_or_else(|| {
        JsErrorBox::generic(format!(
            "unknown mgmt connector '{name}' (not in mgmt.axes; see db-guide management chapter)"
        ))
    })
}

/// 读 `__ops__` 能力表（登记时探一次）。缺 op / 非 JSON / ping 未声明 → Err（fail-loud）。
async fn read_caps(
    vt: &'static oj_plugin_ffi::GenericVtable,
) -> Result<HashMap<String, bool>, String> {
    let v = call_axis(vt, "__ops__", "[]")
        .await
        .map_err(|e| format!("__ops__ probe failed: {e}"))?;
    let arr = v.as_array().ok_or("__ops__ must return an array")?;
    let mut ops = HashMap::new();
    for it in arr {
        let op = it
            .get("op")
            .and_then(Value::as_str)
            .ok_or("__ops__ item missing 'op'")?;
        let ro = it.get("readOnly").and_then(Value::as_bool).unwrap_or(false);
        ops.insert(op.to_string(), ro);
    }
    if !ops.contains_key("ping") {
        return Err("mgmt connector must declare a read-only 'ping' op in __ops__".to_string());
    }
    Ok(ops)
}

// ---------------------------------------------------------------------------
// ops
// ---------------------------------------------------------------------------

/// 登记管理面 connector：`__ops__` 探能力 → `__connect__` 拿句柄 → `ping` 探活
/// （失败 = 拒绝，fail-loud）→ 入表（容量/per-owner 锁内重判）。
#[op2]
#[serde]
pub async fn op_mgmt_register(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] dsn: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let st = require_gate(&state.borrow(), true)?;
    let vt = mgmt_axis(&st, &name)?.vt;
    let ops = read_caps(vt).await.map_err(JsErrorBox::generic)?;
    let conn = call_axis(vt, "__connect__", &json!({ "dsn": dsn }).to_string())
        .await
        .map_err(|e| JsErrorBox::generic(format!("__connect__ failed: {e}")))?;
    let handle = conn.get("handle").cloned().ok_or_else(|| {
        JsErrorBox::generic("__connect__ must return {\"handle\": …}".to_string())
    })?;
    // 探活（fail-loud：登记时 ping 失败 = 拒绝，评审钉死）。
    let ping_args = json!({ "__handle": handle }).to_string();
    if let Err(e) = call_axis(vt, "ping", &ping_args).await {
        let _ = call_axis(
            vt,
            "__disconnect__",
            &json!({ "handle": handle }).to_string(),
        )
        .await;
        return Err(JsErrorBox::generic(format!(
            "mgmt connector '{name}' unreachable at register (ping failed): {e}"
        )));
    }
    let owner_str =
        db_policy::operator_of(state.borrow().borrow::<super::ReqState>().req.user.as_ref());
    let registry = &st.mgmt;
    registry
        .insert(
            name.clone(),
            MgmtEntry {
                registered_by: owner_str.clone(),
                registered_at_ms: now_ms(),
                dsn_redacted: crate::secret::redact(&dsn),
                handle,
                ops,
            },
        )
        .map_err(JsErrorBox::generic)?;
    mgmt_emit(&state.borrow(), "MGMT_AUDIT", &name, "action=register");
    let e = registry.entry(&name).unwrap();
    Ok(summary(&name, &e))
}

/// 删除：尽力 `__disconnect__` 后摘表（幂等：不存在返回 false）。
#[op2]
pub async fn op_mgmt_delete(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<bool, JsErrorBox> {
    let st = require_gate(&state.borrow(), true)?;
    let Some(e) = st.mgmt.remove(&name) else {
        return Ok(false);
    };
    if let Some(h) = st.mgmt_axes.get(&name) {
        let _ = call_axis(
            h.vt,
            "__disconnect__",
            &json!({ "handle": e.handle }).to_string(),
        )
        .await;
    }
    mgmt_emit(&state.borrow(), "MGMT_AUDIT", &name, "action=delete");
    Ok(true)
}

/// 清单（读档）：脱敏连接串 + ops 名单（readOnly 位如实透出）。
#[op2]
#[serde]
pub fn op_mgmt_list(state: &mut OpState) -> Result<Vec<serde_json::Value>, JsErrorBox> {
    let st = require_gate(state, false)?;
    let names = st.mgmt.names();
    Ok(names
        .iter()
        .filter_map(|n| st.mgmt.entry(n).map(|e| summary(n, &e)))
        .collect())
}

/// 探活（无特权语义——JS 侧据此返回 undefined；未启用恒 false）。
#[op2(fast)]
pub fn op_mgmt_has(state: &mut OpState, #[string] name: &str) -> bool {
    let st = state.borrow::<Arc<StableState>>();
    if !st.mgmt_section.enabled {
        return false;
    }
    st.mgmt.contains(name)
}

/// 管理调用：能力位分档 → 并发闸 → `__handle` 注入 → 审计。
#[op2]
#[serde]
pub async fn op_mgmt_call(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] op: String,
    #[string] args: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let st = {
        let s = state.borrow();
        s.borrow::<Arc<StableState>>().clone()
    };
    let Some(entry) = st.mgmt.entry(&name) else {
        return Err(JsErrorBox::generic(format!(
            "unknown mgmt connector '{name}' (not registered; use mgmt.register)"
        )));
    };
    let Some(read_only) = entry.ops.get(&op).copied() else {
        return Err(JsErrorBox::generic(format!(
            "op '{op}' is not declared by mgmt connector '{name}'"
        )));
    };
    let _ = require_gate(&state.borrow(), !read_only)?;
    let permit = st.mgmt.try_gate(&name).map_err(JsErrorBox::generic)?;
    let vt = mgmt_axis(&st, &name)?.vt;
    // `__handle` 注入：args 必须是 JSON object（管理调用一律带上下文）。
    let mut body: Value = serde_json::from_str(&args)
        .map_err(|e| JsErrorBox::generic(format!("mgmt args must be a JSON object: {e}")))?;
    if !body.is_object() {
        return Err(JsErrorBox::generic("mgmt args must be a JSON object"));
    }
    body["__handle"] = entry.handle.clone();
    let out = call_axis(vt, &op, &body.to_string()).await?;
    drop(permit);
    mgmt_emit(
        &state.borrow(),
        "MGMT_AUDIT",
        &name,
        &format!("action=call op={op} read_only={read_only}"),
    );
    Ok(out)
}

fn summary(name: &str, e: &MgmtEntry) -> Value {
    let mut ops: Vec<&String> = e.ops.keys().collect();
    ops.sort();
    json!({
        "connector": name,
        "dsn": e.dsn_redacted,
        "registeredBy": e.registered_by,
        "registeredAtMs": e.registered_at_ms,
        "ops": ops,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::db_policy::{AllowList, MgmtSection};
    use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use oj_plugin_ffi::{FfiFuture, RString};
    use serde_json::json;

    /// mini connector 的假 vtable（`__ops__`/`__connect__`/`ping`/`kill`；`boom` panic
    /// ——panic 收敛是插件侧义务，宿主只观察 RResult::Err）。
    fn fake_vt() -> &'static oj_plugin_ffi::GenericVtable {
        extern "C" fn call(op: RString, args: RString) -> FfiFuture {
            let op = op.to_string();
            match op.as_str() {
                "__ops__" => oj_plugin_ffi::ready_ok(
                    br#"[{"op":"ping","readOnly":true},{"op":"kill","readOnly":false}]"#.to_vec(),
                ),
                "__connect__" => oj_plugin_ffi::ready_ok(br#"{"handle":"h1"}"#.to_vec()),
                "__disconnect__" => oj_plugin_ffi::ready_ok(br#"{"ok":true}"#.to_vec()),
                "ping" => oj_plugin_ffi::ready_ok(br#"{"ok":true}"#.to_vec()),
                "kill" => oj_plugin_ffi::ready_ok(br#"{"killed":true}"#.to_vec()),
                "boom" => std::panic::panic_any("connector panic"),
                _ => oj_plugin_ffi::ready_err(format!("unknown op {op} {args:?}")),
            }
        }
        // 泛型轴 vtable 是 `&'static` 单槽结构（call）——进程内常量化。
        static VT: std::sync::OnceLock<oj_plugin_ffi::GenericVtable> = std::sync::OnceLock::new();
        VT.get_or_init(|| oj_plugin_ffi::GenericVtable { call })
    }

    fn section(enabled: bool) -> MgmtSection {
        MgmtSection {
            enabled,
            axes: vec!["fake".into()],
            allow: AllowList {
                roles: Some(vec!["admin".into()]),
                ..Default::default()
            },
            allow_read: Some(AllowList {
                roles: Some(vec!["viewer".into(), "admin".into()]),
                ..Default::default()
            }),
            max_entries: 8,
            max_per_owner: 4,
        }
    }

    fn bridge(enabled: bool) -> Bridge {
        Bridge::with_dbs_and_loader(
            std::collections::HashMap::from([(
                "default".to_string(),
                Arc::new(crate::bridge::db::InMemoryAccessor::new()) as _,
            )]),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                mgmt_section: Some(Arc::new(section(enabled))),
                mgmt_axes: Some(crate::bridge::generic_axis::registry_from(vec![(
                    "fake".to_string(),
                    "mini-mgmt".to_string(),
                    fake_vt(),
                )])),
                mgmt: Some(Arc::new(MgmtRegistry::new(8, 4))),
                ..Default::default()
            },
        )
    }

    async fn run_as(b: &Bridge, user: Option<Value>, src: &str) -> Value {
        let req = RequestInfo {
            user,
            ..RequestInfo::default()
        };
        let cap = b.run_with(src, req).await.unwrap();
        serde_json::from_slice(&cap.body).unwrap()
    }

    fn admin() -> Option<Value> {
        Some(json!({"id": "dba", "roles": ["admin"]}))
    }

    /// 未启用 → fail-closed deny（文案点名 mgmt.enabled）。
    #[tokio::test(flavor = "current_thread")]
    async fn disabled_surface_fails_closed() {
        let b = bridge(false);
        let v = run_as(
            &b,
            admin(),
            r#"mgmt.register("fake", "mongo://x").then((r) => json.ok(r))
               .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert!(
            v["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("mgmt is disabled"),
            "{v}"
        );
        // has 恒 false
        let v2 = run_as(&b, admin(), r#"json.ok({ has: mgmt.has("fake") })"#).await;
        assert_eq!(v2["data"]["has"], json!(false), "{v2}");
    }

    /// 主旅程：登记（探 __ops__/__connect__/ping）→ 只读调用（viewer）→ 写调用
    /// （admin，viewer 被拒）→ 未声明 op → 审计可检索 → **axis('fake') 不可达**。
    #[tokio::test(flavor = "current_thread")]
    async fn register_tiers_unsupported_and_isolation() {
        let b = bridge(true);
        let v = run_as(
            &b,
            admin(),
            r#"(async () => {
              const r = await mgmt.register("fake", "mongo://u:p@h/db");
              // 旁路封堵：公开面看不到 fake 轴
              let bypass = null;
              try { await axis("fake").ping(); } catch (e) { bypass = String(e); }
              // 读档：viewer 只读
              const ping1 = await mgmt.call("fake", "ping");
              // 未声明 op
              let unsup = null;
              try { await mgmt.call("fake", "nope"); } catch (e) { unsup = String(e); }
              // 审计
              const evs = await audit.query({ kind: "MGMT_AUDIT" });
              return json.ok({ ops: r.ops, bypass, ping: ping1, unsup,
                               audits: evs.length });
            })().catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["ops"], json!(["kill", "ping"]), "{v}");
        let bypass = v["data"]["bypass"].as_str().unwrap_or_default();
        assert!(bypass.contains("unknown generic axis"), "旁路必须封堵：{v}");
        let unsup = v["data"]["unsup"].as_str().unwrap_or_default();
        assert!(
            unsup.contains("not declared"),
            "未声明 op 应结构化拒绝：{v}"
        );
        assert!(
            v["data"]["audits"].as_u64().unwrap() >= 2,
            "register+call 应落审计：{v}"
        );
    }

    /// 分档：viewer 可只读、不可写；写档拒绝落 MGMT_DENY。
    #[tokio::test(flavor = "current_thread")]
    async fn read_write_tiers_gate_calls() {
        let b = bridge(true);
        // admin 先登记
        let v0 = run_as(
            &b,
            admin(),
            r#"mgmt.register("fake", "mongo://x").then((r) => json.ok(r.ops))
               .catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v0["code"], 0, "{v0}");
        let viewer = Some(json!({"id": "mon", "roles": ["viewer"]}));
        // viewer 只读 → 放行
        let v1 = run_as(
            &b,
            viewer.clone(),
            r#"mgmt.call("fake", "ping").then((r) => json.ok(r)).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert_eq!(v1["code"], 0, "viewer 只读应放行：{v1}");
        // viewer 写 → 拒（MGMT_DENY）
        let v2 = run_as(
            &b,
            viewer,
            r#"mgmt.call("fake", "kill").then((r) => json.ok(r)).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert!(
            v2["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("write access not granted"),
            "{v2}"
        );
        // 未声明 op → 结构化拒绝（管理员也一样）
        let v3 = run_as(
            &b,
            admin(),
            r#"mgmt.call("fake", "nope").then((r) => json.ok(r)).catch((e) => json.fail(500, String(e)))"#,
        )
        .await;
        assert!(
            v3["msg"]
                .as_str()
                .unwrap_or_default()
                .contains("not declared"),
            "{v3}"
        );
        // 审计里有 deny
        let v4 = run_as(
            &b,
            admin(),
            r#"audit.query({ kind: "MGMT_DENY" }).then((r) => json.ok({ n: r.length }))"#,
        )
        .await;
        assert!(v4["data"]["n"].as_u64().unwrap() >= 1, "{v4}");
    }
}
