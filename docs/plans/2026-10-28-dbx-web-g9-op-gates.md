# G9 服务端防护门 实施计划（v0.1.99）

> 需求原文：`docs/prds/oj-feature-requirements.md` §G9；验收判据：不经任何方案包 handler、
> 直接打 op 也拦得住；错误信封 `session_disposition` 语义正确。

**Goal**：三条防护门各自独立在 op 层生效——① 只读会话位（`db.session.begin({readOnly:true})`
→ 该会话上 Write/Ddl 一律拒）；② 写拦截即①的 op 层强制（attach 路径与 `db.session.query/exec`
双入口）；③ 危险语句确认门（`db_access.<alias>.dangerous_confirm_token` 设 token 后，危险语句
须请求头 `x-oj-confirm` 匹配，放行即审计）。

**Architecture**：全部 host 侧。①② 复用既有 risk 分类（`db_policy::classify`）与会话双入口
（attach 的 `query_impl/exec_impl` + `run_attached` 的两个显式 op）；错误用 `BridgeError`
（`session_disposition=Keep`，deno_error 机制挂 `e.sessionDisposition`）——G9 验收的信封语义。
③ 落在 `check_access_intent`（enforce=off 的早退**之前**——显式配了 token 就必须生效），
头匹配大小写不敏感；放行的每条危险语句落 `DB_DANGEROUS_CONFIRM` 审计事件（U-9 单一出口）。

**范围裁决**：
- 「连接级」落到**会话级位**：真正的连接级 read_only（sqlite `mode=ro` / mysql `SET SESSION`
  / pg `default_transaction_read_only`）在 sqlx 池与插件供建连参数里，动它要碰插件/ABI——
  已由 db_access 的文本级 read_only（v0.1.89，op 层先拒）+ 本期会话位双层覆盖，连接级硬保证
  登记 db-guide 为「DB 账号 GRANT 仍是最终防线」（与 L1 既有口径一致）。
- `readOnly` 位 begin 时定死、**不可变**（「位」的语义；要写就开新会话）。

## 决策表

| 项 | 落点 | 值 |
|---|---|---|
| `db.session.begin(opts?)` | `opts.readOnly`（缺省 false；名字由 `DB(name)` 绑定）；`TxSessionEntry.read_only`；summary 透出 | — |
| 会话只读门（显式 op） | `op_db_session_query/exec` 入口 `gate_session_sql` | risk ∈ {Write, Ddl} 拒 |
| 会话只读门（attach 路径） | `db.rs` query_impl/exec_impl 的 `forbid_in_session` 旁 `readonly_gate` | 同上 |
| 错误信封 | `BridgeError{category: Unsupported, disposition: Keep, retryable: false}` | `e.sessionDisposition == "keep"` |
| `dangerous_confirm_token` | `DbAccess` 新字段（serde 缺省 None；空串装配期拒）；运行期 grant 不覆盖 config 安全位（token 只从 config 来） | 头 `x-oj-confirm` 大小写不敏感匹配 |
| 确认门生效点 | `check_access_intent`，在 `enforce==Off` 早退**之前**（显式配 token = 要它生效） | `c.dangerous && token 已配` 才查 |
| 放行审计 | kind `DB_DANGEROUS_CONFIRM`（audit::emit 单一出口） | 每条确认语句一条 |

## 测试锚（Review Focus）

1. 只读会话：attach 后 `db.exec("insert …")` 直发 op（不经 handler 包装语义）被拒；错误含
   `is read-only`；`e.sessionDisposition === "keep"`；SELECT 照常；`db.session.exec(id, insert)` 同拒。
2. 非 readOnly 会话写照常（回归锚：位缺省 false 不改既有行为）。
3. 确认门：配 token 后危险语句（无 WHERE 的 UPDATE）——无头拒、错头拒、对头放行、
   放行落 `DB_DANGEROUS_CONFIRM` 且 `audit.query` 可检索。
4. token 只配在 config：运行期 `db.access.grant` 后 token 仍然生效（防运行期绕过）。
