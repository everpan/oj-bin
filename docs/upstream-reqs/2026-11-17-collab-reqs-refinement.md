# 上游需求细化：协作能力缺口 REQ-CAP-01～06（定稿 v3）

> **性质**：oj 上游（本仓库）对下游 `oj-wiki-docs/docs/upstream-issues/` 六条 issue 草稿的
> **承接细化与裁决**。下游草稿回答「要什么」；本稿回答「上游怎么落、落哪层、代价多大、
> 哪些诉求应当改道」。
> **证据基线**：全部结论基于 2026-11-17 对本仓库工作树的代码核实（非转抄下游声明），
> 关键证据以 `file:line` 标注。下游文档行号勘误见 §三.4。
> **评审状态**：v3 定稿 —— v2 已并入三方专家（协同编辑 / 架构师 / 分布式系统）对抗评审的
> 全部「必须修改项」（裁决汇总见 §六）；§四的 5 个决策点已由需求方裁决（2026-11-17），
> 详见 §四决策记录。

---

## 〇、裁决总表

| REQ | 下游诉求 | 上游裁决 | 落点形态 | ABI | 批次 |
|---|---|---|---|---|---|
| 01 | `collab` 运行时全局 + yjs 官方保证 | **拆二**：yjs 导入保证采纳；`collab` 全局降为二阶段（可证伪门），一期官方 blessed JS 库 `oj-collab`（独立 npm 包，常驻 awareness 基座） | JS 库 + CI | 无 | B2（POC 门） |
| 02 | `sess.state` 收二进制 + `ws.roomState` | **部分采纳**：拒「sess.state 收 Uint8Array」，改 `sess.bin*`（连接域）+ `ws.roomState*`（房间域，按连接键入） | core ops（ABI 外） | 无 | B1 |
| 03 | `kv.cas/setNX/getSet/lock` | **采纳（含 `getSet`）**；`lock` 为 bootstrap.js 内 JS 糖 | FFI vtable + core op | **13** | B1 |
| 04 | 集群原生房间扇出 | **采纳**：`ws.rooms_cluster` 旗 + 单共享 topic + 实例级单消费者 + 实例标签过滤；语义直陈 at-most-once、以不丢帧为先 | core + FFI vtable（仅 kafka 臂） | **13**（合并一次 bump） | B1 |
| 05 | `collab.*` 持久化原语进 runtime | **改道**：不进 runtime；作为 `oj-collab` 库的存储层（db 自增 + 事务 + CAS fencing，两阶段压实） | JS 库 | 无 | B2 |
| 06 | `oj migrate import` 外部数据导入 | **采纳**；源/目标连接复用 `DbBackendRegistry`（不加 sqlx feature） | oj CLI 子命令 | 无 | B3 |

依赖关系不变（下游 §七）：03→05，02/03/04/05→01。批次见 §五。

---

## 一、ABI 13 打包方案（一次 bump，两契约变更）

按仓库惯例（ABI 12 / M1 先例：宿主 + 插件同产物重编发布），本批全部契约变更**合并为一次
ABI 12→13 bump**：

1. **KVStoreVtable 增三槽**（`oj-plugin-ffi/src/kv.rs:13-27`，现 5 具名函数 + connect/close，
   无任何通用 command 逃生口——已核实）：
   - `cas(key, expected: ROption<RString>, value: ROption<RString>, ttl_sec: ROption<u64>) -> bool`
     —— `expected=null` 表「键不存在才写」；`value=null` 表「匹配则删除」（解锁原语）。
     退化组合 `cas(k, null, null)` 定义为「键不存在则成功（幂等 noop）」。
   - `setnx(key, value, ttl_sec: ROption<u64>) -> bool`。
   - `get_set(key, value: RString, ttl_sec: ROption<u64>) -> ROption<RString>` ——
     原子交换并返回旧值（键不存在返回 None）；redis 臂以 `SET key value GET [EX ttl]`
     实现（GETSET 自 6.2 起废弃，不用）。
2. **EventBrokerVtable.subscribe 增 group 参**（`oj-plugin-ffi/src/bus.rs:11-22`，现单参）：
   `subscribe(topic, group: ROption<RString>)`。**group 三层语义成文**（优先级从高到低）：
   subscribe 参 `Some(g)` → 以 g 建订阅；`None` → 回落 cfg `broker.group`（`src/config.rs:577-584`
   既有 `BrokerCfg.group`）。不变量改写为「**每 (topic, group) 至多一条订阅**」。
   - **成本声明**：对 kafka 臂这是**消费者生命周期改造**（现插件构造期建单消费者、group 固定
     `"oj-bus"`，`plugins/oj-bus-kafka/src/lib.rs:65`），不是签名微调——按订阅建消费者、
     按订阅管理 offset 提交（见 REF-04 手动 commit）。
   - rabbitmq 臂现状（匿名 exclusive auto-delete 队列，每订阅者各一份，
     `plugins/oj-bus-rabbitmq/src/lib.rs:170,178-190`）无需改动即天然广播扇出。

「尾部追加 + 长度字段」的兼容布局**不做**：ABI 门禁是严格相等，旧插件整体拒载，
兼容布局在现政策下零收益（架构师评审确认）。

---

## 二、逐条细化

### REF-03　KV CAS / setNX / lock（B1，无前置依赖）

**契约（JS 面，kv 与 redis 同源同面自动生效）**：

```ts
kv.cas(key, expected: string | null, value: string | null, ttlSec?: number): Promise<boolean>
kv.setNX(key, value: string, ttlSec?: number): Promise<boolean>
kv.getSet(key, value: string, ttlSec?): Promise<string | null>  // 原子交换，返回旧值（不存在为 null）
kv.lock(key, { ttlSec = 30, retry?: { tries, delayMs } }): Promise<Lock>
// Lock: { token, unlock(): Promise<boolean>, extend(): Promise<boolean> }
```

- `lock` 是 **JS 糖**（bootstrap.js 实现，零 vtable 成本）：acquire = `setNX(key, token, ttlSec)`；
  `unlock` = `cas(key, token, null)`；`extend` = `cas(key, token, token, ttlSec)`。
  **extend 返回 false = 已失去锁，调用方必须立即放弃工作**（弃权契约，写进文档）。
- **fencing 模式写进文档**：正确性**不得依赖锁**——锁只是吞吐优化；持锁方提交关键变更时
  再以 `cas` 校验代际值兜底（规格见 REF-05，含 expected 来源的硬性规定）。

**实现落点**：
- trait `KVStore`（`src/bridge/kv.rs:19-34`）增 `cas` / `setnx`；内置 `InMemoryKV`
  （`src/bridge/kv.rs:40-101`，RwLock<HashMap> 惰性过期）实现规约（分布式评审钉死）：
  **过期判定所用的 `Instant::now()` 必须在写锁内取**——锁外取 now + 锁上排队会产生
  「陈旧 now 判未过期 → 双持有者」时序；单把写锁内完成「查过期+比较+写」即原子，
  多 runtime 池共享同一 Arc 无碍；
- oj-kv-redis：`setnx` = `SET NX [EX]`；`cas` = 单条 EVAL Lua 原子 compare-and-set/del；
- ops：`op_kv_cas` / `op_kv_setnx`（`src/bridge/mod.rs:574-578` 处注册）。

**非目标**：Redlock 多节点仲裁。

**验收（测试化）**：
- InMemoryKV 单测：N 并发 setNX/lock 仅一胜出；CAS 值已变返回 false 且不写；TTL 到期后
  可重新获取；错 token unlock 返回 false；**「TTL 到期瞬间并发 extend vs setnx 循环竞争：
  任一时刻 ≤1 持有者」混沌用例钉住 now-在锁内规约**。
- redis 腿 env-gated（`OJ_TEST_REDIS=127.0.0.1:6399`——本机既有 `poc-redis` 容器
  redis:8-alpine 已复用常驻，2026-11-17 实测 PONG）跑同一组断言。

### REF-02　二进制会话 / 房间状态（B1，无前置依赖）

**裁决：拒绝「sess.state 直接收 Uint8Array」**——sess.state 的传输契约是每帧
`serde_json::to_string` 注入 + `op_ws_sess_set(#[serde] Value)` 回传
（`src/bridge/frame_pool.rs:432`、`src/bridge/ws.rs:229-231`），塞二进制意味着引入
`{"$bytes": base64}` 隐形信封，复杂度上移而语义不变，且「不可序列化静默丢失」的坑原样保留。
**替代方案**：

```ts
sess.binSet(key, bytes)   // 连接域；断连自动释放
sess.binGet(key): Uint8Array | null
ws.roomStateSet(room, key, bytes)   // 房间域；进程内 hub；值帽默认 1 MiB 可配
ws.roomStateGet(room, key): Uint8Array | null
ws.roomStateDel(room, key)
```

- **实现落点**：`WsHub`（`src/bridge/ws.rs:25-27`）增连接域与房间域两个
  `Mutex<HashMap<…, BTreeMap<String, Vec<u8>>>>`（连接域键 conn id，断连 leave 时清理）；
  ops `op_ws_bin_*` / `op_ws_room_state_*`。
- **roomState 键布局约定（写死，防 LWW 互踩）**：awareness 聚合一律**按连接键入**
  `roomStateSet(room, "<connId>", bytes)`；**禁止多客户端共写同一键**——单键单 blob 无合并
  语义，共写即互踩。迟到者全量读取各键逐个 apply（由 `oj-collab` 库封装）。
  SKILL.md 放「sess.state / sess.bin / roomState / kv」四选一决策表。
- **集群语义**：房间状态**进程本地镜像、最终一致**（文档明写，非强一致全局）；集群下靠
  REF-04 中继更新各实例镜像，幽灵光标由 awareness beacon 自愈（~15s 重播 / 30s 超时）。
- `ws.broadcast` 已天然排除发送者自身（`ws.rs:170-181`，hub_tests 有断言）——中继语义现成。

**验收（测试化）**：往返无损；断连后连接域条目消失；值帽超限报错明确；房间隔离；
**两客户端各发 awareness 帧，镜像无互踩（迟到者读到两份均 apply）**。

### REF-04　跨实例房间扇出（B1，依赖 broker 运维能力[已有]）

**投递语义（直陈，不粉饰）**：bus 两臂现均为 **at-most-once**（kafka
`enable.auto.commit=true`，`oj-bus-kafka/src/lib.rs:84-85`；rabbit「收到即 ack」，
`oj-bus-rabbitmq/src/lib.rs:3`）——**重复基本不存在，丢帧才是主敌**：kafka poll 后、
auto-commit（默认 ~5s）前崩溃 ⇒ echo 永久丢失 ⇒ 该实例在线客户端静默分叉直至重连。
评级：CRDT 帧丢失 = HIGH（Yjs 无反熵自愈）；awareness 帧丢失 = LOW（beacon 自愈）。
**缓解必选其一 + 兜底**：kafka 臂改**手动 commit（本地投递完成后提交）**；
`oj-collab` 库内置周期 resync（快照+日志重放对账）兜一切丢帧。

**契约**：

```yaml
ws:
  rooms_cluster: true   # 缺省 false；贴 ws.workers_per_route 风格（架构师评审定名）
```

- **单共享 topic**（复用 broker 配置，`cluster.topic` 砍掉——BrokerCfg 已有 `topic_prefix`
  先例，单值无 knob 必要）；**实例级单消费者**：每实例对 topic 建一条订阅
  （rabbit = 一个匿名排他队列；kafka = group 为 boot uuid 的独立消费者），
  收到后经 WsHub 按房间扇出。**禁止按 WS 连接各 subscribe**（N 连接 N 队列 N 倍放大）。
- envelope：`{room, inst, seq, bytes}`——`seq` 为 **per-room 单调序号**（取自
  appendUpdate 返回的 db 日志 seq），用于缺口检测、join 去重与 resync 对账。
- 集群模式 `ws.broadcast`：**本地成员直接投递（延迟 0）+ publish 到 topic**；各实例收到后
  `inst == self` 丢弃、否则投递本地房间成员。收敛已推演确认：发送者实例的其他成员由
  本地直投覆盖，发送者自身协议上无需回发（客户端发送前已本地 apply）。
- **不变量（写进文档）**：房间成员表与 roomState 始终进程本地；跨实例只传帧。
  迟到加入者的文档恢复走 db（REF-05 的 join-before-replay），**不依赖中继回放**——因此
  kafka `auto.offset.reset` 须由现码 **earliest 改为 latest**（`lib.rs:85` 现为 earliest，
  属本批必改项）+ per-instance boot-uuid group：新实例不消费历史帧，停机窗口帧按设计丢弃
  （由 resync 兜底）。**per-instance group 同时消掉了实例间 rebalance 抢消费**（本设计
  最关键收益，写进文档）。
- **fail-fast 门禁（三面封悬崖）**：
  1. `rooms_cluster=true` 而 broker 仍为进程内 → 启动拒绝；
  2. `rooms_cluster=true` 而 kv 仍为内置 InMemoryKV → 启动拒绝（InMemoryKV 锁/CAS 进程
     本地，`src/bridge/kv.rs:40-101`——跨实例协调会假成功，第二个悬崖）；错误信息须点名
     两个条件 + 给出修法（声明 `redis.default`/kv 插件与 `broker.kind`）+ 明示单实例部署
     不应开此旗；
  3. **boot 探针**：rooms_cluster=true 时对 broker 做可达性探测（kafka metadata 请求 /
     rabbit 连接+topic 权限试产试消）——kafka 生产者离线构造、惰性兜底
     （`lib.rs:84` 附近）会静默骗过启动门禁；
  4. **反向告警**：broker 已配集群后端而 `rooms_cluster=false` → 启动告警（回到静默
     单实例是原始故障模式，不能无声）。
- `roomSize` 保持本地语义并文档化；跨实例 presence 由 awareness 中继覆盖，**不新增全局
  presence 原语**。CRDT 更新可交换 ⇒ 中继顺序不关正确性只关延迟。
- **规模上界（诚实声明）**：单 topic 广播 = 每实例收所有活跃房间全部帧；按 wiki 场景
  （活跃房间 ≪ 总页面数、实例 ~10）成立，超出再议 per-room topic。
- topic 名漂移（实例间配置不一致）无法自动检测——运维文档明示。

**验收（测试化）**：两实例各连一客户端，任一发帧两端都收到且模块代码零改动；无 broker /
无 redis 下开旗 → 启动失败且文案含修复指引；`inst==self` 不重复投递；
**kill -9 于 kafka poll 后 commit 前窗口 ⇒ seq 缺口被检测且 resync 愈合**；
kafka 同 broker 下业务订阅与 rooms 中继并存互不抢消费（(topic,group) 不变量）。

### REF-05　更新日志与快照压实（B2，依赖 REF-03；裁决：改道为库）

**裁决：不进 runtime**——db 已具备全部底座：自增主键给单调序号
（`insert().returning(["id"])`；PG/sqlite `id_returning`、MySQL `id_two_step`，
`docs/devkit/api-manual.md:2145-2146`）、事务给原子性；剩余是一次性的表结构 + 事务编排，
属库代码。下游「每模块重复实现」由「官方库只写一次」消除（与 REF-01 同理）。

**库内契约（`oj-collab` 存储层，模块以 schema.yaml 迁移建表）**：

```sql
collab_log(doc_key, seq 自增 PK, upd blob, ts)     -- append = insert().returning(["id"])
collab_snap(doc_key PK, seq, bytes)                -- 快照指针 = 单行 upsert
```

```ts
appendUpdate(docKey, bytes): number          // 先持久、后广播（顺序契约，见下）
readUpdatesSince(docKey, seq, limit?): {seq, bytes}[]
readSnapshot(docKey): {seq, bytes} | null
compact(docKey): void
```

**压实 = 两阶段（评审后改定，替代原「单事务」设计）**：

1. **tx1**：合并生成新快照 → upsert `collab_snap(seq=S₂)`（单行原子换指针）；
2. **tx2**：日志**分块删除** `DELETE WHERE seq<=S₂`（每块 ≤N 行）。

理由（分布式评审）：
- **读一致性洞**：单事务「upsert+DELETE」下，先读到旧快照 S₁ 的读者会拿到
  「S₁ + 日志空洞」而重建损坏——两阶段 + **重建读事务化**（下述）双保险；
- **MySQL gap 锁**：RR 下大范围 `DELETE seq<=S` 的 next-key/gap 锁会封住并发 append
  （阻塞乃至 lock wait timeout 丢帧）——tx2 降 **READ COMMITTED**（仅行锁）+ 分块；
  sqlite 单写者长 DELETE 会冻结帧路径——分块同样消解；PG RC 本就仅行锁；
- **崩溃间隙无害**：tx1 后 tx2 前崩溃，残留日志中的已合并 update 重放为 no-op
  （yjs applyUpdate 幂等），压实可重试，快照与日志始终可用；
- **fencing 规格（三补丁，防永久变砖）**：`expectedSeq` **只从 kv 读**
  （`cas("collab:compact:<key>", expected, S₂)`，expected 为压实开始时 kv 中的代际值）——
  若从 db 快照行读，「kv CAS 成功 → db 提交前崩溃」会造成 kv/db 永久错位、此后所有压实
  CAS 永假；**kv CAS 成功是 tx1 的前置门禁，失败则禁止一切 db 写**（kv CAS 不随 db
  事务回滚，表述不得混为「事务内首句」）；双压实者推演：谁后 CAS 谁失败回滚，
  快照内容是 seq 前缀的确定函数 ⇒ 数据无损，锁只决定谁先做无用功。
- **重建读规约**：`readSnapshot` + `readUpdatesSince` 在**同一 REPEATABLE READ 事务**内
  执行（MySQL 默认 RR；PG 显式设置；sqlite 天然串行）——杜绝读侧看到半压实状态。
- **append 侧**：append 失败可重试（db INSERT 幂等性由调用方 token 去重或 yjs update
  可交换性兜底）；`oj-collab` 可选 50–100ms micro-batch 合并行（写放大降 5–10 倍，
  正确性中性）。

**验收（测试化）**：并发追加序号单调无丢失；压实后快照+剩余更新重建与压实前逐字节一致；
**压实进行中并发读快照+日志重建无损**；**租约短于压实（注入 pause 使 A 超时、B 介入）：
恰一 CAS 胜者，B 先/A 先两序重建等价**；**kv CAS 后 db 提交前 kill -9：重启压实不死砖**；
**压实 DELETE 期间并发 append ×三方言：零死锁、零丢 seq、尾延迟有界**；
压实中途模拟崩溃可恢复可重试。

### REF-01　一等协作文档原语（B2，POC 门；裁决：拆二）

**1a　yjs 官方导入保证 —— 采纳，POC 先行**：
- POC（上游跑，B2 入口门）：sample 模块 `import * as Y from "yjs"`，
  `Y.applyUpdate` / `Y.encodeStateAsUpdate` / awareness 编解码往返，`oj test` 运行。
  事实预期：yjs/lib0 本体为环境无关纯 JS 无 Node 内建；真实风险在 **lib0 的 exports
  子路径映射 + ESM/CJS 双格式解析**（版本间布局变过）——POC 半天可证伪，**锁死版本矩阵**；
- POC 通过 → CI 常驻腿（版本固定矩阵）+ devkit 四件套写入兼容矩阵；失败 → 升级为
  runtime 侧 module_loader CJS 互操作修复议题，不阻塞 B1；
- yjs 来源 = 模块自带 node_modules（裸 specifier 已支持，`module_loader.rs:426-485`）；
  vendor 进 runtime 不采纳（2026-11-17 裁决：模块自带 node_modules + CI 兼容矩阵）。

**1b　`collab` 全局 —— 降为二阶段，一期交付官方库 `oj-collab`（独立 npm 包）**：
- **分发**：独立 npm 包（仓库发布 tarball / 私有 registry），经既有 node_modules 裸
  specifier 消费；bin/devkit 只做文档+类型渠道。文档划界：api-manual 只收平台原语
  （sess.bin/roomState/kv.cas/lock 及错误/限制表）；库 API 进库 README；
  scenarios/SKILL 收场景与陷阱条目；README 记兼容矩阵（库版本 ↔ runtime 版本 ↔ yjs 版本
  三钉）；xtask 契约用例校验文档与源一致（同 devkit 四件套机制）。
- **常驻基座声明（架构师评审）**：yrs 不含 awareness（那是 y-protocols 的 JS 编码）——
  即使二阶段 native 化，awareness 永远留在 JS 层。`oj-collab` 不是一次性原型，
  是常驻生产件，按生产件质量门槛交付（CI + 契约测试）。
- **一期库职责**：
  - 房间 join/sync（y-protocols sync step1/2/update）；**step2 差量由服务端库做**——
    无常驻 Y.Doc，迟到加入者以 `Y.mergeUpdates` + 临时文档 `encodeStateAsUpdate(sv)`
    生成差量（O(文档) CPU，压实兜底），写明；
  - **join-before-replay 不变量**：先 join 房间收 live 帧（缓冲，不丢——Yjs 无反熵自愈，
    丢帧=永久分叉）→ `readSnapshot` 得 S → `readUpdatesSince(S)` 重放 → live 帧
    `seq<=S` 丢弃、`>S` 幂等重放，缓冲按序 flush；重放缓冲设内存帽；
  - **append 先持久后广播**顺序契约（中继帧携带日志 seq，缺口可检测）；
  - awareness 中继（`ws.broadcast` 排己 + REF-02 roomState 按连接键入镜像）；
  - **周期 resync**（快照+日志重放对账）兜中继丢帧；
  - 授权与生命周期：依赖 §三.1 的 `connection()`/`disconnect()` 钩子对；
  - 可选 micro-batch append。
- **二阶段触发门（可证伪化）**：B2 交付时建 CI bench（模拟单房间 50 客户端 awareness+
  update 帧路径）取 p99 实测基线；**触发条件 = 单房间并发编辑者 ≥50 或帧路径 p99 >
  基线 ×10**（数字随基线定稿，测量装置先于数字存在——架构师评审要求）。

**下游验收四条全部保留**；「模块代码无部署细节」由库承担；「断线重连正确恢复」由
join-before-replay + resync 覆盖。**测试基线（协同评审）**：3 客户端并发编辑+各自断线
重连后 `encodeStateAsUpdate` 逐字节一致；重放期间持续 live 帧到达、加入者最终无缺帧；
双实例 echo 无遗漏无重复（帧计数断言）；错误凭证首帧前关闭且客户端不无限重连。

### REF-06　外部数据源迁移工具（B3，独立）

**采纳，形态 = `oj migrate import` CLI 子命令**：

```bash
oj migrate import --from <dsn> --to <dsn|named> --mapping <file> \
  [--dry-run] [--on-conflict skip|update] [--report <file.json>]
```

- **连接复用 `DbBackendRegistry`**（`oj/src/serve_cmd.rs:1569-1583`）：PG 源 =
  oj-db-postgres 插件、MySQL 源 = oj-db-mysql 插件、sqlite 直连（内置二后端，
  `src/bridge/db_backend.rs:36-41`）。**不给 CLI 加 sqlx pg/mysql feature**——维持
  「生产路径的 PG/MySQL 由插件承载」约定（根 `Cargo.toml:106-114`）。
- **类型保真矩阵**：UUID→CHAR(36)；bytea→BLOB 列（源读侧经 `$bytea:<base64>` cellenv
  信封解码，`api-manual.md:1281`，字节无损）；JSONB→TEXT(JSON)；TIMESTAMPTZ→TIMESTAMP
  （UTC 归一）；BIGINT→BIGINT。
- **幂等/断点**：目标库检查点账本 `_oj_import_state(table, watermark, updated_at)`
  （前向 only，与 `_oj_migrations` 同风格）；行级幂等 = 源 PK 直写 +
  `--on-conflict skip|update`（缺省 skip）。
- **对账报告**（JSON）：每表行数（源/目标/差异）、抽样行 hash、冲突计数定位到 PK；
  「渲染对账」**出界**——报告留槽位，内容渲染由下游模块自校。
- **dry-run**：零写入，只解析 mapping + 源探测 + 冲突预演。

**验收（测试化）**：含 UUIDv7/bytea/JSONB/时间戳/外键的样例 PG 库（env-gated，
复用本机 oj-test PG 容器）全量导入，字节级与行数对账通过；中断续跑零重复；dry-run 零写入。

---

## 三、新增发现（下游六条未覆盖，由本轮核实带出）

1. **`connection()`/`disconnect()` 生命周期钩子缺失**（B1，core 变更，无 ABI）：下游 REF-01
   草图假设 `export default { connection(), message() }`，但上游 WS 是纯帧驱动
   （`src/bridge/frame_pool.rs`），不存在 per-connection 入口/出口钩子。需在 frame_pool +
   serve 的 WS accept/close 路径增加可选 `connection()` / `disconnect()` 导出：
   - `connection()`：**accept 后、首帧派发前**调用，抛错/返回拒绝信号 → 以**保留 close
     code（4000 段，如 4001）**关闭——普通 1000/1001 会触发 y-websocket 无限指数退避
     重连，文档写明客户端在该 code 停 provider；
   - `disconnect()`：离开时广播 awareness 置空（他人光标立即消失）+ 清理 roomState
     镜像死条目。
2. **InMemoryKV 跨实例假成功悬崖**：kv 缺省内置 InMemoryKV（`oj/src/app.rs:117,134`），
   其 lock/CAS 进程本地——多实例下 `kv.lock` 各自成功（假单飞）。B1 fail-fast 联动门禁
   封死（REF-04 机制节 2）。
3. **bus 两臂 at-most-once + kafka `auto.offset.reset` 现为 earliest**（`lib.rs:84-85`）：
   与 REF-04「不回放历史」的需期相悖，earliest→latest 属本批必改项（§REF-04）。
4. **下游文档行号勘误**（内容全部属实，仅行号漂移，供对方修订 issue）：
   kv API L1292-1302 → **1298-1306**；bus.publish 恒 0 L952-953 → **958-959**；
   bus 缺省进程内 L1714 → **1720**、L4056 → **4062**；WS 二进制限制 L4058 → **4064**；
   WS 执行模型 L883-896 → **886-891**；`DB(name)` L1087-1089 → **1095**；
   blob API L1317-1326 → **1320 起**；bytea 信封 L1279-1280 → **1281**；
   crontab L2477-2515 → **2520-2530**。

---

## 四、决策记录（需求方裁决，2026-11-17）

| # | 决策点 | 裁决 |
|---|---|---|
| 1 | REF-01 形态 | 确认：一期 blessed 库 `oj-collab` + 二阶段可证伪门 |
| 2 | REF-02 裁决 | 确认：拒 sess.state 收二进制，采用 `sess.bin` + `ws.roomState` |
| 3 | yjs 来源 | 模块自带 node_modules + CI 兼容矩阵（不 vendor） |
| 4 | `getSet` | 一并实现（已并入 ABI 13，见 §一；契约见 REF-03） |
| 5 | redis 测试容器 | 复用本机既有 `poc-redis`（redis:8-alpine，127.0.0.1:6399），无需新起 |

本稿为定稿；再改上述任一裁决须在本节追加变更记录。

## 五、批次与里程碑

- **B1 协作地基**（一次发版，含 ABI 13）：REF-03（cas/setnx + InMemoryKV 规约）+
  REF-02（sess.bin/roomState）+ `connection()`/`disconnect()` 钩子（含 close code 契约）+
  REF-04（rooms_cluster 旗、实例级单消费者、手动 commit、earliest→latest、boot 探针、
  双向 fail-fast 门禁）；devkit 四件套逐条同步，`cargo xtask build` 归置。
- **B2 协作库**（POC 门）：yjs 导入 POC → 通过则交付 `oj-collab` npm 包（含 REF-05
  存储层两阶段压实 + join-before-replay + resync）+ CI 兼容矩阵 + 基线 bench；不通过则
  先修 module_loader CJS 互操作。
- **B3 迁移工具**：`oj migrate import`，随下游正式数据切换交付。

每批独立评审与验收；B1 是 B2 的硬前置，B3 与 B1/B2 无耦合可并行。

---

## 六、三方专家评审汇总（v2 并入记录）

| 评审方 | REQ-01 | REQ-02 | REQ-03 | REQ-04 | REQ-05 | REQ-06 |
|---|---|---|---|---|---|---|
| 协同编辑 | 有保留→已补 | 同意 | —（未评） | —（未评） | 同意（补读一致） | — |
| 架构师 | 有保留→已补 | 同意（附键布局条件） | 同意 | 有保留→已补 | 同意 | 同意 |
| 分布式 | 一期可行（库担 resync） | 可接受 | 采纳（附 fencing 补丁） | 采纳（附五条件） | 改道正确（附两阶段） | 不涉及 |

**已并入的关键修正**：压实/读一致性洞与两阶段压实（协同+分布式）；fencing expectedSeq
只读 kv、CAS 前置于写（分布式，防压实永久变砖）；bus 两臂 at-most-once 改口 + 手动
commit + boot 探针 + 反向告警 + 实例级单消费者 + earliest→latest（分布式）；
roomState 按连接键入防 LWW 互踩（架构师）；group 三层语义与 (topic,group) 不变量
（架构师）；join-before-replay + 重放缓冲 + append 先持久后广播 + close code 契约 +
disconnect 钩子（协同+分布式）；二阶段触发门可证伪化与 awareness 常驻 JS 声明（架构师）；
oj-collab 独立 npm 分发与文档划界（架构师）；InMemoryKV now-在锁内规约（分布式）；
`cluster.topic` 删除、旗名改 `ws.rooms_cluster`（架构师）。

**评审方法说明**：三方各自独立读稿 + 抽查源码佐证（行号均已复核）；三份评审未交叉见过
彼此结论，冲突项以实现成本与正确性优先裁决（如两阶段压实同时满足协同的读一致与
分布式的 gap 锁/停摆诉求）。
