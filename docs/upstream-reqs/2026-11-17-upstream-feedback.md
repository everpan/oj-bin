# 上游反馈回执：REQ-CAP-01～06（v0.1.138 起的承接状态）

> 面向 `oj-wiki-docs/docs/upstream-issues/` 六条 issue 的投递方；依据：细化稿 v3
> （[2026-11-17-collab-reqs-refinement.md](2026-11-17-collab-reqs-refinement.md)，经三方专家
> ——协同编辑/架构师/分布式系统——对抗评审）+ v0.1.138 实际交付。每节可单独摘出回贴 tracker。

## 〇、逐条状态总览

| # | 需求 | 裁决 | 状态 |
|---|---|---|---|
| 01 | 一等 CRDT 协作文档原语 | **拆二**：yjs 导入保证采纳；`collab` 运行时全局降为二阶段（可证伪门），一期 = 官方 blessed JS 库 `oj-collab`（独立 npm 包） | 契约定稿，**B2**（POC 门后动工） |
| 02 | WS 会话/房间二进制状态 | **部分采纳**：拒「sess.state 收 Uint8Array」，代以 `sess.bin`（连接域）+ `ws.roomState`（房间域，按连接键入） | 契约定稿，**B1 后续**（本版未含 WS 面） |
| 03 | KV CAS/setNX/getSet/lock | **全量采纳（含 getSet，需求方追加）** | ✅ **v0.1.138 已交付** |
| 04 | 跨实例房间扇出 | **采纳**：`ws.rooms_cluster` 旗 + 单共享 topic + 实例级单消费者 + boot 探针 + 双向 fail-fast | 契约层已就位（ABI 13 group 参），**核心未实现**（B1 后续） |
| 05 | 二进制更新日志与快照压实 | **改道**：不进 runtime，作为 `oj-collab` 库存储层（db 自增 + 事务 + CAS fencing，**两阶段压实**） | 契约定稿，**B2** |
| 06 | 外部数据源迁移工具 | **采纳**：`oj migrate import`，连接复用 `DbBackendRegistry`（不加 sqlx feature） | **B3** |

## 一、v0.1.138 已交付（可直接消费）

- **REQ-03 全部四件**：`kv.cas`（expected/value 双 null 语义：`expected=null` = 键不存在才写、
  `value=null` = 匹配删除）、`kv.setNX`、`kv.getSet`、`kv.lock`（JS 糖：随机 token，
  `unlock/extend` 走 cas——**返回 false 即租约已失，必须弃权**；retry 经 `tasks.sleep`）。
  真 Redis 臂已实测（EVAL Lua 原子 cas / `SET NX` / `SET … GET`）。
- **REQ-04 的契约层**：`subscribe` 增 group 参——kafka 显式组 = per-subscribe 消费者 +
  `auto.offset.reset=latest`（每实例广播扇出、不回放历史、不与同组多实例抢消费）；
  rabbitmq 现状（匿名排他队列）即天然广播，零改动。
- **ABI 12→13**（`oj-plugin-ffi` 0.1.4）。**升级动作：重新整体拷贝 `bin/`**——旧 ABI 12
  插件会被门禁按设计拒载（报错明示 `plugin=12 host=13`），禁止增量覆盖。
- devkit 四件套已同步：api-manual kv 表四行 + §13 限制表「内存 KV 进程本地」行 / SKILL
  陷阱两行 / scenarios 场景 79「压实单飞 + CAS fencing」可照抄 / `global.d.ts` 类型同步。

## 二、必须转达的三处认知修正（下游文档需修订）

1. **行号勘误**（内容全部属实，仅手册版本漂移，按当前 v0.1.138 手册对照）：
   下游引用 → 实际：kv API L1292-1302 → **1298-1306**；bus.publish 恒 0 L952-953 →
   **958-959**；bus 缺省进程内 L1714 → **1720**、L4056 → **4062**；WS 二进制限制 L4058 →
   **4064**；WS 执行模型 L883-896 → **886-891**；`DB(name)` L1087-1089 → **1095**；
   blob API L1317-1326 → **1320 起**；bytea 信封 L1279-1280 → **1281**；
   crontab L2477-2515 → **2520-2530**。
2. **REQ-01 草图的 `export default { connection(), message() }` 目前跑不了**——上游 WS 是
   纯帧驱动（每路由 W 个无状态 Worker），**不存在 per-connection 生命周期钩子**；
   「握手鉴权在首帧前拒绝」需新增 `connection()`/`disconnect()` 钩子（B1 后续；拒绝用
   4000 段保留 close code——普通 1000 会触发 y-websocket 无限指数退避重连）。
3. **REQ-03/04 各有一个下游未列出的悬崖**：
   (a) 内存 KV（未配 `redis.default`）的 cas/setNX/lock **进程本地**——多实例下
   `kv.lock` 各自成功（假单飞），跨实例协调必须配 Redis；
   (b) bus 两臂实为 **at-most-once**（kafka auto-commit / rabbit 收到即 ack）——主敌是
   丢帧不是重复，「配了 broker 即正确」要补「`oj-collab` 库内置周期 resync 心跳兜底」。

## 三、裁决依据（为何两处与原诉求不同）

- **REQ-01 不做一期 runtime 全局**：一期以官方库交付——保持「WS 无状态 Worker」模型、
  零新有状态子系统、B1 落地即可用；且 yrs **不含 awareness**（awareness 永在 JS 层），
  库不是一次性原型而是常驻生产件。二阶段 native 门可证伪（CI bench 基线：单房间 50 并发
  或帧路径 p99 超基线 ×10 才立项）。三方独立评审（协同/架构/分布式）均支持此拆法。
- **REQ-05 不进 runtime**：db 已具备全部底座（`insert().returning(["id"])` 三方言能力矩阵
  + 事务 + REQ-03 的 CAS），缺的只是一次性表结构 + 事务编排——官方库「只写一次」同样消除
  重复实现。且评审发现原「单事务压实」设计有**数据损坏级洞**（压实提交后读旧快照的读者会
  拿到日志空洞）与 MySQL gap 锁封 append / sqlite 长事务停摆问题——**两阶段压实**
  （tx1 快照 upsert + tx2 分块删除，MySQL 臂 READ COMMITTED）已并入契约；
  fencing 规格三补丁（expectedSeq 只从 kv 读、CAS 前置于一切写、正确性不依赖锁）见细化稿。

## 四、对下游的行动项

1. 升级 v0.1.138：**整体重拷 `bin/`**；多实例部署确认 `redis.default` 已配。
2. 修订 issue 里的行号引用（§二.1 对照表）。
3. REQ-02 验收标准补一条：「awareness 镜像按连接键入（`roomStateSet(room, "<connId>", …)`），
   禁止多客户端共写同一键」（单键单 blob 无合并语义，共写即 LWW 互踩）。
4. REQ-06 的「渲染对账」出 CLI 边界——报告留槽位，内容渲染由模块自校。
5. B2 入口门（yjs 导入 POC：`import * as Y from "yjs"` 在 oj runtime 跑
   `Y.applyUpdate`/`encodeStateAsUpdate` 往返）由上游执行，结论回填本回执。

## 五、里程碑对照（下游 issue 的三批建议 → 上游排期）

| 下游建议批次 | 上游排期 | 差异说明 |
|---|---|---|
| 第一批「协作地基」（#02～#05） | B1（ABI 13 部分**已发** v0.1.138；WS 面/集群扇出核心待续） | #03 全量已交付；#04 契约层已发、核心待续；#02 未动（本版无 WS 面）；#05 改道 B2 |
| 第二批「一等协作原语」（#01） | B2（yjs POC 门 → `oj-collab` npm 包） | 形态从 runtime 全局改为官方库（§三） |
| 第三批「数据迁移」（#06） | B3（随正式数据切换交付） | 采纳，无差异 |
