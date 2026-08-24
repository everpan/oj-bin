//! redis 全局对象：M0 内置的内存 KV 抽象（移植自 Go kv.go）。
//! 真实 Redis 命名实例 Redis(name) 待 server 层接入 redis crate 时再移植。

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
fn expire_secs(ttl: std::time::Duration) -> i64 {
    (ttl.as_secs() + u64::from(ttl.subsec_nanos() != 0)) as i64
}

/// 真 Redis 驱动（redis 0.27 ConnectionManager 复用连接；全命令落真 Redis）。
pub struct RedisKV {
    conn: redis::aio::ConnectionManager,
}

impl RedisKV {
    /// 连接失败/认证失败 → Err（装配 fail-fast；与 InMemoryKV 同接口可替换）。
    /// 先单次连接探活（ECONNREFUSED 立即失败）——ConnectionManager 内部连不上会无限重试，
    /// 直接构造会把「redis 宕机」变成启动挂死而非 fail-fast。
    pub async fn arc(url: &str) -> BridgeResult<Arc<RedisKV>> {
        let client = redis::Client::open(url).map_err(|e| format!("redis open {url}: {e}"))?;
        let _probe = client
            .get_multiplexed_tokio_connection()
            .await
            .map_err(|e| format!("redis connect {url}: {e}"))?;
        let conn = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| format!("redis connect {url}: {e}"))?;
        Ok(Arc::new(Self { conn }))
    }
}

#[async_trait]
impl KVStore for RedisKV {
    async fn get(&self, key: &str) -> BridgeResult<Option<String>> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        c.get(key).await.map_err(|e| format!("redis get: {e}").into())
    }

    async fn set(&self, key: &str, value: &str) -> BridgeResult<()> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        let _: () = c
            .set(key, value)
            .await
            .map_err(|e| -> String { format!("redis set: {e}") })?;
        Ok(())
    }

    async fn del(&self, key: &str) -> BridgeResult<()> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        let _: i64 = c.del(key).await.map_err(|e| -> String { format!("redis del: {e}") })?;
        Ok(())
    }

    async fn expire(&self, key: &str, ttl: std::time::Duration) -> BridgeResult<bool> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        c.expire(key, expire_secs(ttl)).await.map_err(|e| format!("redis expire: {e}").into())
    }

    async fn incr(&self, key: &str) -> BridgeResult<i64> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        c.incr(key, 1).await.map_err(|e| format!("redis incr: {e}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    /// 连接拒绝 → Err（装配 fail-fast 路径；端口 1 无监听）。
    #[tokio::test(flavor = "current_thread")]
    async fn redis_connect_refused_errors() {
        assert!(RedisKV::arc("redis://127.0.0.1:1/").await.is_err());
    }

    /// 真 Redis roundtrip：env `OJ_TEST_REDIS` 给 DSN（如 redis://127.0.0.1:6379/1）。
    /// 未设置 → 跳过。
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn redis_roundtrip() {
        let Ok(url) = std::env::var("OJ_TEST_REDIS") else {
            eprintln!("skip: OJ_TEST_REDIS unset");
            return;
        };
        let kv = RedisKV::arc(&url).await.unwrap();
        let k = format!("oj-test/{}", std::process::id());
        kv.set(&k, "v").await.unwrap();
        assert_eq!(kv.get(&k).await.unwrap().as_deref(), Some("v"));
        assert!(kv.incr(&k).await.is_err()); // INCR 于非数字值 → Err
        kv.set(&k, "1").await.unwrap();
        assert_eq!(kv.incr(&k).await.unwrap(), 2);
        assert!(kv.expire(&k, Duration::from_secs(10)).await.unwrap());
        kv.del(&k).await.unwrap();
        assert_eq!(kv.get(&k).await.unwrap(), None);
        // 毫秒 TTL 取整契约：500ms → EXPIRE 1s（键不立即消失；等 1.2s 后消失）。
        kv.set(&k, "v").await.unwrap();
        assert!(kv.expire(&k, Duration::from_millis(500)).await.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(kv.get(&k).await.unwrap().as_deref(), Some("v"), "500ms 不得被 EXPIRE 0 立即删键");
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert_eq!(kv.get(&k).await.unwrap(), None);
        kv.del(&k).await.unwrap();
    }
}
