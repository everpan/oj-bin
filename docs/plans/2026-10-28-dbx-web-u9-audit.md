# U-9 审计事件归一 + 可检索存储 实施计划（v0.1.98）

> 需求原文：`docs/prds/oj-feature-requirements.md` §U-9；交接文档 §6 行 4。

**Goal**：把安全审计的散点 stderr 出口（`DB_CLIENT_AUDIT` / `DB_CLIENT_DENY` / `DB_ACCESS_AUDIT` /
`DB_SESSION_*`）归一到**统一事件类型 + 单一 emit 出口**；事件入**进程内有界环形存储**；
新增 `audit.query(filter)` 检索出口（即导出通道），留存按条数 + 年龄双限自动清理。

**Architecture**：`src/bridge/audit.rs`（事件 + 存储 + emit/query）；`AuditStore` 挂
`StableState.audit`（进程级，热更存活，Extras 注入同一 Arc——与 tx_sessions 同款）；
`TxSessionRegistry` 持 store 克隆，让 **reaper 后台路径**（TIMEOUT 判死）也能入库。
stderr 行统一为 `OJ_AUDIT: kind=… user=… tenant=… alias=… detail=…`。

**范围裁决**（诚实登记）：
- **`SqlEvent`（sql_trace）不归**：那是 per-request dev 画像（全 SQL 入库 = 内存与开销不可控），
  与「安全审计」是两回事；边界写进 db-guide。PRD 的「SQL/行数/成败/耗时」列落在 sql_trace 的
  `db.sqlProfile()` 既有出口。
- **存储为进程内环形缓冲，不跨重启**：PRD 验收 =「按 user/alias/时间检索 + 留存到期自动清理」，
  环形缓冲全覆盖；跨重启持久化（外部库 sink）另期（接 audit.rs 单一出口即可，不散点）。
- **防篡改不做**（PRD 需求行里有；进程内缓冲无此性质——登记为已知限制，真需求接外部 sink）。

## 决策表

| 项 | 值 |
|---|---|
| `audit:` 配置段 | `{ enabled: true（缺省开）, max_entries: 2000, retention_secs: 86400, allow: AllowList }`；0 值装配期拒启；`enabled && allow 全缺（不限制）` → 装配期 **warn**（不 fail——新段不 brick 既有部署，读面默认放开因事件零凭据且进程内） |
| 事件字段 | `ts_ms / kind / user / tenant / module / source / alias / detail`（零凭据：alias/dialect/动作/id 前缀，无 DSN 无密码） |
| kind 集合 | `DB_CLIENT_AUDIT / DB_CLIENT_DENY / DB_ACCESS_AUDIT / DB_SESSION_{BEGIN,COMMIT,ROLLBACK,TIMEOUT,TIMEOUT_ROLLBACK_FAILED,DISCARD,CLOSE_ERR}` |
| stderr 格式 | `OJ_AUDIT: kind=X user=… tenant=… module=… source=… alias=… detail="…"`（旧散点前缀退役——**CHANGELOG 登记 breaking**，日志解析器需更新） |
| 检索 | `audit.query({kind?, user?, tenant?, alias?, sinceMs?, untilMs?, limit?})` → 最新在前；limit 缺省 100、硬顶 1000。门 = `audit.allow` 四维并集（同 client_gate 形态，每 op 重判） |
| reaper 入库 | `TxSessionRegistry` 构造时持 `Arc<AuditStore>` 克隆（`new(cfg, audit)`），TIMEOUT/TIMEOUT_ROLLBACK_FAILED 照常入库 |

## 测试锚（Review Focus）

1. 环形缓冲双限：超 max_entries 丢最旧；超 retention 的条目查询/入库时被清。
2. 过滤器：kind/user/tenant/alias/since/until 各维独立生效；limit 截断；空过滤 = 全量（受 limit）。
3. 门：`allow` 未命中 → query 拒（错误不回显 allow）；命中 → 拿到含别主体事件（管理员视角全局可见）。
4. reaper 路径入库：TIMEOUT 事件（后台、无 OpState）也进存储。
5. 零凭据：任何事件字段不含 DSN/密码（事件构造处只喂 alias/dialect/action/前缀 id）。
