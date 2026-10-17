# 模块写隔离设计（module_write_guard）

日期：2026-10-17
状态：**评审定稿 / 待实施**（工程师 + 架构师 + 产品 + 安全 四路只读评审已收敛）
来源：下游 oj-module 诉求 OJ-7（docs/prd/202610170016-oj-table-write-guard-requirement.md）
关联：src/bridge/guard.rs、src/bridge/mod.rs、src/bridge/query.rs、src/bridge/db.rs、oj/src/app.rs、oj/src/migrate.rs、oj/src/schema.rs、src/config.rs
基线：oj v0.1.76

---

## 0. 给不同读者的三句话

- **平台/运维（部署方）**：以后「哪个模块能写哪些表」由你写在 config.yaml 里；模块自己在 manifest 里写的声明一律不作数。没写授权的模块，只能写自己的表。
- **模块开发者**：manifest.yaml 里新增的 export_writes 是**声明**（表达「我需要什么权限」），不是授权。oj build --check 会告诉你「声明的权限 ⊄ 部署方授予」，据此去申请。
- **下游 oj-module**：方向认可，但形态要改——见 §13「与 OJ-7 原文的差异」。

---

## 1. 为什么要做：三个真实场景

### 场景 A｜第三方市场模块偷改角色表

order 模块（市场下载的第三方）想给自己提权，只要在自己的 manifest.yaml 写一行：

    deps: { _platform: "^1.0.0" }     # 自证：我依赖平台模块

然后 db.exec("UPDATE sys_role SET ...")——今天直接成功。
原因：deps 由访问方自证，且守卫不区分读写（guard.rs:249 judge 命中即放行）。

### 场景 B｜共享 service 的「混淆代理人」

_platform/_shared/roles.ts 是平台共享 service，任何模块都能 import。今天 order 模块 import 后调用它，
SQL 以「roles.ts 所在模块」的视角执行，等于借平台之手改写 sys_role。平台侧无论怎么收紧 service 内部代码都挡不住。

（好消息：oj 的模块身份按入口文件的祖先目录命中，src/bridge/mod.rs:1326-1339 → ReqState.module。
所以共享 service 天然以调用方身份执行，场景 B 在身份层已经可解，缺的只是「写」的判定。）

### 场景 C｜一个 migration 造出一张全平台可写的表

模块自带 <module>/migrations/0002__init.sql：

    CREATE TABLE shadow (id INT PRIMARY KEY, payload TEXT);
    UPDATE sys_user_role SET role_id = 1 WHERE user_id = 42;

- dev 模式 server.migrate_on_start 默认 auto（src/config.rs:170-174），进程启动即 apply，无人值守；
- migration 走 DataAccessor 直执（oj/src/migrate.rs:345），不过任何守卫；
- 新建的 shadow 表不在任何 schema.yaml 里 → owner_of = None → guard.rs:250 直接放行 → 此后任意模块可任意读写它。

> 这条链的严重性高于 A/B：攻击者不需要 deps、不需要绕扫描、不需要任何授权，放一个文件即可。

---

## 2. 目标与非目标

**目标**：让「外部/第三方解决方案模块」即使被植入恶意代码，也无法写入（污染、删改）它不该写的表；
并使平台方能在安装期明确看到并批准「这个模块将获得哪些写权限」。

**非目标**（沿用 OJ-7 §10，另加两条）：

- 不做进程/沙箱隔离、不要求 JS 运行时隔离；
- 不改 ownership_guard 现有语义（新开关独立灰度）；
- 不做行级（RLS）——列级已够，行级由宿主 service 承担；
- 不承担代码审计、签名验签、市场审核、审计留存告警（归市场/安装器）；
- 不接管安装器/运维身份的写操作（但模块自带的 migration 文件要受审，见 §6）。

---

## 3. 方案总览：一个谓词，四个消费方

核心是单一判定谓词：

    may_write(module, table, verb, anchored) -> allow | deny

| 要素 | 今天 | 方案后 |
|---|---|---|
| 归属（table → owner） | 模块自带 schema.yaml（app.rs:427），自证 | **平台侧锚定**；模块声明降级为「申请」，未锚定 = 无主 |
| 授权源 | 访问方自证 deps（app.rs:449-450） | **config（部署方/安装器）授予**，模块不可自配 |
| 读写 | 不区分 | 分开判：SELECT 看 deps；写看授予 |
| 无主表 | 放行（fail-open，guard.rs:250） | **拒绝**（fail-closed） |
| 无模块身份（WS/任务） | 放行（guard.rs:237） | 按入口文件补身份；补不出则拒绝 |
| migration | 完全不受限 | 走同一谓词（执行侧） |

**四个消费方共用一份实现**，避免「dev 放行、release 拒绝」这种上线才炸的分裂：

1. 运行期 SQL 守卫（guard.rs judge）
2. migration 执行侧（oj/src/migrate.rs:414 apply_module，模块名与 SQL 已在内存，不需要 OpState）
3. config 授予校验（装配期：授予指向非属主的表 → fail-fast）
4. 安装期写面渲染（把谓词实际授予集渲染给用户确认）

---

## 4. 配置形态（部署方视角）

    server:
      ownership_guard: warn          # 既有，语义不变
      module_write_guard: deny       # 新增：off（默认，零破坏）| warn | deny

    platform:
      modules: [_platform, auth]     # 既有：平台特权模块
      root: _platform

    # 新增：模块授信档位 + 写授权（部署方/市场安装器生成，模块自己写的不作数）
    modules:
      _platform:
        tier: platform               # platform | internal | third_party（缺省 third_party）
      order:
        tier: third_party
        write_tables:                # 授予：允许写哪些「别人的」表
          - { table: sys_oplog, mode: append_only }
          - { table: users, mode: update_only, columns: [avatar_base64] }
        allow_raw_write: false       # 第三方档默认 false → 写只能走构造器（见 §5.3）

**缺省规则（安全默认）**：

- 未出现在 modules: 下的模块 = third_party 档 = 只能写自己归属的表，一切跨模块写、DDL、裸 SQL 写均拒；
- write_tables 缺省 = 空 = 拒绝；授权与 mode 只写这一处（不两处可争、审计不用拼）；
- mode：full（默认，增删改）/ append_only（仅 INSERT）/ update_only + columns（仅白名单列）。

manifest.yaml 侧的 export_writes 降级为声明：

    # order/manifest.yaml
    export_writes:                   # 声明：「我需要这些写权限」
      sys_oplog: { mode: append_only }

oj build --check 输出：声明的写权限 ⊄ config 授予：sys_oplog 未授予 → 开发者本地闭环；
安装期把它当作默认建议值渲染给部署方确认（部署方并不知道 sys_oplog 该 append_only，属主知道）。

---

## 5. 判定口径

### 5.1 归属锚定（一切的前提）

现状的致命点是「判据由被约束方提供」：owner_of 的输入是模块自带的 schema.yaml，且类型是 Option，
None 时直接放行。于是两条路都能绕开授权：① 首占——目标表无人声明，自己在 schema.yaml 声明即「成为」属主
（app.rs:436-443 的 S002 只防抢已被声明的表，防不住首占）；② 干脆不声明 → owner = None → 不设防。

**修法**：归属以平台侧权威源为准——config 显式登记，或以库内实际元数据对账
（oj/src/schema.rs:379-382 已有 information_schema 内省能力）。模块 schema.yaml 降级为申请，
未锚定的表 fail-closed deny。

### 5.2 verb 判定（读写分离，不做通用 SQL 解析）

不引入 sqlparser（方言分歧 + INSERT..SELECT / CTE 必误判）。沿用 guard.rs:445-458 的「诚实降级」口径：

1. 先 strip_literals（去字符串/注释）再取首词；
2. 首词 ∈ {INSERT, UPDATE, DELETE, REPLACE, MERGE, TRUNCATE, ALTER, DROP, CREATE, CALL, GRANT, REVOKE} → 写；
3. 首词 = WITH / 注释残留 / 未知 → 按写处理（证明不了只读即写）；
4. SELECT ... FOR UPDATE 特判为读；
5. owner = None / 无模块身份 → 按写处理并 deny（deny 档）。

### 5.3 R0：第三方模块的写只走构造器（性价比最高的一条）

构造器写面是封闭子集：src/bridge/query.rs:157-162 有 Verb::{Insert,Update,Delete}，
query.rs:1025-1080 在 AST 层硬拒 joins / unions / with / columns / 派生表 →
目标表唯一、verb 精确、req.sets 精确已知，不靠词法扫描。

所以：第三方档模块禁止裸 SQL 写（db.query / db.exec 的写语句），只允许构造器写。
这让 append_only、列白名单从「文本猜测」变成「AST 精确裁决」——是让授权真正生效的唯一通路。

执行约束（避免硬断裂）：构造器目前无 upsert / ON CONFLICT / 多表 UPDATE / CTE 写，
存量下游会当场红。故必须 warn 一版观察 → 给构造器补 upsert → 再切 deny；缺口未补齐前，
用 config 侧临时豁免名单（部署方授予、可审计、可回收），不用模块自带的逃生注释。

### 5.4 覆盖全部执行入口（补身份面）

module = None 时守卫整体短路（guard.rs:237），且是 ownership 与 tenant 两道一起短路：
WS 帧（mod.rs:1035）、ws_connect（mod.rs:1102）、task_connect（mod.rs:1176）、run_with。
修治病灶集中在 checkout_armed / checkout_reset（mod.rs:916 / mod.rs:880）——
按入口文件路径复用 run_module 的祖先命中逻辑注入 ModuleCtx，后端判定侧无需改动。

---

## 6. migration 执行侧三条规则

落点：oj/src/migrate.rs:414 apply_module(acc, module: &str, mdir: &Path, ...)——
模块名是显式入参、SQL 已在内存，不需要造 OpState，一处覆盖 dev 自动 apply / release verify / 手动 oj migrate。
因此 dev 默认 auto 不必改，也不必禁止第三方携带 migration。

| # | 规则 | 说明 |
|---|---|---|
| 1 | 任何 verb 的目标表 ∈ **平台锚定的本模块归属集** | 判据不是「本模块 schema.yaml 已声明集」——那仍是自证，首占路径完整存活 |
| 2 | DROP / TRUNCATE **全局禁** | 对齐 guard.rs 既有「DDL 无法注入条件 → 一律拒」口径；含首占成功的本模块表 |
| 3 | 破坏性 ALTER 子类型按分级灰度 | 加列是官方演进路径（oj/src/checks.rs:223 文案指向 migrations/），不禁；DROP COLUMN / 改类型另档，third_party 先 deny、platform/internal 先 warn；方言差异按「ALTER TABLE 后第二个关键词」枚举，判不出即归破坏性 |

已否决：整体禁 ALTER（会让存量模块 apply_all 启动即死，migrate.rs:503 逐模块 map_err，违反零破坏）；
一律禁 DML（会把一次性数据回填逼进随启动重放的 seed.sql，语义错误，且该洞已被规则 1 覆盖）。

---

## 7. 覆盖面矩阵

| 上下文 | 受守卫 | 理由 |
|---|---|---|
| 模块路由 handler | 是 | 主目标 |
| 共享 service（被任意模块 import） | 是，以**调用方**身份 | 场景 B 的解法 |
| 模块自带 WS / ws.ts | 是（补身份后） | 今天 module=None 全开 |
| 定时任务 / 定时器 / run_with | 是（补身份后） | 同上 |
| **模块自带 migrations/*.sql** | **是（执行侧）** | 场景 C，最高危；S007 今天只查序号连续 |
| seed.sql | 是（执行侧，同规则 1） | 启动重放（oj/src/seed.rs:174-178），S006 只在 build 期 |
| 安装器 / 市场程序 / 手工 oj migrate | 否 | 平台/运维身份 |
| db.asSystem() | 加 config 白名单 + 审计（P1） | 现状无门禁（db.rs:376-382），一行绕过全部租户隔离 |

---

## 8. 场景说明（方案落地后逐条对照）

### S1 第三方模块偷改角色表（场景 A 的解）

order（third_party）执行 db.exec("UPDATE sys_role SET ...")，
modules.order.write_tables 无 sys_role → 拒绝，500 返回：

    oj module write denied: module "order" cannot write table "sys_role"
      owner: _platform | verb: UPDATE
      grant: config modules.order.write_tables -> add { table: sys_role, mode: ... }
      config: server.module_write_guard=deny

把 deps: {_platform} 写进 manifest 不再起作用（deps 退化为「读 + 耦合声明」）。

### S2 共享 service 被外部模块调用（场景 B 的解）

order import _platform/_shared/roles.ts 后执行写 sys_role：
身份按入口文件落在 order → 以 order 的授予集裁决 → 拒绝。
平台不需要改一行 service 代码，也不依赖调用方自律。

### S3 审计日志只允许追加

modules.order.write_tables 授予 sys_oplog + append_only：
INSERT 通过；UPDATE / DELETE 拒绝。删审计的通路被堵死（OJ-7 的典型诉求）。

### S4 恶意 migration（场景 C 的解）

- CREATE TABLE shadow → 规则 1：shadow ∉ 平台锚定的本模块归属集 → 拒绝；
- UPDATE sys_user_role → 规则 1 → 拒绝；
- DROP TABLE users → 规则 2 → 全局禁；
- 即便它首占成功把 shadow 声明进自己的 schema.yaml，规则 1 的判据是平台锚定集，首占拿不到增益。

### S5 部分部署 / 表还没属主

灰度期只部署了 order，_platform 未部署 → sys_role 的 owner 未锚定。
今天 owner_of = None 放行；方案后 fail-closed 拒绝（deny 档）+ 一条告警。
（OJ-7 第 11 条把这条列为「实现期定义」，本设计直接定为 deny + 告警，不留未决项。）

### S6 WS 推送模块 / 定时任务写表

第三方模块自带 ws.ts 在连接帧里写 users：今天 module=None 守卫全关；
补身份后按文件祖先命中 order → 走同一谓词 → 拒绝（无授予）。

### S7 平台自研模块的日常演进（零回归）

_platform 加列 ALTER TABLE users ADD COLUMN vip INT：
tier=platform → 规则 3 允许（非破坏性子类型）；DROP COLUMN 在 platform 档先 warn、third_party 直接 deny。

### S8 表外写面（第二期，须明示）

kv / blob / bus / es 今天完全没有模块命名空间：键、桶、topic、索引全局共享，
第三方模块可以删掉别人的缓存与对象、订阅别人的 topic、把数据塞进 ES；fetch 无出网白名单
（src/bridge/fs.rs:44-46 明示 net 恒放行）。
表守卫挡不住「把数据搬到表外再外传」，故表外 capability 收口必须与表同级排进 P1，
否则「写隔离」只是把门从正门挪到侧门。

### S9 第三方模块想 import 平台 service（可选，P1-5）

第三方档的 order 模块写：

    import { createSearch } from "#/_platform/_shared/search";

今天只要补一行 deps 即可（这是 docs/devkit/scenarios.md:1993 教过的公开用法）。
方案后：loader 直接拒绝（dev 立即报错；release 在 build 期拒绝，不拖到线上首次请求）。
平台能力改走受控入口。

**只对 third_party 档生效**——platform / internal 档保持现状，避免破坏下游既有用法。

### S10 为什么不做「把 _shared 收进 runtime 内建」

把平台 service 做成框架内建的 ext: 模块，**技术上可行且泄漏面封闭**：
deno_core 禁止非 ext:/node: 的引用方导入 ext:（modules/map.rs:1412-1441），oj loader 只放行 file:
（module_loader.rs:60-64），双重封堵；op_* 只从 ext:core/ops 导出、fs 有 jail、error.stack 不外泄。
但本设计不采用，两个理由：

1. **内建不自动提权，强行提权反而更糟**。内建代码走同一批 op，必经 guard；ReqState.module 由入口文件
   命中、内建代码不改它 → judge 仍按**业务模块**裁决 → 内建 service 写 sys_role 照样被拒。
   为了让它能写自己的表而授予平台身份（asSystem 只置 rs.system，judge 不看，得另开闸门），
   业务模块一调用就拿到平台身份——**混淆代理人从「偷偷借刀」升级成「官方盖章的借刀」**。
2. **_platform 是部署方资产，不是框架资产**：各家 users/tenant 表结构与权限模型不同，
   框架已提供 platform.root 受控挂载路线（config.rs:308-318），取向本就是「受控目录」而非「内建」。
   内建还会绑死框架版本（dist/manifests.yaml 里 _platform 是独立版本，下游 5 个模块写了 deps）、
   失去 dev 热重载、使 S003/S008 全部重定义。

取而代之做 **import 可见性分级**（P1-5）：保留文件形态，只限制谁能 import 谁。

---

## 9. 报错与审计

- 用户可见：500 + 可操作指引（属主、verb、如何授予），对齐 ownership_guard 的成功经验；
- 结构化审计字段：{ ts, module, table, owner, verb, anchored, granted, decision, sql_fingerprint }。
  今天拒绝路径只有 eprintln（guard.rs:265），事后不可取证——必须补；
- 审计对象：asSystem() 调用、migration 执行、逃生门出现、deny 事件。

---

## 10. 灰度与零破坏

- 新增独立开关 server.module_write_guard: off|warn|deny，不动 ownership_guard；
- 路径：off（默认，零破坏）→ warn（观测现有跨模块写）→ deny（收口）；
- 所有破坏性规则挂同一开关：off/warn 档只告警，不得 fail-fast。
  尤其 migrate 侧——migrate.rs:503 逐模块 map_err，dev auto 下 fail 即启动即死，比运行期 500 严重一档；
- 存量模块在 off/warn 档下不得启动失败（写进验收，不只在正文）。

---

## 11. 实施阶段

| 阶段 | 内容 | 主要落点 |
|---|---|---|
| P0-1 | 归属图平台侧锚定 + 未锚定表 fail-closed | oj/src/schema.rs（information_schema 对账）、guard.rs:250 反号 |
| P0-2 | 补 module=None 面（WS / task / run_with） | src/bridge/mod.rs:880/916 |
| P0-3 | migration 执行侧三条规则 | oj/src/migrate.rs:414（apply_module / apply_all / verify_all） |
| P1-1 | 单一谓词 may_write + 四消费方 | guard.rs、migrate.rs、app.rs 装配校验、安装期渲染 |
| P1-2 | config modules: 段（tier + write_tables） | src/config.rs（复用 platform.modules 形状，config.rs:308） |
| P1-3 | 表外 capability 收口（kv/blob/bus/es/fetch） | 各 op 侧引入模块前缀/命名空间 |
| P1-4 | db.asSystem() 加 config 白名单 + 审计 | src/bridge/db.rs:376 |
| P1-5 | **import 可见性分级**：third_party 档禁止 import 本模块之外的目录 | src/bridge/module_loader.rs:57-76（resolve_inner 拦截点）+ oj/src/build_cmd.rs:634 与 checks.rs:131（S008 补相对写法，双落点） |
| P2-1 | R0：构造器补 upsert → warn → deny 裸 SQL 写 | src/bridge/query.rs |
| P2-2 | verb 判定 + DDL fail-closed + append_only / 列白名单 | guard.rs |

ABI/FFI 全程无影响（改动在 bridge/oj/config 侧，插件契约不动）。
与多租户正交：tenant 管行、capability 管模块。

---

## 12. 验收标准

沿用 OJ-7 §9 第 1–11 条，另补 7 条：

| # | 用例 | 期望 |
|---|---|---|
| 12 | 模块 migration 触碰他模块表 | 拒绝；dev 自动 apply 场景也必须生效（不能只在 build 生效） |
| 13 | 模块 migration 含 DROP / TRUNCATE | 拒绝（含首占成功的本模块表） |
| 14 | 首占夺主（third_party 档声明拥有他人已存在物理表） | 拒绝或被平台侧归属图否决 |
| 15 | 口径一致 | 同一条 SQL 在 dev 自动 apply 与 release verify 下判定结果相同 |
| 16 | 未锚定表（owner = None）在 deny 档 | 拒绝 + 告警，不得静默放行 |
| 17 | 安装期渲染的写面清单 | = 谓词实际授予集（防「展示一套、生效另一套」） |
| 18 | 存量模块在 off/warn 档 | 不得启动失败（零破坏回归） |

---

## 13. 与 OJ-7 原文的差异（给下游的答复要点）

| OJ-7 原文 | 本设计 | 理由 |
|---|---|---|
| R1 export_writes 由**属主**授权 | 授予主体改为 config（部署方/安装器），export_writes 降级为声明 | 市场场景下属主模块自己也可能是第三方，export_writes: {users:{to:"*"}} 一行全开。把信任从访问方挪到属主，只是挪到另一个同样不可信的模块，没挪出模块层 |
| R2 读写分离 | 保留，提前做 | 真有效；判据按 verb，不做通用 SQL 解析 |
| R3 列白名单 | 保留，但绑构造器 | 构造器 AST 里 sets 精确已知；裸 SQL 路径上判列必漏 |
| R4 逃生门可关可观测 | 方向修正 | oj:allow-table 只在 build 期生效（oj/src/checks.rs:403），src/bridge/guard.rs 全文不认——运行期本就没有这个门。R4 改为：build 期可关 + 留痕；运行期 deny 不可被任何注释旁路 |
| §7 migration 不接管 | 必须接管 | 见场景 C，最高危通道 |
| §2「manifest.tables = 属主清单」 | 事实勘误 | 运行期零参与，归属锚定对象是 schema.yaml（app.rs:427/433） |
| §5 默认 off | 接受默认 off，但市场场景目标档为「未授信模块 deny / 授信模块 warn」 | 默认关闭的守卫等于不存在；但一次切 deny 会硬断裂，故走 §10 灰度 |

---

## 14. 明确不覆盖（避免过承诺）

- **dist 侧静态检查不算安全控制**：checks::run 唯一调用点是 oj build（build_cmd.rs:64），
  release 装配（app.rs:1526-1590）不跑任何 S*；且被扫的 SQL/manifest 与 dist 同出一个攻击者，
  仍是自证，只能当 lint。真正产生 trust 的是「从 src 重 build」或平台侧签名/授信锚。
- **行级隔离**：构造器 update/delete 要求 conditions 非空是防误操作的完整性门禁，不是 scope 约束；行级由宿主 service 承担。
- **模块自带 migration 无害性**：规则只保证「不污染他模块」；本模块自有表仍可被自家 migration 改写，纵深上要靠审计。
- **不做 _platform/_shared 整块内建**（区别于 P1-5）：机制可行（ext: 模块对业务不可 import，泄漏面封闭），
  但内建代码仍以调用方身份过守卫；为让它写自己的表而授予平台身份，等于把「借刀」官方化。
  且 _platform 是部署方资产，内建会绑死框架版本、失去热重载。详见 §8 S10。
- **顺带发现的一处既有缺陷**（建议单独立项，与本需求无关）：
  check_tenant_raw 用 split_whitespace().next() 取首词 → 前导注释（/* x */ delete from t）或
  WITH 前缀使首词取不到动词，fail-closed 不触发，租户防护在这些形态下静默降级。
