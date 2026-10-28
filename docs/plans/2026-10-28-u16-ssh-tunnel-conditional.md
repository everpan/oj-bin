# U-16 服务端 SSH 隧道（条件项）设计稿

> 状态：**设计稿（三路专家评审完成——产品路裁决：不立项，维持备查；处置表见 §6）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-16`（「仅当产品取舍被推翻才立项」）。
> 相邻：U-7（client 轴配额）；凭据密封（`ENC[...]`，v0.1.33——**密封值只在 config 装载期
> 解封**，运行期无 open 路径，见 §3 依赖）。

## 0. 一句话

数据库客户端（dbx Web）的用户在跳板机后的库上点「测试连接」时，若产品坚持「客户端直连」，
这类库永远连不上——SSH 隧道是把「拨号」这一跳挪到 **oj 服务端**（服务端 → 跳板机 → 目标库），
客户端仍然只触达 oj。

## 1. 触发条件（立项门，评审加严）

满足其一才动工，否则本文件保持「备查」：

1. **产品裁决推翻「客户端不触达跳板机后的库」**（PRD 原条件）；
2. **≥ 2 个真实部署方报「无隧道不可用」且已按部署级隧道文档操作仍不可用**，且愿意出环境
   做验收（评审原为「报不可用即可」——产品路加严：部署级隧道是零代码首选解，必须先排除
   「只是没配对」的情形）。

**动工前置（零成本项）**：先交付一篇**部署级隧道 recipe 文档**（ssh -L / socat sidecar 配方）
进 devkit，兜住大多数「连不上」诉求——这一步不依赖立项。

## 2. 形态（触发后的设计骨架，评审修订后）

| 项 | 设计 |
|---|---|
| 参数面 | `db.client.register(name, dsn, { tunnel?: { jumpHost, jumpPort?, user, auth: { passwordEnc? / keyEnc? / keyPath? }, targetHost?, targetPort? } })`——auth 字段只收 **`ENC[...]` 密封值**（明文拒收）；`targetHost/Port` 缺省从 dsn 解析 |
| 隧道实现 | **首选 rust 进程内**（`russh`/`tokio）；**备选**派生 `ssh -L -N` 子进程（零新依赖）——评审裁决：russh 会给根 crate 加 TLS/crypto 重依赖，**两案合并进 T1 spike 一起裁**；若走 rust 库，隧道独立成 `src/bridge/tunnel.rs` 模块并评估 feature 门（不进 `client_db.rs`） |
| 注册表 | 隧道句柄挂 `ClientDbEntry`（`tunnel: Option<TunnelHandle>`）；生命周期与 entry 绑定（delete/reRegister 拆链）——**拆除连带面复用 reRegister 既有先例**：`tx_sessions.discard_alias` + 在途 stream abort（`client_db.rs:528/581`） |
| 配额 | 独立预算 `tunnel_max`（缺省 = `max_entries`）+ **per-owner 隧道上限**（架构路：与红线⑤「隧道是登记者的资源」一致，缺 `max_per_owner` 即与 U-7 纪律矛盾）；拨号守卫（U-7 锁退）对跳板机连接同样生效 |
| 审计 | **`DB_CLIENT_AUDIT` / `DB_CLIENT_DENY`**（架构路修正：隧道挂 client 轴，用 client 轴既有 kind——原稿误用 MGMT_*，既错轴又对未立项的 U-15 形成依赖），含 jumpHost/target、**不含凭据** |
| 安全红线 | ① auth 三态只收密封值；② `known_hosts` 校验策略显式化（`accept_new` 缺省 + 文档警示，`strict` 可选）；③ 本地监听只绑 `127.0.0.1`；④ argv 直传（若走子进程，零 shell 拼接）；⑤ 隧道是**登记者的资源**：owner 之外不可见不可用 |

## 3. 依赖（评审补充）

- **U-10 前提 1/2**：密封值运行期解封（公钥装配期加载 + seal/open 面）——本项 auth 只收
  `ENC[...]` 的红线**隐式依赖**它们；立项时一并评估（现状：密封值只在 config 装载期解封）。
- T1 参数面若在 U-10 落地前动工，auth 字段只能先落 keyPath 形态（文件路径不经运行期解封）。

## 4. 若立项的分期（评审修订预排）

| 批 | 内容 | 估 |
|---|---|---|
| T1a | rust 库 vs ssh 子进程 spike（**出口标准：对真实 sshd 完成一次 up → 查询 → down 闭环**） | 2 天 |
| T1b | known_hosts/TOFU 策略定夺 + auth 密封值接入评估（依赖 §3） | 1 天 |
| T2 | 生命周期（up/down/reRegister 拆链 + 拆除连带）+ 配额（全局 + per-owner）+ `DB_CLIENT_*` 审计 + global.d.ts | 2 天 |
| T3 | 文档 + scenarios 场景 53 + devkit 对齐 + 发版 | 1 天 |

## 5. 不做（划界）

- 反向隧道 / 动态 SOCKS / 端口段转发——只做「一库一隧道」定向转发。
- 隧道高可用（跳板机故障自动漂移）——down 即 `DB_CLIENT_DENY` + 手动 reRegister。
- 客户端本地隧道方案（部署文档的事；见 §1 动工前置的 recipe 文档）。

## 6. 评审处置表（三路 4 条：采纳 4、未采纳 0）

| # | 来源 | 原意见要点 | 处置 |
|---|---|---|---|
| 1 | 产品 | **裁决：不立项，维持备查**——部署拓扑问题首选部署级隧道（零代码）；触发条件 2 加严（先按部署文档操作仍不可用 + 出验收环境）；先交付部署 recipe 文档兜底；原稿红线清单合格、封存即可 | **采纳**（§1 加严 + 动工前置） |
| 2 | 工程 | auth「只收 ENC」与 U-10 死结（运行期无 open 路径）；拆除顺序复用 reRegister 连带面 | **采纳**（§3 依赖 + §2 拆除行） |
| 3 | 架构 | 审计 kind 用 DB_CLIENT_*（隧道挂 client 轴，误用 MGMT_* 对未立项的 U-15 形成依赖） | **采纳**（§2 审计行改写） |
| 4 | 架构 | 补 per-owner 隧道上限（红线⑤自洽）；russh 重依赖 → tunnel.rs 独立模块 + feature 门，并入 T1 spike 裁决 | **采纳**（§2 配额/实现行 + §4 T1 拆分） |
