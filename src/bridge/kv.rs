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
        let entry = g.entry(key.to_string()).or_insert_with(|| ("0".into(), None));
        let n: i64 = entry
            .0
            .parse()
            .map_err(|_| format!("kv.incr: {key} is not a number ({})", entry.0))?;
        let n = n + 1;
        entry.0 = n.to_string();
        Ok(n)
    }
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
        let b = Bridge::new(
            Arc::new(InMemoryAccessor::new()),
            kv as Arc<dyn KVStore>,
        );
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
        assert!(v["data"]["err"].as_str().unwrap().contains("not supported"), "{v}");

        let cap = b
            .run_with(
                r#"(async () => { await kv.incr("k"); json.ok({}); })().catch((e) => json.ok({ err: String(e) }));"#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"]["err"].as_str().unwrap().contains("not supported"), "{v}");
    }
}
