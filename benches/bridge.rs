#![allow(clippy::doc_overindented_list_items)]
//!
//! 两层口径：
//!   - rust/*   —— 纯 Rust 层（信封序列化、trait 实现），无 JS 开销
//!   - js/*     —— JS op 全链路（JS 调用 → op → Rust → Promise 解析），
//!                每次迭代在 JS 循环里执行 N=100 次 op，结果除以 N 即单次 op 耗时
//!
//! 运行：cargo bench

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use only_js::bridge::generic_axis::registry_from;
use only_js::bridge::ldap::{FfiLdapBackend, LdapConfig};
use only_js::bridge::{
    Bridge, DataAccessor, Extras, InMemoryAccessor, InMemoryKV, KVStore, RequestInfo,
    SchemaRegistry,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

/// 每次迭代 JS 循环内的 op 调用次数（fetch 除外）。
const N: usize = 100;

/// ldap 通道对比的每次迭代调用次数（任务书要求 N=1000 级）。
const LDAP_N: usize = 1000;

// ---- ldap typed vs generic 通道对比：mock vtable 做最小工作（立即 ready），----
// ---- 只测通道纯开销（JS 面 → op → 校验/序列化 → FFI future → JSON 回程）。----

extern "C" fn mock_ldap_call(_req: oj_plugin_ffi::RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_ok(br#"true"#.to_vec())
}
static MOCK_LDAP_VT: oj_plugin_ffi::LdapVtable = oj_plugin_ffi::LdapVtable {
    call: mock_ldap_call,
};

extern "C" fn mock_axis_call(
    _op: oj_plugin_ffi::RString,
    _args: oj_plugin_ffi::RString,
) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_ok(br#"true"#.to_vec())
}
static MOCK_AXIS_VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable {
    call: mock_axis_call,
};

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn new_bridge() -> Bridge {
    let db = Arc::new(InMemoryAccessor::new());
    db.seed([json!({"id": 1, "name": "ever"})]);
    Bridge::new(db, Arc::new(InMemoryKV::new()))
}

/// 默认请求上下文（per-request 状态随 run_with 注入）。
fn req() -> RequestInfo {
    RequestInfo {
        method: "GET".into(),
        ..Default::default()
    }
}

/// 纯 Rust 层：信封序列化与 trait 实现。
fn bench_rust(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("rust");
    group.throughput(Throughput::Elements(1));

    let data = json!({"user": {"id": 1, "name": "ever"}, "tags": ["a", "b"]});
    group.bench_function("envelope.ok", |b| b.iter(|| only_js::bridge::ok(&data)));

    let kv = InMemoryKV::new();
    rt.block_on(kv.set("k", "v")).unwrap();
    group.bench_function("kv.get", |b| b.iter(|| rt.block_on(kv.get("k")).unwrap()));
    group.bench_function("kv.set", |b| {
        b.iter(|| rt.block_on(kv.set("k", "v")).unwrap())
    });

    let da = InMemoryAccessor::new();
    da.seed([json!({"id": 1, "name": "ever"})]);
    group.bench_function("accessor.query", |b| {
        b.iter(|| rt.block_on(da.query("select 1")).unwrap())
    });

    group.finish();
}

/// JS op 全链路：同步 op 用普通 JS 循环，异步 op 用 async IIFE + await。
fn bench_js(c: &mut Criterion) {
    // log 输出到 sink：包含 tracing 格式化成本，但不刷屏。
    tracing_subscriber::fmt()
        .with_writer(std::io::sink)
        .try_init()
        .ok();

    let rt = runtime();
    let mut group = c.benchmark_group("js");

    // 基线：单次 run() 固定开销（脚本编译 + 执行 + event loop 驱动）。
    group.throughput(Throughput::Elements(1));
    let bridge = new_bridge();
    group.bench_function("baseline.run(json.ok x1)", |b| {
        b.iter(|| rt.block_on(bridge.run_with("json.ok(1);", req())).unwrap())
    });

    group.throughput(Throughput::Elements(N as u64));
    let bridge = new_bridge();
    let script = format!("for (let i = 0; i < {N}; i++) json.ok({{i}});");
    group.bench_function("json.ok", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script = format!(r#"for (let i = 0; i < {N}; i++) log.info("bench", "i", i);"#);
    group.bench_function("log.info", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    rt.block_on(bridge.run_with(r#"redis.set("k", "v");"#, req()))
        .unwrap();
    let script =
        format!("(async () => {{ for (let i = 0; i < {N}; i++) await redis.get(\"k\"); }})()");
    group.bench_function("redis.get", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script = format!(
        "(async () => {{ for (let i = 0; i < {N}; i++) await redis.set(\"k\", \"v\"); }})()"
    );
    group.bench_function("redis.set", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script = format!(
        "(async () => {{ for (let i = 0; i < {N}; i++) await db.query(\"select 1\"); }})()"
    );
    group.bench_function("db.query", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script =
        format!("(async () => {{ for (let i = 0; i < {N}; i++) await db.exec(\"update x\"); }})()");
    group.bench_function("db.exec", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    // fetch：本地 keep-alive 服务器（连接由 reqwest 连接池复用，测稳态吞吐）。
    group.throughput(Throughput::Elements(20));
    let bridge = new_bridge();
    let addr = rt.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let body = r#"{"hello":"world"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let resp = resp.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    // 顺序请求（无流水线）：每读到一个请求回一个响应，连接保持。
                    while matches!(s.read(&mut buf).await, Ok(n) if n > 0) {
                        if s.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    });
    let script = format!(
        "(async () => {{ for (let i = 0; i < 20; i++) await fetch(\"http://{addr}/\").then((r) => r.json()); }})()"
    );
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(5))
        .bench_function("fetch(local, x20)", |b| {
            b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
        });

    group.finish();
}

/// ldap 通道对比：同形态 mock 操作（立即 ready 的 bind）分别走
/// typed（`ldap.bind` → op_ldap_call → FfiLdapBackend → LdapVtable）与
/// generic（`axis("ldap").bind` → op_axis_call → GenericVtable），
/// 差额 = 泛型通道相对 typed 通道的纯开销。
fn bench_ldap_channel(c: &mut Criterion) {
    let rt = runtime();
    let ldap_cfg = LdapConfig::from_value(&json!({
        "default": { "url": "ldap://127.0.0.1:1" },
    }))
    .unwrap();
    let axes = registry_from(vec![(
        "ldap".to_string(),
        "ldap".to_string(),
        &MOCK_AXIS_VT,
    )]);
    let bridge = Bridge::with_dbs_and_loader(
        std::collections::HashMap::new(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras {
            ldap: Some(Arc::new(FfiLdapBackend::new(&MOCK_LDAP_VT, ldap_cfg))),
            generic_axes: Some(axes),
            ..Default::default()
        },
    );
    let mut group = c.benchmark_group("js");
    group.throughput(Throughput::Elements(LDAP_N as u64));

    let typed = format!(
        "(async () => {{ for (let i = 0; i < {LDAP_N}; i++) \
            await ldap.bind(\"uid=eve,dc=example,dc=com\", \"pw\"); }})()"
    );
    group.bench_function("ldap.typed(bind, x1000)", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&typed, req())).unwrap())
    });

    let generic = format!(
        "(async () => {{ for (let i = 0; i < {LDAP_N}; i++) \
            await axis(\"ldap\").bind(\"uid=eve,dc=example,dc=com\", \"pw\"); }})()"
    );
    group.bench_function("ldap.generic(bind, x1000)", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&generic, req())).unwrap())
    });

    group.finish();
}

criterion_group!(benches, bench_rust, bench_js, bench_ldap_channel);
criterion_main!(benches);
