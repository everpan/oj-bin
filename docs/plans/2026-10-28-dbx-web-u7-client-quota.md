# U-7 client 轴配额与预算 实施计划（v0.1.97）

> 需求原文：`docs/prds/oj-feature-requirements.md` §U-7；第一刀口径：交接文档 §6 行 3。

**Goal**：动态数据源（client 轴）补齐配额三件——per-owner 登记上限、全局并发流预算、
`db.client.test` 防内网端口扫描（拨号超时 + 连续失败锁退）——并把「真实并发」算式写进文档。

**Architecture**：全部在 host 侧（`ClientDbRegistry` + 既有 op 插桩），**零 ABI/插件改动**。
流预算用 `tokio::sync::Semaphore`，permit 由 pump task 持有、随流结束（自然/abort/close）释放；
扫描防护按 owner（operator 串）记连续 connect 失败，达阈值锁退一段窗口。

**范围裁决**（诚实登记）：
- 「查询超时 / 结果集字节上限」**不做**：raw `db.query` 全平台无查询超时（非 client 轴独有缺口）；
  行数上限走既有 `db_access.max_rows`（对动态库按别名声明即可）。真要查询超时须平台级方案，另期。
- 拨号超时与扫描阈值用**常量**（5s / 8 次 / 60s 锁退），不加配置键——真网络差到 5s 拨不通的
  部署本就不该用 `db.client.test` 做常规运维。

## 决策表

| 项 | 落点 | 值 |
|---|---|---|
| `db_client.max_per_owner` | `DbClientSection`（serde field default 8）+ 装配期 0 拒启、> max_entries 拒启 | 8 |
| `db_client.max_streams` | 同上（default 32）+ 装配期 0 拒启 | 32 |
| per-owner 计数 | `ClientDbRegistry::register` 锁内按 `registered_by`（Option<String> 相等）计数 | — |
| 全局流预算 | `ClientDbRegistry.streams: Semaphore`；`op_db_stream_open` 对**动态别名** try_acquire，permit 移入 pump | 32 |
| 扫描锁退 | `ClientDbRegistry.scan: Mutex<HashMap<owner,(consecutive,locked_until)>>`；test/register/reRegister 共用 | 8 次 → 锁 60s |
| 拨号超时 | `tokio::time::timeout(5s, connect)` 包住 test/register/reRegister 的 connect | 5s |
| 拒绝日志 | `DB_CLIENT_DENY: action=… reason=… owner=… alias=…`（U-9 归一时的第三类出口） | — |

**Owner key**：`db_policy::operator_of(user)` 串（与 `ClientDbEntry.registered_by` 同源），None 记 `"-"`。

## 测试锚（Review Focus）

1. per-owner 上限：同 owner 第 N+1 个登记被拒且错误可辨识（含 cap 值）；异 owner 不受影响；delete 释放。
2. 流预算：信号量占满后 `try_acquire` 失败，错误点名 `db_client.max_streams`；permit 随 pump 结束释放。
3. 扫描锁退：连续 8 次 connect 失败 → 第 9 次在**拨号之前**被拒（错误含 retry 提示）；成功一次即清零。
4. 兼容性：旧配置（无新键）解析后 max_per_owner=8 / max_streams=32；`enabled=true` 旧配置照常启动。

---
