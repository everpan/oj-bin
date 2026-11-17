//! KV 全局对象：内置内存 KV 抽象，作为未声明 redis.default 的兜底。
//! 真 Redis 已迁入 oj-kv-redis 插件（Task 4.4）；本模块只留 InMemoryKV 与 trait。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::RwLock;

use async_trait::async_trait;
use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde_json::Value;

use super::{BridgeResult, StableState};
use std::sync::Arc;

/// Redis 风格键值存储的统一契约（接口隔离）。M0 用内存实现，Phase 6 加真 RedisKV。
#[async_trait]
pub trait KVStore: Send + Sync {
    /// 读取键值，None 表示不存在。
    async fn get(&self, key: &str) -> BridgeResult<Option<String>>;
    /// 写入键值。
    async fn set(&self, key: &str, value: &str) -> BridgeResult<()>;
    /// 删除键（幂等：不存在为成功）。
    async fn del(&self, key: &str) -> BridgeResult<()>;
    /// 设置过期（相对 ttl 后读不到）；键不存在 → false。默认不支持。
    async fn expire(&self, key: &str, _ttl: std::time::Duration) -> BridgeResult<bool> {
        Err(format!("KVStore expire: not supported ({key})").into())
    }
    /// 原子自增并返回新值（缺失从 0 起；非数字 → Err）。默认不支持。
    async fn incr(&self, key: &str) -> BridgeResult<i64> {
        Err(format!("KVStore incr: not supported ({key})").into())
    }
    /// 原子比较写：`expected=None` 表「键不存在才写」（`value=None` 时为幂等 noop——
    /// 键不存在 → true 且不写）；`value=None` 表「匹配则删除」（解锁原语）。
    /// 值不匹配 → false 且不写。默认不支持。
    async fn cas(
        &self,
        key: &str,
        _expected: Option<&str>,
        _value: Option<&str>,
        _ttl: Option<std::time::Duration>,
    ) -> BridgeResult<bool> {
        Err(format!("KVStore cas: not supported ({key})").into())
    }
    /// 原子占位：键（存活视图中）不存在才写，返回是否写入。默认不支持。
    async fn setnx(
        &self,
        key: &str,
        _value: &str,
        _ttl: Option<std::time::Duration>,
    ) -> BridgeResult<bool> {
        Err(format!("KVStore setnx: not supported ({key})").into())
    }
    /// 原子换写：无条件覆盖并返回旧值（不存在 → None）。默认不支持。
    async fn getset(
        &self,
        key: &str,
        _value: &str,
        _ttl: Option<std::time::Duration>,
    ) -> BridgeResult<Option<String>> {
        Err(format!("KVStore getset: not supported ({key})").into())
    }
}

/// KVStore 的内存实现（fake，测试/演示用）。
/// 值存 `(value, Option<expires_at>)`，expires_at 用 `tokio::time::Instant`
/// （paused-clock 感知：`#[tokio::test(start_paused)]` + `advance` 可测）。
#[derive(Default)]
pub struct InMemoryKV {
    mu: RwLock<HashMap<String, (String, Option<tokio::time::Instant>)>>,
}

impl InMemoryKV {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl KVStore for InMemoryKV {
    async fn get(&self, key: &str) -> BridgeResult<Option<String>> {
        let now = tokio::time::Instant::now();
        let mut g = self.mu.write().unwrap();
        match g.get_mut(key) {
            // 惰性过期：到点即删并返回 None
            Some((_, Some(exp))) if *exp <= now => {
                g.remove(key);
                Ok(None)
            }
            Some((v, _)) => Ok(Some(v.clone())),
            None => Ok(None),
        }
    }

    async fn set(&self, key: &str, value: &str) -> BridgeResult<()> {
        self.mu
            .write()
            .unwrap()
            .insert(key.to_string(), (value.to_string(), None));
        Ok(())
    }

    async fn del(&self, key: &str) -> BridgeResult<()> {
        self.mu.write().unwrap().remove(key);
        Ok(())
    }

    async fn expire(&self, key: &str, ttl: std::time::Duration) -> BridgeResult<bool> {
        let mut g = self.mu.write().unwrap();
        if let Some(entry) = g.get_mut(key) {
            entry.1 = Some(tokio::time::Instant::now() + ttl);
            return Ok(true);
        }
        Ok(false)
    }

    async fn incr(&self, key: &str) -> BridgeResult<i64> {
        let mut g = self.mu.write().unwrap();
        let entry = g
            .entry(key.to_string())
            .or_insert_with(|| ("0".into(), None));
        let n: i64 = entry
            .0
            .parse()
            .map_err(|_| format!("kv.incr: {key} is not a number ({})", entry.0))?;
        let n = n + 1;
        entry.0 = n.to_string();
        Ok(n)
    }

    /// 原子原语共用形态：**先拿写锁、再取 `Instant::now()`**（锁外取 now + 锁上排队会以
    /// 陈旧 now 误判未过期 → extend/setnx 双持有者）；单把写锁内「查过期+比较+写」即原子，
    /// 多 runtime 池共享同一 Arc 无碍。存活判定 = 无 TTL 或未到点（到点即惰性清除）。
    async fn setnx(
        &self,
        key: &str,
        value: &str,
        ttl: Option<std::time::Duration>,
    ) -> BridgeResult<bool> {
        let mut g = self.mu.write().unwrap();
        let now = tokio::time::Instant::now();
        if live_entry(&mut g, key, now).is_some() {
            return Ok(false);
        }
        g.insert(key.to_string(), (value.to_string(), ttl.map(|d| now + d)));
        Ok(true)
    }

    async fn cas(
        &self,
        key: &str,
        expected: Option<&str>,
        value: Option<&str>,
        ttl: Option<std::time::Duration>,
    ) -> BridgeResult<bool> {
        let mut g = self.mu.write().unwrap();
        let now = tokio::time::Instant::now();
        // 存活视图（过期条目视作不存在，就地惰性清除）。
        let cur = live_entry(&mut g, key, now);
        let matched = match expected {
            None => cur.is_none(),
            Some(e) => cur.as_deref() == Some(e),
        };
        if !matched {
            return Ok(false);
        }
        match value {
            // 幂等 noop：键不存在即成功（expected=None + value=None）
            None if cur.is_none() => Ok(true),
            None => {
                g.remove(key);
                Ok(true)
            }
            Some(v) => {
                g.insert(key.to_string(), (v.to_string(), ttl.map(|d| now + d)));
                Ok(true)
            }
        }
    }

    async fn getset(
        &self,
        key: &str,
        value: &str,
        ttl: Option<std::time::Duration>,
    ) -> BridgeResult<Option<String>> {
        let mut g = self.mu.write().unwrap();
        let now = tokio::time::Instant::now();
        let old = live_entry(&mut g, key, now);
        g.insert(key.to_string(), (value.to_string(), ttl.map(|d| now + d)));
        Ok(old)
    }
}

/// 取键的存活值（无 TTL 或未到点）；过期条目就地惰性清除。调用方必须已持写锁，
/// `now` 必须取自同一临界区内（见 setnx 注释）。
fn live_entry(
    map: &mut HashMap<String, (String, Option<tokio::time::Instant>)>,
    key: &str,
    now: tokio::time::Instant,
) -> Option<String> {
    let live = match map.get(key) {
        Some((v, exp)) if exp.as_ref().is_none_or(|e| *e > now) => Some(v.clone()),
        _ => None,
    };
    if live.is_none() {
        map.remove(key);
    }
    live
}

/// redis.get(key)：Promise<string|null>，不存在为 null。
#[op2]
#[serde]
pub async fn op_kv_get(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<serde_json::Value, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    match kv.get(&key).await {
        Ok(Some(v)) => Ok(Value::String(v)),
        Ok(None) => Ok(Value::Null),
        Err(e) => Err(JsErrorBox::generic(e.to_string())),
    }
}

/// redis.set(key, value)：Promise<true>。
#[op2]
pub async fn op_kv_set(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
) -> Result<bool, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.set(&key, &value)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// kv.del(key)：Promise<true>。
#[op2]
pub async fn op_kv_del(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<bool, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.del(&key)
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    Ok(true)
}

/// kv.expire(key, ttlMs)：Promise<bool>（键不存在 → false）。
#[op2]
pub async fn op_kv_expire(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[number] ttl_ms: i64,
) -> Result<bool, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.expire(&key, std::time::Duration::from_millis(ttl_ms.max(0) as u64))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// kv.incr(key)：Promise<number>（缺失从 0 起自增；非数字 Err）。
#[op2]
pub async fn op_kv_incr(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
) -> Result<f64, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.incr(&key)
        .await
        .map(|n| n as f64)
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// JS 值（string | null）→ Option<String>：null =「不存在」语义（cas 的
/// expected/value 双向使用），非字符串非 null 响亮报错。
fn value_to_opt(v: &Value) -> Result<Option<String>, JsErrorBox> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        other => Err(JsErrorBox::generic(format!(
            "kv: string or null expected, got {other}"
        ))),
    }
}

/// ttlMs → Option<Duration>：负值 = 无 TTL（JS 面 undefined/null ttl 的约定）。
fn ttl_from_ms(ttl_ms: i64) -> Option<std::time::Duration> {
    (ttl_ms >= 0).then(|| std::time::Duration::from_millis(ttl_ms as u64))
}

/// kv.cas(key, expected|null, value|null, ttlMs)：Promise<boolean>（ABI 13 原子原语）。
#[op2]
pub async fn op_kv_cas(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[serde] expected: serde_json::Value,
    #[serde] value: serde_json::Value,
    #[number] ttl_ms: i64,
) -> Result<bool, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.cas(
        &key,
        value_to_opt(&expected)?.as_deref(),
        value_to_opt(&value)?.as_deref(),
        ttl_from_ms(ttl_ms),
    )
    .await
    .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// kv.setNX(key, value, ttlMs)：Promise<boolean>（键存在才写失败）。
#[op2]
pub async fn op_kv_setnx(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
    #[number] ttl_ms: i64,
) -> Result<bool, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    kv.setnx(&key, &value, ttl_from_ms(ttl_ms))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))
}

/// kv.getSet(key, value, ttlMs)：Promise<string|null>（原子换写，返回旧值）。
#[op2]
#[serde]
pub async fn op_kv_getset(
    state: Rc<RefCell<OpState>>,
    #[string] key: String,
    #[string] value: String,
    #[number] ttl_ms: i64,
) -> Result<serde_json::Value, JsErrorBox> {
    let kv = state.borrow().borrow::<Arc<StableState>>().kv.clone();
    match kv
        .getset(&key, &value, ttl_from_ms(ttl_ms))
        .await
        .map_err(|e| JsErrorBox::generic(e.to_string()))?
    {
        Some(v) => Ok(Value::String(v)),
        None => Ok(Value::Null),
    }
}

/// Redis EXPIRE 只接受整秒：毫秒 TTL 向上取整到 ≥1s。
/// 直接 `ttl.as_secs()` 会把 500ms → 0s → EXPIRE key 0 立即删键，
/// 与 InMemoryKV 的毫秒语义相悖（见 expire_secs 测试）。
pub(crate) fn expire_secs(ttl: std::time::Duration) -> i64 {
    (ttl.as_secs() + u64::from(ttl.subsec_nanos() != 0)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use serde_json::Value;

    use crate::bridge::{Bridge, InMemoryAccessor, RequestInfo};

    /// expire 惰性过期（未到 TTL 可读、到点后 get None、缺失键返回 false）；
    /// incr 从 0 起、连续自增、非数字 Err。start_paused + advance 免真实等待。
    #[tokio::test(start_paused = true)]
    async fn expire_and_incr() {
        let kv = InMemoryKV::new();
        // incr：缺失从 0 起、连续自增
        assert_eq!(kv.incr("b").await.unwrap(), 1);
        assert_eq!(kv.incr("b").await.unwrap(), 2);
        kv.set("a", "41").await.unwrap();
        assert_eq!(kv.incr("a").await.unwrap(), 42);
        kv.set("s", "not-a-number").await.unwrap();
        assert!(kv.incr("s").await.is_err());
        // expire：未到 TTL 可读 → 到点 get None；缺失键 false
        kv.set("t", "v").await.unwrap();
        assert!(kv.expire("t", Duration::from_secs(1)).await.unwrap());
        assert_eq!(kv.get("t").await.unwrap().as_deref(), Some("v"));
        tokio::time::advance(Duration::from_millis(1500)).await;
        assert_eq!(kv.get("t").await.unwrap(), None);
        assert!(!kv.expire("missing", Duration::from_secs(1)).await.unwrap());
        // expire 之后再 set 清掉 TTL（永不过期）
        kv.set("t", "v2").await.unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(kv.get("t").await.unwrap().as_deref(), Some("v2"));
    }

    /// Redis EXPIRE 只接受整秒：毫秒 TTL 必须向上取整到 ≥1s——
    /// 否则 500ms → 0s → EXPIRE key 0 立即删键（与 InMemoryKV 毫秒语义相悖）。
    #[test]
    fn expire_secs_ceil_never_zero_for_positive_ms() {
        assert_eq!(expire_secs(Duration::from_millis(500)), 1);
        assert_eq!(expire_secs(Duration::from_millis(1)), 1);
        assert_eq!(expire_secs(Duration::from_secs(1)), 1);
        assert_eq!(expire_secs(Duration::from_millis(1500)), 2);
        assert_eq!(expire_secs(Duration::from_secs(2)), 2);
        assert_eq!(expire_secs(Duration::ZERO), 0); // 0 保持 0（立即过期，语义一致）
    }

    // ---- cas / setnx / getSet（ABI 13）BDD 用例（INVEST：单行为、独立、可定位）----

    /// Given 空闲键 When setnx 占位 Then 写入成功；重复占位被拒且原值不变。
    #[tokio::test(start_paused = true)]
    async fn given_free_key_when_setnx_then_true_and_repetition_false() {
        let kv = InMemoryKV::new();
        assert!(kv.setnx("lk", "tokA", None).await.unwrap());
        assert!(!kv.setnx("lk", "tokB", None).await.unwrap());
        assert_eq!(kv.get("lk").await.unwrap().as_deref(), Some("tokA"));
    }

    /// Given 持 TTL 租约 When 租约到期后再 setnx Then 可重新占位（持有者崩溃释放语义）。
    #[tokio::test(start_paused = true)]
    async fn given_expired_lease_when_setnx_then_reacquired() {
        let kv = InMemoryKV::new();
        assert!(
            kv.setnx("lk", "tokA", Some(Duration::from_secs(1)))
                .await
                .unwrap()
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(kv.setnx("lk", "tokB", None).await.unwrap());
        assert_eq!(kv.get("lk").await.unwrap().as_deref(), Some("tokB"));
    }

    /// Given 持锁 token When 同值 cas 续租 Then 成功；错 token 续租被拒且原值不变。
    #[tokio::test(start_paused = true)]
    async fn given_matching_token_when_cas_extend_then_ok_and_wrong_token_rejected() {
        let kv = InMemoryKV::new();
        kv.set("lk", "tokB").await.unwrap();
        assert!(
            kv.cas(
                "lk",
                Some("tokB"),
                Some("tokB"),
                Some(Duration::from_secs(1))
            )
            .await
            .unwrap()
        );
        assert!(
            !kv.cas("lk", Some("tokA"), Some("tokA"), None)
                .await
                .unwrap()
        );
        assert_eq!(kv.get("lk").await.unwrap().as_deref(), Some("tokB"));
    }

    /// Given 持锁 token When value=None 的 cas Then 匹配删除（解锁）；错 token 不误删。
    #[tokio::test(start_paused = true)]
    async fn given_matching_token_when_cas_delete_then_unlocked_and_wrong_token_kept() {
        let kv = InMemoryKV::new();
        kv.set("lk", "tokB").await.unwrap();
        assert!(!kv.cas("lk", Some("tokA"), None, None).await.unwrap());
        assert_eq!(kv.get("lk").await.unwrap().as_deref(), Some("tokB"));
        assert!(kv.cas("lk", Some("tokB"), None, None).await.unwrap());
        assert_eq!(kv.get("lk").await.unwrap(), None);
    }

    /// Given 键不存在 When expected=None 的 cas Then true（幂等 noop）；
    /// 键存在后重试 Then false 且不写。
    #[tokio::test(start_paused = true)]
    async fn given_absent_key_when_cas_null_expected_then_noop_true_and_present_false() {
        let kv = InMemoryKV::new();
        assert!(kv.cas("k2", None, None, None).await.unwrap());
        assert!(kv.cas("k2", None, Some("v"), None).await.unwrap());
        assert!(!kv.cas("k2", None, None, None).await.unwrap());
        assert_eq!(kv.get("k2").await.unwrap().as_deref(), Some("v"));
    }

    /// Given 键不存在 When getSet Then 首写返回 None；再写返回旧值且覆盖。
    #[tokio::test(start_paused = true)]
    async fn given_absent_key_when_getset_then_none_then_previous_value() {
        let kv = InMemoryKV::new();
        assert_eq!(kv.getset("k3", "n1", None).await.unwrap(), None);
        assert_eq!(
            kv.getset("k3", "n2", None).await.unwrap().as_deref(),
            Some("n1")
        );
        assert_eq!(kv.get("k3").await.unwrap().as_deref(), Some("n2"));
    }

    /// Given 旧值已过 TTL When getSet Then 返回 None（读路径惰性过期）且新值写入。
    #[tokio::test(start_paused = true)]
    async fn given_expired_old_value_when_getset_then_none_and_new_written() {
        let kv = InMemoryKV::new();
        kv.set("k4", "v").await.unwrap();
        kv.expire("k4", Duration::from_secs(1)).await.unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(kv.getset("k4", "nv", None).await.unwrap(), None);
        assert_eq!(kv.get("k4").await.unwrap().as_deref(), Some("nv"));
    }

    /// Given InMemoryKV 后端 When JS 调 kv.setNX / kv.cas / kv.getSet Then 语义端到端成立。
    #[tokio::test(flavor = "current_thread")]
    async fn given_inmemory_backend_when_js_cas_setnx_getset_then_semantics_hold() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()) as Arc<dyn KVStore>,
        );
        let cap = b
            .run_with(
                r#"(async () => {
                    const a = await kv.setNX("lk", "t1", 10);
                    const b2 = await kv.setNX("lk", "t2", 10);
                    const c = await kv.cas("lk", "t1", "t2");
                    const d = await kv.getSet("lk", "t3");
                    const e = await kv.cas("lk", null, "fresh");
                    json.ok({ a, b2, c, d, e });
                })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["a"], true, "{v}");
        assert_eq!(v["data"]["b2"], false, "{v}");
        assert_eq!(v["data"]["c"], true, "{v}");
        assert_eq!(v["data"]["d"], "t2", "{v}");
        // cas(expected=null)：键已存活 → false（noop）
        assert_eq!(v["data"]["e"], false, "{v}");
    }

    /// Given InMemoryKV When JS kv.lock Then 获得/释放/重获语义成立：unlock 释放后可再锁；
    /// 已被持时无 retry 立刻 reject；带 retry 空转耗尽后 reject；extend 续租成功；
    /// 二次 unlock 返回 false（token 已失效）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_js_lock_when_acquire_release_retry_then_semantics_hold() {
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            Arc::new(InMemoryKV::new()) as Arc<dyn KVStore>,
        );
        let cap = b
            .run_with(
                r#"(async () => {
                    const l1 = await kv.lock("job", { ttlSec: 10 });
                    const u1 = await l1.unlock();
                    const l2 = await kv.lock("job");
                    let refused = false;
                    try { await kv.lock("job"); } catch (e) { refused = true; }
                    const e1 = await l2.extend();
                    const u2 = await l2.unlock();
                    const u2b = await l2.unlock();
                    // 重持 job，再对已持键 retry → 空转耗尽后 reject；释放后 retry 可获得。
                    const h = await kv.lock("job");
                    let retryExhausted = false;
                    try {
                        await kv.lock("job", { retry: { tries: 2, delayMs: 1 } });
                    } catch (e) { retryExhausted = true; }
                    const hu = await h.unlock();
                    const l3 = await kv.lock("job", { ttlSec: 10, retry: { tries: 2, delayMs: 1 } });
                    const u3 = await l3.unlock();
                    json.ok({ u1, refused, e1, u2, u2b, retryExhausted, hu, u3 });
                })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["u1"], true, "{v}");
        assert_eq!(v["data"]["refused"], true, "{v}");
        assert_eq!(v["data"]["e1"], true, "{v}");
        assert_eq!(v["data"]["u2"], true, "{v}");
        assert_eq!(v["data"]["u2b"], false, "{v}");
        assert_eq!(v["data"]["retryExhausted"], true, "{v}");
        assert_eq!(v["data"]["u3"], true, "{v}");
    }

    /// 默认 trait 方法（expire/incr 未覆写）→ Err；经 Bridge 验证调用路径。
    struct PlainKV(std::sync::RwLock<std::collections::HashMap<String, String>>);
    #[async_trait]
    impl KVStore for PlainKV {
        async fn get(&self, key: &str) -> BridgeResult<Option<String>> {
            Ok(self.0.read().unwrap().get(key).cloned())
        }
        async fn set(&self, key: &str, value: &str) -> BridgeResult<()> {
            self.0.write().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        async fn del(&self, key: &str) -> BridgeResult<()> {
            self.0.write().unwrap().remove(key);
            Ok(())
        }
    }

    /// get 直接报错的后端，用于覆盖 op_kv_get 的错误分支（line 112）。
    struct ErrKV;
    #[async_trait]
    impl KVStore for ErrKV {
        async fn get(&self, _key: &str) -> BridgeResult<Option<String>> {
            Err("kv boom".into())
        }
        async fn set(&self, _key: &str, _value: &str) -> BridgeResult<()> {
            Ok(())
        }
        async fn del(&self, _key: &str) -> BridgeResult<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_error_propagates_to_op() {
        let b = Bridge::new(Arc::new(InMemoryAccessor::new()), Arc::new(ErrKV));
        let cap = b
            .run_with(
                r#"(async () => { await redis.get("k"); json.ok({}); })().catch((e) => json.fail(500, String(e)));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["code"], 500, "{v}");
        assert!(v["msg"].as_str().unwrap().contains("kv boom"), "{v}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_expire_incr_unsupported() {
        use crate::bridge::Bridge;
        let kv = Arc::new(PlainKV(Default::default()));
        let b = Bridge::new(Arc::new(InMemoryAccessor::new()), kv as Arc<dyn KVStore>);
        let cap = b
            .run_with(
                r#"(async () => {
                    await kv.expire("k", 1);
                    json.ok({});
                })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("not supported"),
            "{v}"
        );

        let cap = b
            .run_with(
                r#"(async () => { await kv.incr("k"); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(
            v["data"]["err"].as_str().unwrap().contains("not supported"),
            "{v}"
        );
    }
}
