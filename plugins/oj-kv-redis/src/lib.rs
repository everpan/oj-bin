//! oj-kv-redis：kv 轴 redis cdylib 插件（Task 4.4；RedisKV 迁自 core kv.rs）。
//! 迁移决策同 db/blob/bus 插件（spec §3 插件自包含）：redis 驱动逻辑逐字复制自 core，
//! 行为与下线前的 RedisKV 对齐（ConnectionManager 复用连接；connect 单次探活 fail-fast）。
//! InMemoryKV 留 core 内置兜底——未声明 redis.default 即内存 KV，不进插件。
//!
//! cfg 契约：init cfg = `{}`；connect(cfg) 收 `{"url": "redis://..."}` JSON。
//! 句柄约定：connect 分配 handle（AtomicU64），close 释放。
//! 返回编码：get = JSON `Option<String>`；expire = JSON `bool`；incr = JSON `i64`；
//! set/del = 空。跨线时长以秒计（宿主侧已向上取整，Redis EXPIRE 只认整秒）。

use oj_plugin_ffi::{
    ABI_VERSION, FfiFuture, HostContext, KVStoreVtable, PluginDescriptor, RArc, RResult, RString,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 插件侧配置视图（= host 侧 `{"url": "..."}` JSON）。
#[derive(Deserialize)]
struct ConnectCfg {
    url: String,
}

/// 插件共享状态（进程级单例，init 建立）。
struct KVPluginState {
    rt: tokio::runtime::Runtime,
    stores: Mutex<HashMap<u64, Arc<RedisKV>>>,
    next_handle: AtomicU64,
}

static PLUGIN: OnceLock<KVPluginState> = OnceLock::new();

fn state() -> &'static KVPluginState {
    PLUGIN.get().expect("oj-kv-redis: init not called")
}

// ---- FfiFuture 桥（统一走 oj-plugin-ffi 的 catch_unwind 安全工厂：spawn_ffi_future / catch_future）----

// ---- redis 逻辑（迁自 core RedisKV，语义对齐）----

/// 真 Redis 驱动（ConnectionManager 复用连接；全命令落真 Redis）。
struct RedisKV {
    conn: redis::aio::ConnectionManager,
}

impl RedisKV {
    /// 连接失败/认证失败 → Err（装配 fail-fast；先单次连接探活——ConnectionManager
    /// 内部连不上会无限重试，直接构造会把「redis 宕机」变成启动挂死而非 fail-fast）。
    async fn arc(url: &str) -> Result<Arc<RedisKV>, String> {
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

    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        c.get(key).await.map_err(|e| format!("redis get: {e}"))
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), String> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        let _: () = c
            .set(key, value)
            .await
            .map_err(|e| -> String { format!("redis set: {e}") })?;
        Ok(())
    }

    async fn del(&self, key: &str) -> Result<(), String> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        let _: i64 = c
            .del(key)
            .await
            .map_err(|e| -> String { format!("redis del: {e}") })?;
        Ok(())
    }

    async fn expire(&self, key: &str, ttl_secs: u64) -> Result<bool, String> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        c.expire(key, ttl_secs as i64)
            .await
            .map_err(|e| format!("redis expire: {e}"))
    }

    async fn incr(&self, key: &str) -> Result<i64, String> {
        use redis::AsyncCommands;
        let mut c = self.conn.clone();
        c.incr(key, 1).await.map_err(|e| format!("redis incr: {e}"))
    }
}

impl KVPluginState {
    fn store(&self, handle: u64) -> Result<Arc<RedisKV>, String> {
        self.stores
            .lock()
            .unwrap()
            .get(&handle)
            .cloned()
            .ok_or_else(|| format!("kv: unknown handle {handle}"))
    }

    async fn do_get(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        let v = self.store(handle)?.get(key).await?;
        // JSON 编码 Option<String>：`"value"` 或 `null`（宿主 serde 解码）。
        serde_json::to_vec(&v).map_err(|e| format!("kv get encode: {e}"))
    }

    async fn do_set(&self, handle: u64, key: &str, value: &str) -> Result<Vec<u8>, String> {
        self.store(handle)?.set(key, value).await?;
        Ok(b"".to_vec())
    }

    async fn do_del(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        self.store(handle)?.del(key).await?;
        Ok(b"".to_vec())
    }

    async fn do_expire(&self, handle: u64, key: &str, ttl_secs: u64) -> Result<Vec<u8>, String> {
        let ok = self.store(handle)?.expire(key, ttl_secs).await?;
        serde_json::to_vec(&ok).map_err(|e| format!("kv expire encode: {e}"))
    }

    async fn do_incr(&self, handle: u64, key: &str) -> Result<Vec<u8>, String> {
        let n = self.store(handle)?.incr(key).await?;
        serde_json::to_vec(&n).map_err(|e| format!("kv incr encode: {e}"))
    }
}

// ---- vtable（同步签名返回 FfiFuture；connect 产 handle，close 释放）----

extern "C" fn connect(cfg: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            let cfg: ConnectCfg =
                serde_json::from_str(&cfg[..]).map_err(|e| format!("redis: bad cfg: {e}"))?;
            let kv = RedisKV::arc(&cfg.url).await?; // 探活 fail-fast（conn 不挂启动）
            let handle = st.next_handle.fetch_add(1, Ordering::SeqCst) + 1;
            st.stores.lock().unwrap().insert(handle, kv);
            Ok(format!(r#"{{"handle":{handle}}}"#).into_bytes())
        })
    })
}

extern "C" fn get(handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { st.do_get(handle, &key[..]).await })
    })
}

extern "C" fn set(handle: u64, key: RString, value: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_set(handle, &key[..], &value[..]).await
        })
    })
}

extern "C" fn del(handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { st.do_del(handle, &key[..]).await })
    })
}

extern "C" fn expire(handle: u64, key: RString, ttl_secs: u64) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move {
            st.do_expire(handle, &key[..], ttl_secs).await
        })
    })
}

extern "C" fn incr(handle: u64, key: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(|| {
        let st = state();
        oj_plugin_ffi::spawn_ffi_future(&st.rt, async move { st.do_incr(handle, &key[..]).await })
    })
}

extern "C" fn close(handle: u64) {
    oj_plugin_ffi::catch_void(|| {
        state().stores.lock().unwrap().remove(&handle);
    })
}

static VTABLE: KVStoreVtable = KVStoreVtable {
    connect,
    get,
    set,
    del,
    expire,
    incr,
    close,
};

// ---- 入口 ----

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("kv-redis"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("kv 轴 redis cdylib 插件：RedisKV 迁自 core kv.rs（Task 4.4）"),
    }
}

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    if PLUGIN.get().is_some() {
        return RResult::Ok(descriptor());
    }
    let _ = (&host, &cfg); // kv 插件 init 无装配期配置（连接 cfg 在 connect 传入）
    // get_or_init：并发 init 时闭包只跑一次（竞争方阻塞复用），不重复建 runtime，
    // 避免 `let _ = set(st)` 在竞争下把败者的 tokio Runtime 从 async 上下文 drop 崩溃。
    PLUGIN.get_or_init(|| KVPluginState {
        rt: runtime(),
        stores: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(0),
    });
    RResult::Ok(descriptor())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("oj-kv-redis tokio runtime")
}

oj_plugin_ffi::oj_plugin_entry!(init, kv => &VTABLE);

#[cfg(test)]
mod tests {
    use super::*;

    /// 连接拒绝 → Err（装配 fail-fast 路径；端口 1 无监听）。
    #[tokio::test(flavor = "current_thread")]
    async fn connect_refused_errors() {
        let desc = match std::result::Result::from(init(host(), RString::from("{}"))) {
            Ok(d) => d,
            Err(e) => panic!("init failed: {}", &e[..]),
        };
        assert_eq!(&desc.name[..], "kv-redis");
        let cfg = serde_json::json!({ "url": "redis://127.0.0.1:1/" }).to_string();
        let mut fut = connect(RString::from(cfg.as_str()));
        let r = drive(&mut fut).await;
        assert!(r.is_err(), "port 1 connect must fail fast: {r:?}");
    }

    /// 真 Redis roundtrip（env-gated）：`OJ_TEST_REDIS` 给 DSN（如 redis://127.0.0.1:6379/1）。
    /// 未设置 → 跳过（不进网络）。
    #[tokio::test(flavor = "multi_thread")]
    async fn real_redis_roundtrip_via_vtable() {
        let Ok(url) = std::env::var("OJ_TEST_REDIS") else {
            eprintln!("skip: OJ_TEST_REDIS unset");
            return;
        };
        let _ = std::result::Result::from(init(host(), RString::from("{}")));
        let cfg = serde_json::json!({ "url": url }).to_string();
        let bytes = drive(&mut connect(RString::from(cfg.as_str())))
            .await
            .expect("connect");
        let handle = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["handle"]
            .as_u64()
            .unwrap();

        let k = format!("oj-test/{}", std::process::id());
        drive(&mut set(
            handle,
            RString::from(k.as_str()),
            RString::from("v"),
        ))
        .await
        .expect("set");
        let got = drive(&mut get(handle, RString::from(k.as_str())))
            .await
            .expect("get");
        let v: Option<String> = serde_json::from_slice(&got).unwrap();
        assert_eq!(v.as_deref(), Some("v"));

        // INCR 于非数字值 → Err；置 1 后 incr → 2。
        assert!(
            drive(&mut incr(handle, RString::from(k.as_str())))
                .await
                .is_err()
        );
        drive(&mut set(
            handle,
            RString::from(k.as_str()),
            RString::from("1"),
        ))
        .await
        .expect("set1");
        let n = drive(&mut incr(handle, RString::from(k.as_str())))
            .await
            .expect("incr");
        assert_eq!(serde_json::from_slice::<i64>(&n).unwrap(), 2);

        // 整秒 TTL：EXPIRE 10 → true；del 后 get None。
        let b = drive(&mut expire(handle, RString::from(k.as_str()), 10))
            .await
            .expect("expire");
        assert!(serde_json::from_slice::<bool>(&b).unwrap());
        drive(&mut del(handle, RString::from(k.as_str())))
            .await
            .expect("del");
        let got = drive(&mut get(handle, RString::from(k.as_str())))
            .await
            .expect("get2");
        assert_eq!(
            serde_json::from_slice::<Option<String>>(&got).unwrap(),
            None
        );

        close(handle);
    }

    extern "C" fn test_log(_level: u8, _msg: RString) {}
    extern "C" fn test_deliver(_topic: RString, _payload: RString) {}

    fn host() -> RArc<HostContext> {
        RArc::new(HostContext {
            log: test_log,
            deliver: test_deliver,
        })
    }

    /// FfiFuture → 测试异步桥（等价 core await_ffi 的 poll 轮询）。
    async fn drive(fut: &mut FfiFuture) -> Result<Vec<u8>, String> {
        for _ in 0..100_000 {
            match (fut.poll)(fut.state) {
                0 => tokio::task::yield_now().await,
                code => {
                    let r = (fut.take)(fut.state);
                    (fut.free)(fut.state);
                    fut.state = std::ptr::null_mut();
                    return match (code, std::result::Result::from(r)) {
                        (1, Ok(b)) => Ok(b.iter().copied().collect()),
                        (_, Err(e)) => Err(e[..].to_string()),
                        _ => Err("ffi drive timeout".into()),
                    };
                }
            }
        }
        Err("ffi drive timeout".into())
    }
}
