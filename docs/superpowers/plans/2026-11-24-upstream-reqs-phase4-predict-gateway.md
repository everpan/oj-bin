# REQ-PREDICT Phase 4：预测执行与合规网关 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 平台提供 `predict.run({pql, dataScope, consent}) → {result, trace}` 受控执行面：dataScope 越界拒、consent 未过不执行且拒绝也留审计、引擎可插拔（内置确定性 mock 引擎离线可测）、全程结构化可观测——上游 REQ-A4 四条之后的第四条 P2 需求（§3.2）。

**Architecture:** 新增第一方 cdylib 插件 `plugins/oj-predict`，经泛型轴 `predict` 暴露（零 ABI 变更，oj-ldap 先例）；网关三闸（dataScope 白名单 ⊆ 校验 / consent 授权表校验 / 审计经 HostContext log）全部插件侧实现，宿主只提供通道与配置透传。**引擎契约 + 确定性 mock 引擎随包**；外部引擎（KumoRFM 等）适配器由部署侧按契约自备插件实现——上游明确「具体形态由上游裁定」，平台不瞎猜外部产品 API。

**Tech Stack:** Rust cdylib（oj-plugin-ffi 契约 + 泛型轴）、零新增依赖、serde_json。

**Spec:** `docs/UPSTREAM-REQS.md` §3.2（REQ-PREDICT 五条验收）；**合规评审为前置门槛**（G2）——本计划 Review Focus 第 1-2 条即合规红线，评审循环加合规专项一路。

## Global Constraints

- 版本分界递增 `oj/Cargo.toml` → **0.1.145**（本阶段发布点，未打标签待确认）。
- devkit 四件（api-manual / scenarios / SKILL / README）与本版用户可见变更逐条对齐（CLAUDE.md 红线）。
- 测试实例凭据只走 env，不进仓库；`--release` 串行跑（禁并发两个 cargo test；管道 `set -o pipefail`）。
- **数据出境红线**：模块侧不得有绕过网关的外呼路径——平台交付面只有 `axis("predict")`；文档明示「handler 内自建 fetch 外呼预测服务 = 绕过合规治理」（REQ-PREDICT 背景第 2 条原文约束）。
- 泛型轴名 `predict` 不撞 9 个保留名（es/db/blob/bus/kv/auth/mq/mail/ldap）。
- 审计不留库：审计走宿主日志槽位（结构化行），不新建表/端点——保持「插件无 DB 访问面」的既有架构边界。

## REQ-PREDICT 验收标准 → 本阶段交付对照

| 上游验收 | 平台侧交付 |
|---|---|
| 1. PQL 由平台受控引擎执行 | 泛型轴 `predict` 是唯一执行入口；内置确定性 mock 引擎实证全链（离线可测）；外部引擎按契约接入点在 config `engine` 段（适配器自备） |
| 2. dataScope 越界被拒 | 网关白名单校验：声明面 ⊄ `policy.allowed_sources` → 整体拒绝（fail-closed，不静默裁剪）+ 结构化 `OJ_PREDICT_SCOPE` + 审计 |
| 3. consent 未通过不执行且留审计 | 授权表校验（grant 存在/未过期/scopes 覆盖）→ `OJ_PREDICT_CONSENT` + **拒绝同样落审计行** |
| 4. 结果可回传 ReAct 作工具输出 | 同步 Promise 返回纯 JSON `{result, trace}`（BigInt-safe 序列化，axis 通道既有语义） |
| 5. 失败/超时可观测、不污染租户数据 | `timeout_ms` 有界 + 引擎错误结构化 `OJ_PREDICT_ENGINE`（消息脱敏）+ trace.status 落 failure + 审计；引擎契约=只读预测，无租户写路径 |

## Review Focus

1. **dataScope 部分越界静默裁剪**（合规红线）——声明的源列表一条越界，网关把越界项剪掉继续执行 = 数据出境未授权。期望：任一条越界即整体拒绝，错误点名越界项。pin 进网关校验测试。
2. **拒绝路径不落审计**（合规红线）——consent 失败直接 return，审计只记成功 = 拒绝不可观测，攻击面探测无痕迹。期望：deny 与 error 路径同样落审计行（含原因码）。pin 进网关测试。
3. **租户数据经错误消息外泄**——引擎把回显的 PQL/数据片段拼进错误文案。期望：引擎错误消息过脱敏（复用 errmap::redact 同款 `://***@` 口径），PQL 原文不进错误信封（只进 trace.pql_hash 或截断）。pin 进插件单测。
4. **超时半途的 trace 谎报 success**——引擎超时后 trace.status 仍是 ok。期望：超时收 `OJ_PREDICT_ENGINE`（或专码）+ trace.status=timeout。pin 进 mock 引擎慢响应腿。
5. **未装插件时的可用性口径**——`axis("predict")` 对 unknown 轴报错并列可用轴（既有通道语义），文档写明插件缺席 = 能力缺席而非静默降级。pin 进宿主装配测试（无插件时 ojInfo().generic_axes 不含 predict）。

---

### Task 1：网关校验核（纯函数 + config 段结构）

**Files:**
- Create: `src/bridge/predict.rs`（校验核 + `PredictSection` 结构）
- Modify: `src/config.rs`（顶层 `predict:` 段字段 + 已知段名清单）
- Modify: `src/bridge/mod.rs`（`pub mod predict`）
- Test: `src/bridge/predict_tests.rs`

**Interfaces:**
- Produces（Task 2/3 消费）：
  ```rust
  pub struct PredictSection {           // config `predict:` 段，全部 serde(default)
      pub axis: String,                 // 收编的泛型轴名，默认 "predict"
      pub timeout_ms: u64,              // 引擎调用上界，默认 15000
      pub policy: PredictPolicy,
  }
  pub struct PredictPolicy {
      pub allowed_sources: Vec<String>, // 形如 "db:<name>.<table>" 或 "db:<name>.*"
      pub grants: Vec<ConsentGrant>,
  }
  pub struct ConsentGrant {
      pub id: String,
      pub scopes: Vec<String>,
      pub not_after_ms: Option<u64>,    // unix 毫秒；None = 永不过期（零时间解析依赖）
  }
  pub fn validate_scope(sources: &[&str], allowed: &[String]) -> Result<(), Vec<String>>;
  // 任一条不命中（精确或 db:<name>.* 通配）→ Err(越界项全列)；fail-closed 不裁剪
  pub fn validate_consent<'a>(grant_id: &str, scopes: &[&str], grants: &'a [ConsentGrant], now_ms: u64) -> Result<&'a ConsentGrant, ConsentDeny>;
  pub enum ConsentDeny { NoSuchGrant, Expired, ScopesInsufficient }
  ```
- config 段挂 `src/config.rs` L1466 `Config` struct（`pub predict: PredictSection`，`#[serde(default, deserialize_with = "null_as_default")]` 同 audit 段形态）+ 段名清单 L1625 处加 `"predict"`。

- [ ] **Step 1：写失败测试**（合规矩阵全覆盖）：部分越界整拒（`["db:a.t1","db:b.t2"]` vs allowed `["db:a.*"]` → Err 含 `db:b.t2` 且**不含**裁剪语义）；精确命中过；`db:name.*` 通配命中；未知 grant / 过期（now > not_after_ms）/ scopes 不足三拒各一；grants 空 = 全拒。
- [ ] **Step 2：跑失败**：`cargo test --release -p only-js predict`。
- [ ] **Step 3：实现** predict.rs 三结构 + 两校验函数（通配匹配 ~6 行：strip 前缀 `db:` 后按 `.*` 尾通配；时间比较纯 u64）。
- [ ] **Step 4：测试绿 + 全量 `cargo test --release`（根 crate）**。
- [ ] **Step 5：提交** `feat(predict): 网关校验核——dataScope 白名单 fail-closed + consent 授权表`

### Task 2：宿主网关全套（op + 门面 + 收编封堵 + 审计）——不可分片

> 收编分流**必须**与网关同一提交：先落网关后补封堵会留下「模块可直连引擎」的中间态（合规红线不可分片）。

**Files:**
- Modify: `src/bridge/predict.rs`（`op_predict_run` op：校验 → audit → dispatch 收编轴 → trace）
- Modify: `src/bridge/mod.rs`（op 注册进 `bridge_ext`）
- Modify: `src/bridge/bootstrap.js`（`globalThis.predict = { run }` 门面，**7-bit ASCII 红线**）
- Modify: `src/bridge/runtime.rs` / StableState 定义处（`predict_axes` 注册表，装配期一次注入，**首次 run 前红线**）
- Modify: `oj/src/serve_cmd.rs`（`build_registries` 泛型轴分流加 predict 臂——mgmt 收编先例 L1868-1884 逐行照抄：`axis == cfg.predict.axis` → `predict_generic`，**不进公开 `generic` 表**）
- Test: `oj/tests/e2e.rs` 新腿
- Create: `tests/plugins/mini-predict/`（echo 引擎夹具，workspace member + 根 Cargo.toml members 表）

**Interfaces:**
- Consumes: Task 1 两校验函数；`audit::emit(&st.audit, kind, &owner, "predict", &detail)`（mgmt.rs L157 同款，Owner 从 http 上下文提取）；`GenericVtable.call(op, args)` dispatch（同 `generic_axis::call_axis` 形态）。
- Produces:
  - JS：`await predict.run({ pql: string, dataScope: { sources: string[], max_rows?: number }, consent: { grant_id: string, scopes: string[] } }) → Promise<{ result: object, trace: object }>`
  - 错误（JS throw，宿主信封 code）：`OJ_PREDICT_SCOPE`（data.violations 列越界项）/ `OJ_PREDICT_CONSENT`（data.reason: no_such_grant|expired|scopes_insufficient）/ `OJ_PREDICT_ENGINE`（引擎错误/超时/插件缺席；消息过 redact 脱敏，**不含 PQL 原文**）
  - trace：`{ id, status: ok|denied|error|timeout, engine_ms, grant_id, sources, ticket }`——**无 PQL 文本**，审计 detail 只带 `pql_sha256` 前 12 hex
  - audit kinds：`PREDICT_AUDIT`（ok/error/timeout，detail.status 区分）/ `PREDICT_DENY`（scope/consent 拒绝，**拒绝必留痕**）
  - 引擎 op 契约（Task 3 插件侧实现）：`call("run", '{"pql":…,"dataScope":…,"ticket":…}')` → JSON `{"result":…, "ticket":…}`；ticket = 网关每调用随机生成（`gt-<hex16>`），插件回显供 trace 校验
- mini-predict 夹具（照抄 mini-generic）：轴名 `predict`，op `run` echo 引擎（回 `{result:{echo_pql_len, sources}, ticket}`）+ op `sleep`（睡 args[0].ms，供 timeout 腿）。

- [ ] **Step 1：写失败 e2e**（oj/tests/e2e.rs，仿 mini-generic 泛型轴腿 L1741 先例：`cargo build -p oj-plugin-test-mini-predict` 现场编译 + 插件扫描装配 + JS handler 断言）——九子用例：run ok（result+trace 形状 + ticket 回显一致）/ 部分越界整拒（violations 点名）/ consent 未知 grant / consent 过期 / timeout（夹具 sleep 5000 > timeout_ms 300 → trace.status=timeout）/ 引擎 Err 脱敏（PQL 原文不在错误消息）/ `audit.query` 可查 PREDICT_DENY / **`axis("predict")` 直连必抛 unknown axis** / `ojInfo().generic_axes` 不含 predict。
- [ ] **Step 2：跑失败**。
- [ ] **Step 3：实现**：build_registries 收编分流 + StableState.predict_axes 注入（装配期）+ op_predict_run（校验→deny audit→dispatch 收编表→timeout→trace）+ bootstrap 门面（predict 恒在：插件缺席时 run 报 `OJ_PREDICT_ENGINE`「predict 引擎插件未装配」——能力缺席非静默，Review Focus 5）。
- [ ] **Step 4：测试绿 + 根 crate 全量回归**。
- [ ] **Step 5：提交** `feat(predict): 宿主合规网关——dataScope/consent/审计三闸 + 引擎轴收编封堵 + predict.run 门面`

### Task 3：真插件 oj-predict（内置确定性 mock 引擎）

**Files:**
- Create: `plugins/oj-predict/`（Cargo.toml cdylib + src/lib.rs）
- Modify: 根 `Cargo.toml` members + `tools/xtask/src/main.rs` PLUGINS 表
- Test: `plugins/oj-predict/src/lib_tests.rs`（照 oj-ldap 形态）

**Interfaces:**
- Consumes: Task 2 引擎 op 契约（`run` + ticket 回显 + `sleep`）；`oj_plugin_entry!(init, config: "predict", generic(predict) => &VT)`（oj-ldap L107 同款；descriptor name = `predict`，**插件名非 crate 名**——oj-ldap L117 测试钉死先例）。
- Produces: mock 引擎 op `run`——确定性预测（无外呼无租户数据）：`{"result":{"engine":"mock","rows":[{period,value}…]（sha256(pql) 播种的线性趋势，args.horizon 条数，缺省 3）},"ticket":<回显>}`；op `sleep`（`{"ms":n}` 睡 n 毫秒回 `{}`，供宿主 timeout 腿真插件复验）；未知 op → ready_err 列合法 op。

- [ ] **Step 1：写失败单测**（插件 crate 内）：run 形状 + 确定性（同 pql 两次同值）+ ticket 回显 + 未知 op 报错文案。
- [ ] **Step 2：跑失败**。
- [ ] **Step 3：实现**（照抄 oj-ldap 骨架：catch_future + ENGINE OnceLock；mock 引擎 ~40 行：sha256 伪随机趋势 + horizon 截断）。
- [ ] **Step 4：绿 + `cargo xtask plugin oj-predict --check` 预检过**。
- [ ] **Step 5：提交** `feat(oj-predict): 泛型轴插件——内置确定性 mock 预测引擎（外部引擎适配器由部署侧按契约自备）`

### Task 4：发布 ritual（devkit 四件 + 手册 + CHANGELOG + bump 0.1.145 + 门禁 + 评审）

**Files:**
- Create: `docs/plugins/predict.md`（插件手册：网关三闸语义 / config `predict:` 段参照 / 引擎契约 + mock / 外部引擎自备指引 / 合规边界声明——「handler 自建 fetch 外呼预测服务 = 绕过合规治理」红线原文 + 数据出境声明）
- Modify: `docs/devkit/api-manual.md`（`predict.run` 全条目 + §13 限制表）/ `SKILL.md`（陷阱 2 行：收编不可直连 axis / grants 静态表非交互 consent）/ `scenarios.md`（场景 85：合规预测执行照抄例）/ `README.md`（能力条款 + v0.1.145 段）
- Modify: `CHANGELOG.md`、`oj/Cargo.toml` + `Cargo.lock`（0.1.145）

- [ ] **Step 1：docs 四件 + 插件手册逐条对齐**（错误文案与实现逐字一致；场景 85 用 mock 引擎离线可照抄）。
- [ ] **Step 2：CHANGELOG + bump 0.1.145**（未打标签，tag 等用户确认）。
- [ ] **Step 3：全门禁（串行）**：`cargo test --release -p only-js`、`-p oj`、`-p oj-predict`、`-p xtask`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo fmt --check`。
- [ ] **Step 4：提交** `chore(release): v0.1.145 版本分界——REQ-PREDICT 预测网关文档与 CHANGELOG`
- [ ] **Step 5：评审循环**：架构师 + 工程师 + 测试工程师（BDD 对照）+ **合规专项评审（G2 hard gate：任何绕过网关的数据出境路径 / 拒绝与失败路径审计完整性 / 凭据与租户数据泄漏面）** → must/should-fix 修订 → 计划文档偏差与裁决记录 → 向用户分阶段总结。

### 后续任务（不在本计划）

- 外部引擎（KumoRFM 等）适配器插件：部署侧按 Task 2 引擎契约自备（上游裁定具体形态）；oj 不随包、不猜测外部产品 API。
- 交互式 consent 流（用户在线授权页）：上游 G2 合规面裁定后另立需求；v1 为部署期静态授权表（grants），文档如实声明。
- 平台级 fetch 出站白名单（REQ-PREDICT 背景第 2 条的完整闭环）：独立安全需求，另立计划。

