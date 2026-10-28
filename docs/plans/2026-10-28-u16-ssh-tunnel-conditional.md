# U-16 服务端 SSH 隧道（条件项）设计稿

> 状态：**设计稿（条件未触发——本文件是「立项申请书」，不是实施计划；§2 触发条件被推翻才动工）**。
> 上游：`docs/prds/oj-feature-requirements.md` §5 `U-16`（「仅当产品取舍被推翻才立项」）。
> 相邻：U-7（client 轴配额——隧道是连接数预算的一部分）；凭据密封（`ENC[...]`，v0.1.33——
> 私钥/口令只能以密封值进出）。

## 0. 一句话

数据库客户端（dbx Web）的用户在跳板机后的库上点「测试连接」时，若产品坚持「客户端直连」，
这类库永远连不上——SSH 隧道是把「拨号」这一跳挪到 **oj 服务端**（服务端 `ssh -L` /
rust 隧道库 → 跳板机 → 目标库），客户端仍然只触达 oj。

## 1. 触发条件（立项门）

满足其一才动工，否则本文件保持「备查」：

1. **产品裁决推翻「客户端不触达跳板机后的库」**（PRD 原条件）——即明确要支持跳板机场景；
2. ≥ 2 个真实部署方报「无隧道不可用」且愿意出环境做验收（避免为想象需求造进程管理面）。

## 2. 形态（触发后的设计骨架）

| 项 | 设计 |
|---|---|
| 参数面 | `db.client.register(name, dsn, { tunnel?: { jumpHost, jumpPort?, user, auth: { passwordEnc? / keyEnc? / keyPath? }, targetHost?, targetPort? } })`——auth 字段只收 **`ENC[...]` 密封值**（明文拒收）；`targetHost/Port` 缺省从 dsn 解析 |
| 隧道实现 | **首选 rust 进程内**（`russh`/`tokio` 库，本地监听 `127.0.0.1:<随机端口>` → 跳板机 → 目标）；**备选**派生 `ssh -L -N` 子进程（零新依赖，但进程生命周期/僵尸回收/信号面更脏）——评审定夺 |
| 注册表 | 隧道句柄挂在 `ClientDbEntry`（`tunnel: Option<TunnelHandle>`）；生命周期与 entry 绑定（delete/reRegister 拆链） |
| 配额 | 每隧道占 1 个 `max_streams` 之外的独立预算 `tunnel_max`（缺省 = `max_entries`）；拨号守卫（U-7 锁退）对跳板机连接同样生效 |
| 审计 | `MGMT_AUDIT`（tunnel_up/tunnel_down，含 jumpHost/target **不含凭据**）；失败 `MGMT_DENY` |
| 安全红线 | ① auth 三态只收密封值；② `known_hosts` 校验策略显式化（`accept_new` 缺省 + 文档警示，`strict` 可选）；③ 本地监听只绑 `127.0.0.1`；④ argv 直传（若走子进程，零 shell 拼接）；⑤ 隧道是**登记者的资源**：owner 之外不可见不可用 |

## 3. 不做（划界）

- 反向隧道 / 动态 SOCKS / 端口段转发——只做「一库一隧道」定向转发。
- 隧道高可用（跳板机故障自动漂移）——down 即 `MGMT_DENY` + 手动 reRegister。
- 客户端本地隧道方案（那是部署文档的事，不是平台能力）。

## 4. 若立项的分期（预排）

| 批 | 内容 | 估 |
|---|---|---|
| T1 | 参数面 + 密封值接入 + rust 隧道库 spike（连一台真实 sshd 的验收环境） | 2 天 |
| T2 | 生命周期（up/down/reRegister 拆链）+ 配额 + 审计 + global.d.ts | 2 天 |
| T3 | 文档 + scenarios 场景 53 + devkit 对齐 + 发版 | 1 天 |

## 5. 待评审问题（触发立项后才走四路）

| 路 | 问题 |
|---|---|
| 安全 | rust 库 vs `ssh` 子进程；`known_hosts` 缺省策略（`accept_new` 的 TOFU 风险是否可接受） |
| 兼容 | 隧道参数进 `register` 的第三参——与既有二参调用兼容（缺省 None），无破坏 |
| 性能 | 每隧道一跳的延迟/吞吐损耗是否需要文档给预期 |
| 产品 | 触发条件本身（§1）是否成立——这是本文件唯一真正的问题 |
