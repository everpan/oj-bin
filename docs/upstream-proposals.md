# 上游提案追踪（upstream proposals）

> 本文件追踪提交/实现的上游能力提案（PR 性质）。每条：提案要点、裁决（做/不做/
> 缓做及理由）、落点（版本 + 代码/文档）、验收状态。新提案先入表再实现；
> 实现完成后把「状态」改为已闭环并补验收口径。历史单篇提案文档：
> [`sql-builder-missing-capabilities-pr.md`](./sql-builder-missing-capabilities-pr.md)。

| ID | 提案 | 裁决 | 落点 | 状态 |
|----|------|------|------|------|
| R7 | 同运行时内部 dispatch 原语（`bridge.call`） | 做（R7-B 隔离内部 actor 池；否决 R7-A 同 isolate——KillSwitch 单槽与失败 runtime 中 drop 的 V8 句柄风险；R7-C MCP 定向过窄） | v0.1.131：`src/bridge/dispatch_bridge.rs` + `serve/src/internal.rs` + `bootstrap.js`；文档 api-manual §6「bridge」/场景 76/SKILL.md 三行 | ✅ 已闭环（2026-11-12）：池 1+1 并发 4 全 200 无挂死 + MCP tools/call 转发 + 深度 2 护栏 503，serve e2e 与全量门禁绿 |
| R6 | Service Credential 原生自动轮换（rotate/current） | 做（消费方惰性自愈；**缓做** `rotates_at`/`rotation_policy` 服务端主动续签——消费方 rotate 已覆盖诉求，yagni；**不做**「永不过期」——另立 R 项） | v0.1.132：`serve/src/svc_gate.rs` 宽限语义 + `serve/src/dispatch.rs` 豁免 + `sample/src/system/service-token/` + 迁移 0003；文档 api-manual §14 轮换小节/场景 77/SKILL.md 三行/mcp-bot-guide §7 | ✅ 已闭环（2026-11-12）：SvcGate 宽限单测 6/6（宽限内放行/过期移出/文本列宽容）；sample 集成测试 `svc-rotation.test.ts`（签发→rotate→current→409 护栏→链式续换→list 不回显明文） |

---

## R7：同运行时内部 dispatch 原语（2026-11-12 提交，同日闭环）

**问题**：handler 作代理/编排层（bot 代理 obo 换签后转发 `tools/call` 到
`POST /api/mcp`）用 `fetch` 回环本进程时，内层 handler 再占一个公共 actor 槽——
并发 ≥ pool_size 全池互相等待永久挂死（看门狗熔断不了 host 侧 await）。

**方案**：`await bridge.call({method, path, headers?, body?})` → 标准 WHATWG
`Response`；目标 endpoint 在**隔离内部 actor 池**执行，公共池不因自环耗尽。

**边界**（成文限制）：非流式（64 MiB/30s 缓冲，超限 502）；深度限 1（dispatch
目标再 call → 503）；`oj serve` 与 `oj test` 可用，裸嵌入式抛
`dispatch not available`。

## R6：Service Credential 原生自动轮换（2026-11-12 提交，同日闭环）

**问题**：svc bearer 是 90d 定长 JWT，到期只能人工 SOP 重签重配；签名密钥在
oj 侧，消费方无法自签而不破坏信任根。

**方案**：由「当前有效 svc bearer 自身」认证的两个端点（密钥不出 oj）：
`POST /v1/api/system/service-token/rotate`（旧行 superseded + 24h 宽限 → 同
name/scopes 签新 token 落库）与 `GET …/current`（取最新 active bearer，跨重启
恢复）。SvcGate 行有效判定升级为 `status`/`grace_until` 过滤——宽限过期自动
移出有效集（40311 同路径）。

**关键裁决**：
- 宽限 24h 固定（无配置项）——配置项是 yagni，出现第二个需求再提；
- rotate/current **scopes 豁免**（read 档 bot 必须能自轮换，强制 write 是提权
  面），**jti 门照常**（fail-closed 不动）；
- 明文 token 落库（R6 跨重启恢复的前提）——暴露面不变：admin list 永不选择
  token 列，明文只在 create 响应与 svc 自认证 `current`；
- 幂等护栏：宽限内旧 bearer 再 rotate → 409，保证同一 name 任意时刻至多一行
  active。

**范围外**（提案自述）：服务端主动续签（`rotates_at`/`rotation_policy`）；
「永不过期」（ttl_secs 钳 ≤90d 且 exp 必设，另立 R 项）；obo 受托 token（无
状态、到期即废，维持原设计）。

**验证期修正**（v0.1.132 落地时，相对 R9 既有"30s TTL 双向滞后"语义的两处偏离）：
SvcGate `admits` 改为**正缓存 30s + miss 穿透**——新签发/轮换的 jti 即时可过门
（等满 TTL 则"无感知续期"不成立），吊销/宽限移出仍 ≤30s 生效；另 SvcGate 装配
库键跟随 `profiles.db` 重定向（`oj test` 走 `db.test` 时不再误读物理 default）。

**评审发现收口**（v0.1.133，外部 review 三条，R6 未打标签期内落地）：
① rotate 并发竞态——SELECT 校验与 UPDATE 之间无锁，两个并发 rotate 可各插一行
active。改为**条件 UPDATE 栅栏**（`WHERE jti=? AND (status IS NULL OR
status='active')` 按影响行数判胜负，行锁串行化后到者 0 行 → 409）；`current`
同秒平局补 jti 字典序 tie-break。真实 dev server 三轮双并发 curl 实测 [0,409]。
② devkit 口径冲突——mcp-bot-guide「落库只存 jti」为 R6 前陈旧表述（评审引用的
`_platform/schema.yaml`/`issueSvcToken` 均不存在；明文列在 admin schema 的
`token`，create/rotate 均落明文）。已纠正并显式声明：**整表即凭据，库导出/
备份按凭据物料管控**（暴露面正式记录，之前只在我方风险清单）。
③ SvcGate miss 穿透无负缓存——伪造 jti 每请求真查全表。穿透仍 miss 记 **2s
负缓存**（窗内 fail-closed 不打库；shortcut：窗内新签发 jti 最长拖 2s，128bit
随机 jti 先探后签概率可忽略）。
