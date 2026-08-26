//! bridge 各模块性能测试。
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
use only_js::bridge::{Bridge, DataAccessor, InMemoryAccessor, InMemoryKV, KVStore, RequestInfo};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

/// 每次迭代 JS 循环内的 op 调用次数（fetch 除外）。
const N: usize = 100;

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
    group.bench_function("envelope.ok", |b| {
        b.iter(|| only_js::bridge::ok(&data))
    });

    let kv = InMemoryKV::new();
    rt.block_on(kv.set("k", "v")).unwrap();
    group.bench_function("kv.get", |b| {
        b.iter(|| rt.block_on(kv.get("k")).unwrap())
    });
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
    rt.block_on(bridge.run_with(r#"redis.set("k", "v");"#, req())).unwrap();
    let script = format!("(async () => {{ for (let i = 0; i < {N}; i++) await redis.get(\"k\"); }})()");
    group.bench_function("redis.get", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script = format!("(async () => {{ for (let i = 0; i < {N}; i++) await redis.set(\"k\", \"v\"); }})()");
    group.bench_function("redis.set", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script = format!("(async () => {{ for (let i = 0; i < {N}; i++) await db.query(\"select 1\"); }})()");
    group.bench_function("db.query", |b| {
        b.iter(|| rt.block_on(bridge.run_with(&script, req())).unwrap())
    });

    let bridge = new_bridge();
    let script = format!("(async () => {{ for (let i = 0; i < {N}; i++) await db.exec(\"update x\"); }})()");
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
                let Ok((mut s, _)) = listener.accept().await else { break };
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

criterion_group!(benches, bench_rust, bench_js);
criterion_main!(benches);
