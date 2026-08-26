# bridge 性能测试

基准：`cargo bench`（criterion，release 模式）。
环境：macOS aarch64（Apple Silicon），deno_core 0.409 / V8 150.4，2026-08-02。

## 口径

- **rust/\***：纯 Rust 层（信封序列化、trait 实现单次调用），无 JS 开销。
- **js/\***：JS op 全链路（JS 调用 → op → Rust → Promise 解析）。每次 criterion 迭代在
  JS 循环内执行 100 次 op（fetch 为 20 次），表中"单次 op"为迭代均值 ÷ 次数。
- `log.info` 的 tracing 输出到 `io::sink`：含格式化成本，不含真实 I/O。
- `fetch` 打本地 loopback keep-alive 服务器，连接由 reqwest 连接池复用（稳态口径）。
- 每次迭代含一次脚本编译+执行（约 0.5 µs 基线），100 次 op 摊薄后约占 1–2%。

## 优化前后对比

| 基准 | 优化前 | 优化后 | 提升 | 手段 |
|---|---:|---:|---:|---|
| rust/envelope.ok | 490 ns / 2.04 M/s | **70 ns / 14.2 M/s** | **7.0x** | 信封单遍序列化（不再构中间 `Value` 树） |
| js/json.ok | 460 ns / 2.17 M/s | **230 ns / 4.35 M/s** | **2.0x** | 同上（信封占其成本大头） |
| js/fetch(local) | 5.45 ms / 183/s | **29.7 µs / 33.7 K/s** | **183x** | `no_proxy` + keep-alive 复用（见下） |
| js/baseline.run | 704 ns | 515 ns | 1.4x | （V8 代码缓存预热，顺带改善） |
| js/redis.get / redis.set | 123 / 152 ns | 138 / 155 ns | ±0 | 已在 op 边界地板 |
| js/db.query / db.exec | 409 / 135 ns | 413 / 146 ns | ±0 | 大头为行数据 serde_v8 物化，不动 |
| js/log.info | 619 ns | 611 ns | ±0 | 大头为 tracing 格式化，不动 |

优化后完整结果：

| 基准 | 迭代均值 | 单次 op | 实测吞吐 |
|---|---:|---:|---:|
| rust/envelope.ok | 70 ns | 70 ns | 14.2 M/s |
| rust/kv.get | 76 ns | 76 ns | 13.2 M/s |
| rust/kv.set | 80 ns | 80 ns | 12.5 M/s |
| rust/accessor.query | 145 ns | 145 ns | 6.89 M/s |
| js/baseline.run（json.ok ×1，固定开销） | 515 ns | — | 1.94 M/s |
| js/json.ok ×100 | 23.0 µs | **230 ns** | 4.35 M/s |
| js/log.info ×100（含格式化） | 61.1 µs | **611 ns** | 1.64 M/s |
| js/redis.get ×100 | 13.8 µs | **138 ns** | 7.26 M/s |
| js/redis.set ×100 | 15.5 µs | **155 ns** | 6.46 M/s |
| js/db.query ×100（1 行结果） | 41.3 µs | **413 ns** | 2.42 M/s |
| js/db.exec ×100 | 14.6 µs | **146 ns** | 6.87 M/s |
| js/fetch(local) ×20 | 593 µs | **29.7 µs** | 33.7 K/s |

## fetch 优化的根因

最初 5.45 ms/req 不是 JS/op 路径开销：本机装有系统代理（127.0.0.1:7890，Clash 系），
reqwest 默认读取 macOS 系统代理配置，且回环例外（`127.*`）未生效——回环流量被送进
本机代理进程（服务器端看到的 `User-Agent: Go-http-client/1.1` 即代理转发证据），
每请求一次新建代理连接。修复：Bridge 的 reqwest Client 改 `no_proxy`（不走系统代理），
纯 reqwest 回环验证从 1.9 ms/req 降至 65 µs/req。
需要代理支持时按配置注入 `Proxy`（见 `mod.rs` 中 ponytail 注释）。

## 解读

- **异步 op 边界约 140 ns**（redis.get/db.exec，7 M/s 级），这是 JS↔Rust Promise 往返的
  地板成本，剩下都是业务代码自己的开销。
- **序列化是主要变量成本**：信封 marshal 已从 op 路径中消除（单遍写 buffer）；
  db.query 的行数据 serde_v8 物化（约 270 ns/行级对象）在真实 SQL 实现接入后仍是
  主要开销，如需再压可评估 `#[buffer]`/resource 句柄直传。

## 复现

```bash
cargo bench                    # 全量（约 40 秒）
cargo bench -- js/db.query     # 单个
open target/criterion/report/index.html   # criterion HTML 报告（含吞吐曲线）
```
