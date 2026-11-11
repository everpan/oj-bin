# MCP 三痛点设计 v2：源码层描述 + 代理凭据（OBO）+ admit 级 scopes 收窄

日期：2026-11-11（v2：五路评审〔builder-check / 架构师 / 产品经理 / 机器人集成工程师 / 体验客户〕修订 + 用户三裁决）
状态：待批准
前置：`docs/superpowers/specs/2026-10-30-mcp-jsonrpc-design.md`（MCP 双线路已落地）。

## 意图与动机

现 MCP 实现三个缺口（v1 调研核实）：

1. **描述贫瘠**：`serve/src/openapi.rs:66` 把 operation `summary` 写死为
   `"GET v1/api/user/account"`——LLM 只知 method+url。`.schema` 无描述性字段，
   且 `description` **不在** `SUPPORTED_KEYWORDS`/`SCHEMA_CHANNELS` 白名单
   （`src/contract.rs:33-49,55`），今天写进契约会装配期 fail-fast。
2. **凭据是人的凭据**：MCP 复用 Bearer 守卫，均为短时效人类登录 token；
   企微机器人等机器接入方没有合法凭据形态，无「机器人代理员工」的身份传播。
3. **权限全量静态**：annotations 只随 tool 发给 client 供参考；`/rpc` 与直连
   REST 对同一凭据无任何档位约束；`server.mcp.modules` 是部署级白名单。

用户裁决（brainstorm 确认 + v2 三裁决）：

- 描述：**开发者在代码里写为主，管理界面运行期补救为辅**——v2 裁决补救层
  **后移一个迭代**（见「后续迭代」节，设计保留、触发条件写死）。
- 凭据：**代理（OBO）模式**——机器人经换签持有员工短时 token 执行。
- 权限：**三档 scopes（read/write/destructive）够用，最终权限面以员工自身
  roles 为准**——scopes 只收窄、永不放大。
- v2 裁决：`act_mask_destructive` **缺省关 + 文档标注「推荐开启」** +
  sample 配置示范开启；审计做到**换签审计表**（全量调用流水表记为后续）。

## 目标 / 非目标 / 反目标

目标：
- P1：`.schema` 声明 operation/参数描述进 spec（白名单放行 + 纯增量键）；
  MCP tool description 选取声明值。
- P2：claims 全链路（签发+守卫两端）→ 服务令牌签发/列表/吊销 → 绑定表
  （provider+external_sub，CSV 导入导出）→ `auth/obo` 换签（租户复制、
  停用拒绝、错误码表、换签审计表）。
- P3：**scopes/jti/act_mask 执行点统一下沉到 `dispatch::admit`**（一处覆盖
  `/mcp`、`/rpc`、直连 REST）；`tools/list` 裁剪退化为展示层；isError
  统一 `[code=<n>] <msg>` 前缀。

非目标：
- OAuth 2.1 授权服务器 / `.well-known` / 动态客户端注册（接入方是自己人）。
- prompts / resources（维持 v1 裁定）。
- 路由级自定义档位（`.schema` 标记记为后续；v1 按 HTTP 方法映射）。
- overlay 本迭代不实现（v2 裁决）。
- 全量调用流水表（v2 裁决：只做换签审计表）。
- 员工自助绑定（后续=企微 OIDC 登录成功即自动绑定，见后续迭代节）。

反目标（做了就算失败）：
1. scopes/act_mask **放大**任何权限——handler 级角色检查原样保留。
2. 机器人凭自身凭据**声称**任意员工身份——必须经 obo 换签。
3. 未声明 description 的路由 spec **逐字节变化**（CI 漂移门禁必须不破）。
4. obo 换签不验 jti、或换签 token 时效过长；svc token 泄露后删行仍可用
  （jti 检查必须覆盖全部请求路径，30s TTL 缓存陈旧为上限）。
5. scopes 校验只在 `/mcp` 面——`/rpc` 与直连 REST 必须同规则（v1 草案曾被
   架构师击穿，本版以 admit 单点执行修复）。

## 前置件：JWT claims 全链路（v1「零插件改动」声明作废）

builder-check + 架构师共同核实：两端 `Claims` 均为固定结构体，自定义 claim
会被 serde 静默丢弃。本设计的一切凭据机制以此为地基，**必须最先交付**：

- `src/bridge/crypto.rs`：`Claims` 增 `#[serde(default)] claims: Option<Value>`；
  `op_jwt_sign` 增可选 `claims`（对象，整体并入 JWT payload 的 `claims` 键）与
  `ttl_secs`（显式时效；服务端钳上限常量 90d，缺省仍 `cfg.access_secs`）。
- `plugins/oj-auth/src/lib.rs`：`Claims` 同形增字段，`verify_jwt` 透传——
  guard 返回的 `claims` 从固定四键变为「四键 + 自定义 claims 合并」。
- 两处本就是解码契约孪生（crypto.rs:60 注释），**必须同一次发布变更**；
  FFI 面是 JSON 字符串、vtable 形状不变，**无需 bump ABI**。
- **连带语义变化（进 CHANGELOG + 测试）**：`user.claims.tenant`
  （`serve/src/dispatch.rs:289-294` 已在读）将从恒失效变为真正生效——
  `tenant_require_signed_claim: true` 部署的既有行为受影响，必须显式登记。
- `jwt.sign` 是公共 JS API 扩展：进 api-manual 对应章节 + 限制表（ttl 上限）。

## P1：工具描述（源码层；overlay 后移）

- **白名单放行**：`description` 进 `SUPPORTED_KEYWORDS` + `SCHEMA_CHANNELS`
  （string-only，形态校验照 pattern 先例 contract.rs:366-377）。参数级与
  operation 级共用此放行。
- `.schema` 动词级 `description: string`（组级同理，优先级沿 `.schema.response`
  先例：动词 > 组）。`openapi.rs`：声明了 description 的 operation 在 spec 上
  **新增** `description` 键；`summary` 恒合成不变——字节只增不改（反目标 3）。
- `mcp.rs` `spec_to_tools`：description 选取 `description || summary`；
  单测钉死「no declared input contract」后缀仍叠加（架构师 N2）。
- **验收 smoke（PM S5）**：固定 5-10 条自然语言任务对 sample MCP 面做
  前后对比（人工跑），作为「描述变好→agent 命中率变高」假设的最小证据。

## P2：代理凭据（服务令牌 + 绑定 + OBO + 审计）

### 服务令牌

- `POST {base}/v1/api/admin/service-token`（admin role 门）：body
  `{name, scopes?}` → `jwt.sign({ sub: "svc:<name>", roles: ["bot"],
  claims: { svc: <name>, jti, scopes }, ttl_secs: 90d })`——**缺省
  scopes `["read"]`，可参数指定**；TTL 90d 常量（PM 建议，服务器钳制上限同值）。
  创建响应注明「token 仅此一次展示」。
- 登记 `oj_svc_token`（`jti PK, name, scopes, created_by, created_at`）。
- **运营面（PM S3）**：list / revoke 端点进 sample admin（谁名下哪些 bot、
  什么 scope 可见）；**吊销 = 删行，admit 级 jti 检查使删行全路径 ≤30s 失效**
  （见 P3），「秒级吊销」叙事成立。
- 零插件结构改动（claims 链路已在前置件解决）。

### 绑定表（架构师 S6 改形）

- `oj_bot_binding`：`(provider TEXT, external_sub TEXT) PK, user_id,
  last_used_at, created_by, created_at`——不焊死企微名词，将来接钉钉/飞书
  免迁移；`last_used_at` 供 admin 识别从未用过（疑似绑错）与活跃绑定
  （机器人工程师 S5）。
- admin CRUD + **CSV 批量导入导出**（体验客户条件③，v1 必备）。
- 绑错排查路径进 devkit（绑错=静默冒充，最坏失败模式必须可诊断）。

### `POST {base}/v1/api/auth/obo`

不进 anonymous_paths（正常 Bearer 门验签），handler 内：

1. 验 `http.user.claims.svc` 存在且 roles 含 `bot`；
2. 查 `oj_svc_token` jti 在表（吊销门，双保险——admit 已查，此处防御纵深）；
3. body `{provider, external_sub}`（v1 传 `{"wecom", <userid>}`）→ 查绑定表
   → **员工停用/不可用状态显式拒绝**（PM B1 补漏：v1 草案实时查 roles 时
   停用员工会照常换签）→ 取员工当前 roles（实时查——降权立即生效）；
4. **复制员工 `claims.tenant` 进新 token**（架构师 S2：否则
   `tenant_require_signed_claim` 部署下换签 token 全部 403）；嵌
   `act: { sub: "svc:<name>" }`；
5. `jwt.sign({ sub: user_id, roles, claims, ttl_secs: 600 })`——**10min 常量**；
6. **写换签审计表** `oj_obo_audit`（`id, svc, provider, external_sub, user_id,
   ok, reason, ts`）——「哪个 bot 何时为哪个员工换签、成没成」，
   admin 可查可导出（体验客户条件②最小集 + 架构师 S5 + 机器人 S5 三路汇合；
   横向移动的首要侦查信号）。
7. 返回 `{access_token, expires_in}`。机器人每条企微回调至多换签一次
   （回调内缓存，`expires_in - 60s` 余量——机器人工程师 S1 纪律）。

### obo 错误契约（机器人工程师 B1 + PM B1）

四种失败各配稳定业务 code（沿 sample 40xxx 语义分配，落 api-manual 错误表）：
**未绑定**（员工侧话术：联系管理员绑定）/ **jti 已吊销**（运维告警，不该答用户）/
**非 svc 凭据**（机器人配置错）/ **员工停用**（账号异常）。机器人 LLM 靠
code 决定给员工的回复，禁止对 msg 做字符串前缀匹配。

### 安全姿态声明（机器人工程师 S4，明示接受）

**svc scopes 不约束换签面**：只读机器人可换签出全量员工 token。接受依据：
`act` 审计可追、jti 删行 ≤30s 全路径失效、**绑定表 = 爆炸半径上界**（IT 精确
配到人）。可选交集 `min(svc scopes, 员工全量)` 记为后续（一个 if，缺省关）。

### 企微端到端交付物（PM B1）

- devkit `scenarios.md` 一篇完整场景：回调 → 取 userid → obo → MCP 调用 →
  回复；含企微三坑骨架（**5 秒 ack 后异步主动发消息 / MsgId 去重防重投 /
  回复 ~2048 字节截断摘要**）与 401 标准模式（见 devkit 节）。
- sample 给**最小 bot handler 骨架**（回调→obo→MCP 调用，独立示例目录）。
- 首次使用知情提示（体验客户⑦）：骨架内含「本次将以你的账号身份操作」回复话术。

## P3：scopes / act_mask（执行点：`dispatch::admit`）

### 执行点（v2 核心修订，架构师 B2a）

**不再在 mcp_http 三处复验**。下沉到 `dispatch::admit`（run_route 与
run_capture 共用，dispatch.rs:167/234-340）——一处覆盖 `/mcp`、`/rpc`、
直连 REST 三个面：

- **scopes**：`user.claims.scopes` 存在时按 verb→档位复验
  （`get`→read；`post|put|patch`→write；`delete`→destructive）；档位缺失 →
  403 业务信封（MCP 侧经既有 R3 映射自然变 isError）。scopes 缺席 → 不检查
  （存量 token 零破坏）。**豁免：`/auth/obo` 路径自身**（否则 read-only svc
  token 连换签都被拦，bootstrap 死锁）。**WS 镜像路由不走 admit——明示排除**
  （文档 + SKILL 陷阱）。
- **jti**：`user.claims.svc` 存在时查 `oj_svc_token`（**30s TTL 缓存**，进程内
  `RwLock<Option<(Instant, HashSet)>>`）；查表失败 fail-closed 拒绝。使
  「删行即失效」对全部路径成立（陈旧上限 30s，体验客户验收口径）。
  AppState 增挂 default-db handle（Option；`build_registries` 装配期注入）。
- **act_mask**：`server.mcp.act_mask_destructive: true` 时 `claims.act` 存在且
  verb==delete → 403。**放 admit 而非仅 mcp_http**（较架构师 N4 的假设更强）：
  obo token 直连 REST 的删除同样被挡，体验客户「经机器人不给删」的直觉在全
  面成立，SKILL 无需登记「为什么 REST 还能删」类工单。
- `tools/list` 裁剪退化为**展示层**（凭 scopes/act_mask 形状过滤，帮 LLM 省
  上下文）；权威执行点在 admit。**-32601 / isError 分层**（机器人工程师 S2）：
  call 侧解析在 modules 过滤后的**全集**上进行——modules 滤掉/未知 tool →
  -32601（对任何凭据都不存在）；scopes/act_mask 拒绝 → admit 403 信封 →
  isError（存在但不属于你）。

### isError 统一形状（机器人工程师 B2）

`capture_to_result` 三条路径统一为 `[code=<n>] <msg>` 文本前缀
（业务 code 保留进文本；structuredContent 原样）。机器人按 code 分类转述
（400=参数可自纠 / 403=无权转告 / 404=不存在），不再解析多种形状。

### 员工可见文案（体验客户①）

Rust 侧拒绝文案恒为稳定技术形状（`[code] msg`）；**人话转述是机器人的职责**
（LLM 依 code 生成），sample bot 骨架示范中文模板（如「您暂无权限执行
「删除订单」，请联系 IT 开通」）；obo（JS 端点）的失败 msg 本身即中文、
可按部署改写——**不引入 Rust 侧文案配置键**（每多一个键都是文档/测试负债，
PM N3 同源判断）。

## 配置面（增量）

```yaml
server:
  mcp:
    act_mask_destructive: false   # 新增；缺省关，文档标「推荐开启」，sample 示范 true
```

接入示例配置（botdev N2）：`server.mcp.modules` **排除 auth/admin**——
`auth/obo`、service-token、绑定 CRUD 混进 tool 面是员工会话噪音 +
prompt injection 社工面；devkit 示例直接带这份配置。

## devkit / 文档对齐（仓规红线）

- `api-manual.md`：jwt.sign 新签名与 ttl 上限、claims 透传语义（含 tenant
  生效变化）、obo 四错误码、svc-token 三端点、绑定 CRUD+CSV、scopes 三档
  映射与**「POST 按 write 档判定」限制**（PM S2：企业 admin 路由大量
  POST-as-query，read 机器人查报表被拒是首个客诉源；两条绕法：补 GET 变体 /
  给机器人签 write）+ e2e 钉住该行为。
- `scenarios.md`：企微端到端一篇（含 401 标准模式：**401 → 换签 → 单次重试 →
  再 401 视为凭据级故障告警**，防无界重试；svc 凭据死 vs 员工 token 死的区分）。
- `SKILL.md` 陷阱速查：WS 镜像不经 admit、线路 A 不受收窄、scopes 缺席语义、
  obo 10min、svc scopes 不约束换签面、act_mask 的 REST 覆盖（v2 已闭合，
  旧版假设作废）。
- **线路 A 限制声明（PM B2）**：P3 收窄/act_mask 仅线路 B；线路 A stdio 快照
  不裁剪、直回源。接入文档定位：**机器人接入一律走 B**；A 面向开发者本机
  （个人 token，不在机器人威胁模型内）。红线（不放大）在 A 上由 roles 兜底
  成立——诚实声明，不冒充全覆盖。
- tools/list 变化感知（机器人工程师 S3）：stateless 无 list_changed 通知，
  文档明示「勿跨会话缓存；每会话首轮回拉；可按 (scopes, act_mask) 形状缓存
  一份服务全部员工」。

## 交付顺序（v2：P3 与 P2 同批，消死代码窗口）

1. **前置件**：claims 全链路两端 + jwt.sign 扩展（独立发布点，tenant 变化进
   CHANGELOG）。
2. **P1 源码层**：白名单 + spec description + mcp 选取 + smoke 对比。
   （与 3 无依赖可并行。）
3. **P2+P3 同批**：svc-token/绑定/obo/审计表（JS+迁移）→ admit
   scopes/jti/act_mask（Rust 单点）→ mcp_http 展示层 + isError 前缀。
   scopes 签发入口（service-token）与执行点同批落地，无空转窗口。

每步照仓规：CHANGELOG + devkit 四件套 + `cargo xtask build` + devkit 契约测试。

## 测试

- 单测：claims 两端 roundtrip（含嵌套 act 对象、ttl 钳上限）；`spec_to_tools`
  description 选取 + 「no contract」后缀叠加；admit 档位映射表。
- 字节兼容：无 description 路由 `oj openapi --check` 对拍 v0.1.129 逐字节一致。
- e2e：
  - **claims 链路**：自定义 claims 经签发→守卫全链路可见（v1 草案在此静默失效）；
    tenant claim 生效后 `tenant_require_signed_claim` 行为矩阵。
  - **admit 执行点**：read svc token 经 `/mcp`、`/rpc`、**直连 REST** 三面 call
    delete/write 均 403；无 scopes 老 token 三面全量（零破坏）；obo 端点自身
    豁免；POST-as-query 行为钉死。
  - **jti**：删行后 30s 内三面全部失效；查表故障 fail-closed。
  - **act_mask**：开 → act 会话三面 delete 拒、员工直连不受影响；关 → 纯继承。
  - **OBO**：签发→换签→以员工角色命中/被拒；员工降权/停用后下一轮 obo 即拒；
    jti 删行拒绝；非 svc 凭据 403；四种失败 code 与 api-manual 一致；
    换签审计行落表；租户复制。
  - **绑定**：CRUD + CSV 导入导出 roundtrip；`last_used_at` 更新。
  - **错误形状**：isError 文本恒 `[code=<n>] <msg>`；-32601 仅 modules 滤掉。
  - **svc-token 运营**：list/revoke；一次性展示纪律（响应不含重复明文）。
- 指标（PM S6，全走日志零新系统）：P1 description 覆盖率 + smoke 前后对比；
  P2 obo 日换签数、绑定缺失错误占比、活跃 svc token 数；P3 scope 拒绝按档位
  分布。诚实条款：服务端测不了「机器人替员工办成事」，5-10 条固定任务 smoke
  列为每版验收动作。

## 后续迭代（设计保留，不在本期）

1. **描述覆盖层 overlay**（v2 裁决后移；触发条件：出现「不能改代码但必须改
   描述」的真实诉求）。v1 草案设计经评审修订后保留于此：
   db 表 `oj_mcp_desc_override`（default-db + 装配期 fail-fast，架构师 S4）；
   admin CRUD（**列表须是工作清单**：`{operation_id, effective_description,
   source: overlay|source|synthetic}`，PM S1——孤儿 join 因 JS 拿不到 spec
   集合而砍掉，架构师 S3，文档写「路由挪位请清理旧键」）；AppState db handle
   + 30s TTL；**是否配网页 UI 由触发时的真实用户定**（体验客户：没有真页面
   永远不用；PM：persona 是技术运维端点够）；覆盖时保住 no-contract 后缀
   语义（架构师 N2）；多 api 挂载无命名空间隔离记一句（N3）。
2. 员工自助绑定：**企微 OIDC 登录成功即自动 upsert 绑定**（PM S4，仓内
   OIDC OP/RP 现成）——运营归零的正解。
3. 全量调用流水表（act 会话每调用落行；v2 裁决暂不做）。
4. 路由级档位标记（`.schema` flag 覆盖方法映射；POST-as-query 根治）。
5. obo 交集 `min(svc scopes, 员工全量)`（一个 if，缺省关）。
6. OAuth 2.1 授权服务器 + 元数据发现（对外多租户自助接入时立项）。
7. admit 级收窄补 WS 镜像路由（如真实需求出现）。

## 我拍板的细节 v2（用户可复核）

1. claims 契约：自定义一切都在 JWT 的 `claims` 键内（`svc`/`jti`/`scopes`/
   `act`/`tenant`），守卫出口原样透传；ttl 钳制上限 = 90d。
2. svc token TTL 90d 常量（PM 建议）；obo 10min 常量；均不进配置。
3. jti admit 检查 30s TTL 缓存、fail-closed；豁免路径仅 `/auth/obo`。
4. act_mask 执行点放 admit（比架构师 N4 假设更强，全路径覆盖）。
5. 绑定表 `provider + external_sub`；`last_used_at`；CSV 导入导出 v1 必备。
6. Rust 侧拒绝文案不加配置键；人话转述归机器人（骨架示范中文模板）。
7. svc scopes 不约束换签面：明示接受（审计+秒吊+绑定=爆炸半径上界），
   交集记后续。
8. 交付顺序：claims 前置件 → P1 ∥ P2+P3。
9. overlay 全节后移（含孤儿打标砍掉、网页 UI 待真实用户定）。

## 评审记录（v2 修订依据）

- **builder-check**：claims 两端静默丢弃（B1）→ 前置件；/rpc 绕过（B2）→
  admit 下沉；svc TTL/吊销覆盖面（B3）→ 90d + jti 全路径。
- **架构师（打回）**：B1 同 claims；B2 → admit 单点（含 obo 豁免、WS 排除、
  AdmitOut 已带 user 改动更小）；B3 → 随 B2a 消解；S1 ttl_secs；S2 租户复制；
  S3 孤儿砍；S4 db 键；S5 换签审计；S6 绑定表改形；N1 白名单修正「零代码」；
  N2/N4 进测试与 SKILL。
- **产品经理**：B1 企微端到端交付物 + 停用员工拒绝；B2 线路 A 限制声明；
  B3 overlay 后移（用户裁决采纳）；S1 工作清单（随 overlay 后移）；S2
  POST-as-query 限制表；S3 svc 运营面；S4 OIDC 自动绑定（后续）；S5 顺序
  修订；S6 指标节；N1-N4 采纳。
- **机器人集成工程师**：B1 obo 错误码表；B2 isError 前缀统一；S1 401 标准模式；
  S2 -32601 分层；S3 列表缓存指引；S4 安全姿态声明；S5 绑定发现
  （last_used_at + 换签审计）；S6 租户；S7 sample 安全关键实现标注
  （devkit 规范实现 + 契约测试覆盖完整失败路径）；N1-N4 采纳。
- **体验客户**：①文案→机器人侧模板（裁决 6）；②审计→换签审计表（v2 裁决）；
  ③CSV 导入（v1 必备）；④页面→随 overlay 后移定；⑤反馈闭环/历史版本→
  overlay 触发条件内重审；⑥POST 误伤→限制表+e2e；⑦知情提示进骨架 +
  act_mask 推荐开启（v2 裁决）。
