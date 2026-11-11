# MCP 三痛点（描述/代理凭据/scopes）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 oj MCP 面交付三件事：`.schema` 源码层工具描述、JWT claims 全链路 + 企微机器人 OBO 代理凭据（含换签审计）、admit 单点 scopes/act_mask/jti 收窄。

**Architecture:** claims 链路是前置件（crypto.rs 签发端 flatten 透传 + oj-auth 守卫端同形解出）；描述走既有 `RouteTable→spec→tools` 链（纯增量键）；权限收窄**单点下沉 `dispatch::admit`**（async 化，覆盖 /mcp、/rpc、直连 REST 三面），`tools/list` 裁剪退化为展示层；业务端（svc-token / 绑定 / obo / 审计）全部是 sample JS 端点 + 迁移表，零 FFI 变更。

**Tech Stack:** Rust（deno_core / axum / jsonwebtoken）、TS（sample 业务端点，`db`/`jwt`/`bcrypt`/`json` 全局）、sqlite/mysql/pg 迁移。

**Spec:** `docs/superpowers/specs/2026-11-11-mcp-desc-cred-scopes-design.md`（v2，已批准；本计划从它论证，执行者两份都读）

## 任务状态跟踪（执行者每完成/阻塞一个任务必须更新本表）

| Task | 名称 | 状态 | 小结 |
|---|---|---|---|
| 1 | crypto.rs claims+ttl 签发端 | 完成 | dbc9502；4 新用例；JWT_TTL_CAP_SECS=90d 钳制 |
| 2 | oj-auth 守卫端 claims 透传 | 完成 | c8841c7；flatten 字段孪生；旧 token 键集不变；12/12 |
| 3 | contract.rs description 白名单 | 完成 | f6edee4；顶层+属性级 string-only，非串 fail-fast；1119/1119 |
| 4 | openapi/mcp description 透传 | 完成 | 22b815c；232/232；md5 基线全等（字节零漂移） |
| 5 | sample 迁移四表 + users.disabled | 完成 | 92bcfea；sqlite 幂等实证 + pg/mysql scratch 方言腿通过；users 归 _platform |
| 6 | auth/obo 端点 + 换签审计 | 完成 | 4892903；六路实测全对（含 /me 解 act/tenant/600s）；oj-auth bump 0.1.1（4d665b3，用户约定：改插件必 bump） |
| 7 | admin 运营端点（svc-token/绑定/审计） | 完成 | 3e7b00b；实测全过（revoke→obo 40311 闭环）；多路由=组对象+组 .route |
| 8 | admit async 化 + scopes/jti/act_mask 门 | 完成 | 53eac73；svc_gate.rs（TTL 30s/fail-closed）+ 三检查冻结序 + /auth/obo 豁免；serve 239/only-js 1119/oj 311+e2e 60 全绿；补 admin/manifest.yaml 三表（S005） |
| 9 | mcp_http 展示层裁剪 + isError 前缀 | 完成 | 959c1e2；filter_tools_for_display 与 admit 同口径（只裁展示面）；[code=<n>] 前缀两面统一；serve 243 全绿 |
| 10 | e2e 全矩阵 | 完成 | e09d264；e2e_svc 4/4（desc 透传/scopes 三面/obo 全矩阵/tenant 复制）+ e2e 60/60 零回归；勘误：插件每进程单例 → 拆 e2e_svc.rs + tests/common；/mcp+/rpc 端点级 scopes 豁免（传输动词非业务动词） |
| 11 | 配置示范 + devkit 四件套 + CHANGELOG | 完成 | a485505；oj 0.1.130 + 四件套对齐（api-manual/scenarios 场景 75+指标口径/SKILL 九行/README）+ sample mcp 示范段 + bot 骨架；xtask 10/10 + smoke + e2e 60/60 + e2e_svc 4/4 全绿；插件 bump 复核仅 oj-auth 0.1.1 |

## Global Constraints

- 一律 `--release`（仓规禁 debug）；**禁止并发跑两个 cargo test**；接管道须 `set -o pipefail`。
- 测试注释用仓内 BDD 三行式：`// Given … // When … // Then …`。
- 全部单元/集成测试目标：根 crate `-p only-js`、`-p serve`、`-p oj-auth`、e2e `-p oj --test e2e`、devkit `-p xtask`。
- tokio 测试用 `#[tokio::test(flavor = "current_thread")]`；条件等待一律 deadline/有界轮询，禁固定 sleep。
- 不动 ABI（`oj-plugin-ffi` vtable 形状零变更）；`bootstrap.js` 不触碰。
- 版本号：本批整体发 **v0.1.130**（CHANGELOG 于 Task 11 统一登记；commit message 带 `v0.1.130` 前缀说明可无版本号，但 Task 11 必须有）。
- commit message 尾行固定：`unix@vip.qq.com ai`。
- 新增公开错误文案（英文技术形状）一旦落码即冻结（botdev B1/B2：机器人按 code/形状分类，禁止改措辞破坏集成者）。
- 保留字：JWT 自定义 claim 禁用键 `{sub, roles, iat, exp}`（Task 1 fail-fast）；scopes 词汇仅 `{read, write, destructive}`；豁免路径常量 `/auth/obo`。

## Review Focus（规格隐含但无任务直测的输入类，逐条已钉进对应任务）

1. **存量旧 token（无自定义 claims）部署日直接可用**——守卫端 flatten 缺省必须空 map 而非 None 报错；scopes 缺席 = 全量随 roles（Task 2 单测 + Task 10 e2e 回归）。
2. **`scopes: []` 与垃圾档位串**——present-but-empty = 显式零授权（全拒），未知串 = 档位不匹配拒（fail-closed）；缺席才是不检查（Task 8 单测钉三态）。
3. **cookie 会话路径经 admit**——scopes/act_mask 判据读 user JSON，与 Bearer 同源；cookie 会话用户带 scopes 时同样被裁（Task 8 单测，仿 lib_tests.rs:2597 anon 预短路样板）。
4. **POST-as-query 业务路由**（企业 admin 惯例）——read 机器人被 write 档拒，403 信封诚实（Task 10 e2e 钉住 + Task 11 限制表）。
5. **自定义 claim 撞保留键**（`claims: {exp: …}`）——签发端 fail-fast 带键名报错，不静默覆盖（Task 1 单测）。

---

### Task 1: crypto.rs — 签发端 claims 透传 + ttl_secs

**Files:**
- Modify: `src/bridge/crypto.rs:60-116`（`Claims` 结构体 + `op_jwt_sign`）
- Test: `src/bridge/crypto.rs` 既有 `#[cfg(test)]` 模块内追加

**Interfaces:**
- Consumes: 无（前置件）。
- Produces: `op_jwt_sign` payload 契约 `{sub: string, roles?: string[], claims?: object, ttl_secs?: number}`；`Claims { sub, roles, iat, exp, #[serde(default, flatten)] extra: serde_json::Map<String, Value> }`；常量 `pub const JWT_TTL_CAP_SECS: u64 = 7_776_000;`（90d）。JWT payload = 命名字段 + 自定义 claims **平铺到顶层**（非嵌套 `claims:` 键——`dispatch.rs:291` 的 PR-9 读路径 `user.claims.tenant` 依赖平铺）。

- [ ] **Step 1: 写失败测试**（BDD 注释；放 crypto.rs 测试模块）

```rust
// Given payload 带 claims {"tenant":"t1","svc":"bot1"} 与 ttl_secs 600
// When op_jwt_sign + op_jwt_verify roundtrip
// Then verify 出的 claims 含 tenant/svc 平铺键，exp-iat==600；
// Given claims 含保留键 "exp"
// When sign
// Then Err 且文案含 "reserved claim key"；
// Given ttl_secs 超过 JWT_TTL_CAP_SECS
// When sign
// Then exp-iat == JWT_TTL_CAP_SECS（钳制，不报错）；
// Given 旧形态 payload 仅 {sub, roles}
// When sign+verify
// Then 无自定义键、exp-iat==cfg.access_secs（零破坏回归）。
```

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p only-js jwt -- --nocapture | tail -20`
Expected: 编译失败或断言 FAIL（无 claims/ttl 参数）

- [ ] **Step 3: 实现**——`Claims` 加 `#[serde(default, flatten)] extra: serde_json::Map<String, Value>`；`op_jwt_sign`：读 `payload["claims"]`（必须 object，否则 Err）逐键校验非保留键（保留键 → `Err("jwt.sign: claims contain reserved claim key \`{k}\`")`）、读 `payload["ttl_secs"]`（u64，可选）→ `exp = now + ttl_secs.min(JWT_TTL_CAP_SECS)`。serde flatten 顺序：命名字段优先序列化，map 键随后；保留键已在入口拒绝，无重复键风险。

- [ ] **Step 4: 跑测确认通过**

Run: `set -o pipefail; cargo test --release -p only-js jwt -- --nocapture | tail -20`
Expected: PASS（含既有 jwt 测试零回归）

- [ ] **Step 5: Commit** `git add -A && git commit -m "feat(bridge): jwt.sign 支持自定义 claims 平铺与 ttl_secs（守卫侧孪生变更随下一提交）"`

### Task 2: oj-auth — 守卫端 claims 透传

**Files:**
- Modify: `plugins/oj-auth/src/lib.rs:9-16`（`Claims`）、`:169-182`（`verify_jwt` 不变——`"claims": claims` 整体序列化已带 flatten 键）
- Test: `plugins/oj-auth/src/lib.rs` 测试模块（既有 `sign()` 助手在 `:300` 附近）

**Interfaces:**
- Consumes: Task 1 的 payload 形状（两端孪生，**同发布**）。
- Produces: guard verify 返回 `{"id","roles","claims"}`，其中 `claims` = `{sub, roles, iat, exp, ...自定义平铺键}`；旧 token（无自定义键）`claims` 形状与今日逐字节一致。

- [ ] **Step 1: 写失败测试**

```rust
// Given sign 侧用与 Task 1 同构的手工编码 payload（含 "svc":"b1","scopes":["read"]）
// When guard verify
// Then user["claims"]["svc"]=="b1" 且 user["claims"]["scopes"] 为数组；
// Given 无自定义键的旧 token
// When verify
// Then claims 键集恰为 {sub,roles,iat,exp}（Review Focus 1）。
```

（注：oj-auth 测试侧没有 Task 1 的 sign 函数——用 `jsonwebtoken::encode` 手工编码同形 payload。）

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p oj-auth | tail -10`
Expected: FAIL（claims 无自定义键）

- [ ] **Step 3: 实现**——`Claims` 加同名 `#[serde(default, flatten)] extra: serde_json::Map<String, Value>`；`verify_jwt` 主体不动。

- [ ] **Step 4: 跑测确认通过**（含既有守卫测试零回归）

Run: `set -o pipefail; cargo test --release -p oj-auth | tail -10`
Expected: PASS

- [ ] **Step 5: Commit** `git commit -m "feat(oj-auth): Claims flatten 透传自定义 claim（与 crypto.rs 同发布孪生变更）"`

### Task 3: contract.rs — description 白名单放行

**Files:**
- Modify: `src/contract.rs:33-49`（`SUPPORTED_KEYWORDS`）、`:55`（`SCHEMA_CHANNELS`）、`:305-315`（`audit_schema` 加顶层 description string 校验）、`:349-362`（`audit_keywords` 加 description string 校验分支）
- Test: `src/contract_tests.rs`

**Interfaces:**
- Produces: `.schema` 顶层 `description: string`（组/动词级）与契约通道内属性级 `description: string` 通过审计；二者随 `RouteRow.schema` Value 透传（`mergeSchema` 末尾未知键拷贝已保证动词 > 组，见 bridge/mod.rs:1774-1775——本任务加测试钉住，不改 mergeSchema）。

- [ ] **Step 1: 写失败测试**（contract_tests.rs）

```rust
// Given .schema 顶层 description（字符串）+ params.properties.id.description（字符串）
// When audit_schema
// Then Ok；
// Given description 为数字/对象（任一处）
// When audit_schema
// Then Err 且文案含 "description must be a string"；
// Given 组级 schema description="G"、动词级 v.schema description="V"
// When mergeSchema（bridge 内省 JS 路径，若单测不可达则在此任务改为 contract 侧金样）
// Then 合并结果 description=="V"（动词优先）。
```

（mergeSchema 在内嵌 JS 里，Rust 单测够不到——动词优先的钉子改放 Task 10 e2e 或 `oj/src/app_tests.rs` 路由表金样；本任务只钉审计三态。）

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p only-js description -- --nocapture | tail -10`
Expected: FAIL（unsupported channel/keyword）

- [ ] **Step 3: 实现**——两处白名单各追加 `"description"`；`audit_schema` 对顶层 `description` 校验 string；`audit_keywords` 对 `description` 键校验 string（递归天然覆盖属性级）。

- [ ] **Step 4: 跑测确认通过**

Run: `set -o pipefail; cargo test --release -p only-js contract | tail -10`
Expected: PASS（含既有 fail-fast 白名单测试零回归——白名单测试若断言「白名单恰 N 项」需同步更新计数）

- [ ] **Step 5: Commit** `git commit -m "feat(contract): .schema description 通道与关键字白名单放行（string-only）"`

### Task 4: openapi.rs + mcp.rs — description 进 spec 与 tool 选取

**Files:**
- Modify: `serve/src/openapi.rs:66`（summary 插入点旁加 description 插入）、`serve/src/mcp.rs:110-117`（description 选取）
- Test: `serve/src/openapi.rs` 测试模块、`serve/src/mcp.rs` 测试模块

**Interfaces:**
- Consumes: Task 3 的 `RouteRow.schema.description`。
- Produces: spec operation 新增可选 `description` 键（summary 恒合成不变）；`spec_to_tools` 的 tool.description = `description || summary`；「no declared input contract」后缀逻辑不变（仍叠加在选取值之后）。

- [ ] **Step 1: 记录基线 + 写失败测试**

先跑 `cargo build --release -p oj && ./bin/oj openapi -c sample/config.yaml -d sample/src | md5` 把改动前 spec 哈希记入本步骤勾选项旁（Step 4 对拍用）。

openapi.rs 测试：
```rust
// Given 契约带顶层 description "查询用户账户详情"
// When generate
// Then op["description"]=="查询用户账户详情" 且 op["summary"] 仍为合成串 "GET v1/api/user/account"；
// Given 无 description 的契约
// When generate
// Then op 无 description 键（键缺失，不是 null——字节兼容）。
```
mcp.rs 测试：
```rust
// Given op 带 description "D" 与 summary "S"
// When spec_to_tools
// Then tool.description=="D"；
// Given 仅 summary
// Then description=="S" 且 no-contract 后缀仍叠加（既有 undeclared 测试改造为双断言）。
```

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p serve description | tail -10`
Expected: FAIL

- [ ] **Step 3: 实现**——openapi.rs 在 `op.insert("summary",…)` 后：`if let Some(d) = schema.get("description") { op.insert("description".into(), d.clone()); }`（schema 即该行 `RouteRow.schema`，实现时对照 generate() 现场变量名）。mcp.rs：`let mut description = op.get("description").and_then(|v| v.as_str()).unwrap_or(summary…)…`（先 description 后 summary）。

- [ ] **Step 4: 跑测确认通过 + 字节兼容对拍**

Run: `set -o pipefail; cargo test --release -p serve | tail -10`
Expected: PASS
Run: `cargo build --release -p oj && ./bin/oj openapi -c sample/config.yaml -d sample/src | md5`
Expected: 与 Step 1 记录的基线哈希**全等**（sample 无 description 声明 → 字节零变化，反目标 3 的 CI 漂移门禁不破）。

- [ ] **Step 5: Commit** `git commit -m "feat(serve): openapi description 键透传 + MCP tool description 选取（声明值优先）"`

### Task 5: sample 迁移 — 四张表 + users.disabled

**Files:**
- Create: `sample/src/admin/migrations/{next_seq}__mcp_svc_tables.sql`（oj_svc_token / oj_bot_binding / oj_obo_audit；`{next_seq}` = admin/migrations 现有最大 seq+1，实现时 ls 确认）
- Create: `sample/src/{users 所属模块}/migrations/{next_seq}__users_disabled.sql`（先 `grep -rl "CREATE TABLE.*users" sample/src/*/migrations/` 定位所属模块）
- Modify: `sample/src/admin/schema.yaml`（三张新表声明）、users 所属模块 `schema.yaml`（disabled 列）
- Test: 无单元测试（迁移由 Task 10 e2e 验证）；验证步 = `oj migrate` 干跑

**Interfaces:**
- Produces（列契约，Task 6/7/8 依赖，逐字）：
  - `oj_svc_token(jti TEXT PRIMARY KEY, name TEXT NOT NULL, scopes TEXT NOT NULL, created_by TEXT NOT NULL, created_at INTEGER NOT NULL)`（scopes 存 JSON 数组串）
  - `oj_bot_binding(provider TEXT NOT NULL, external_sub TEXT NOT NULL, user_id TEXT NOT NULL, last_used_at INTEGER NOT NULL DEFAULT 0, created_by TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY(provider, external_sub))`
  - `oj_obo_audit(id INTEGER PRIMARY KEY AUTOINCREMENT, svc TEXT NOT NULL, provider TEXT NOT NULL, external_sub TEXT NOT NULL, user_id TEXT NOT NULL, ok INTEGER NOT NULL, reason TEXT NOT NULL DEFAULT '', ts INTEGER NOT NULL)`
  - `users.disabled INTEGER NOT NULL DEFAULT 0`

（`AUTOINCREMENT` 若 pg 方言不兼容，用 `BIGSERIAL`/`GENERATED` 写方言变体文件 `.__pg.sql`/`.__mysql.sql`——沿 `_platform/migrations/0002__versions.pg.sql` 先例；sqlite 走无后缀通用文件。实现时以 admin 模块现有迁移的方言策略为准。）

- [ ] **Step 1: 写迁移 SQL + schema.yaml 声明**（照 admin/schema.yaml 现有表声明格式；三张表 + disabled 列）
- [ ] **Step 2: 干跑验证**

Run: `cargo build --release -p oj && ./bin/oj migrate -c sample/config.yaml -d sample/src`
Expected: 迁移全部应用，无序列/方言报错；连跑两次第二次为 no-op（幂等/账本）

- [ ] **Step 3: Commit** `git commit -m "feat(sample): svc-token/bot-binding/obo-audit 三表迁移 + users.disabled 列"`

### Task 6: sample auth/obo 端点 + 换签审计 + login disabled 检查

**Files:**
- Create: `sample/src/auth/obo/api.ts`
- Modify: `sample/src/auth/_shared/`（新增 `obo.ts`：共享校验/换签/审计助手——DRY，供 obo 端点与未来 bot 骨架复用）
- Modify: `sample/src/auth/login/api.ts`（select 加 `disabled` 列，`disabled==1` → 与密码错同报 401「invalid credentials」——不泄露账号状态）
- Test: `oj/tests/e2e.rs`（本任务只落实现；e2e 断言统一在 Task 10）

**Interfaces:**
- Consumes: Task 1（jwt.sign claims/ttl）、Task 2（守卫透传 → `http.user.claims.svc`）、Task 5（三张表）。
- Produces（对 Task 7/10/11 的契约）：
  - `POST /v1/api/auth/obo`，body `{provider: string, external_sub: string}` → 200 `{access_token, expires_in: 600}`；失败信封（**code 冻结**）：`40310` 未绑定 / `40311` jti 已吊销 / `40312` 非 svc 凭据 / `40313` 员工停用（msg 中文，如「该账号未绑定机器人代操作，请联系管理员」）。员工 roles 从 users 表实时查；新 token `claims: {act: {sub: "svc:<name>"}, tenant: <员工 claims.tenant 原样复制，缺失则不带>}`，`ttl_secs: 600`。
  - 每次调用（成功或失败）写一行 `oj_obo_audit`（ok: 0/1，reason: 失败 code 名）。
  - `disabled` 判定在换签侧用 `users.disabled==1` → 40313（login 侧同列 401 同报）。

- [ ] **Step 1: 实现 `obo.ts` 助手 + obo/api.ts + login disabled 检查**（roles 检查照 `sample/src/user/_shared` 的 requireRole 形态；jti 查询 `db.table("oj_svc_token")`）
- [ ] **Step 2: 编译级验证**（TS 端点由 dev 热重载载入；语法错会在 serve 启动时报）

Run: `cargo build --release -p oj && ./bin/oj serve -c sample/config.yaml --api-path sample/src &` 后 `curl -s -X POST localhost:9778/v1/api/auth/obo -H 'content-type: application/json' -d '{}'`，期望 40312 信封（无凭据也走守卫 401——先确认裸 401，再带手工编码 svc token（测试用 HS256 + sample jwt secret 手工签）打四条失败路径逐一比对 code）。
Expected: 四个失败 code 各就各位；成功路径手工插一行绑定 + svc token 记录后换签得 600s token，`jwt.verify` 解出 act/tenant。
（验证完杀掉 serve 进程。）

- [ ] **Step 3: Commit** `git commit -m "feat(sample): auth/obo 换签端点（错误码 40310-40313）+ 换签审计表落行 + login 停用检查"`

### Task 7: sample admin 运营端点

**Files:**
- Create: `sample/src/admin/service-token/api.ts`（`get`=list、`post`=create、`post.route="revoke"` 或 `del.route="{jti}"`=吊销——照 admin 模块既有 `.route` 惯例选一种）
- Create: `sample/src/admin/bot-binding/api.ts`（CRUD + `post.route="import"` CSV 批量导入 + `get.route="export"` CSV 导出）
- Create: `sample/src/admin/obo-audit/api.ts`（`get`=分页列表（`_shared/paged` 先例）+ `get.route="export"` CSV）
- Modify: 无（表已由 Task 5 声明）
- Test: Task 10 e2e 覆盖；本任务验证步为手测

**Interfaces:**
- Consumes: Task 5 表、Task 6 的 code 语义；admin role 门照既有 admin 端点的角色检查模式（实现时先读 `sample/src/admin/role-list/api.ts` 抄同款守卫）。
- Produces:
  - service-token create：body `{name, scopes?}` → 生成 jti（`crypto.randomHex(16)`）、`jwt.sign({sub:"svc:"+name, roles:["bot"], claims:{svc:name, jti, scopes}, ttl_secs: 7776000})`，登记表，响应 `{token, jti}`（**仅创建时返回明文**）；list 不含明文；revoke = 删行。
  - binding import：CSV 两列 `provider,external_sub,user_id`（首行表头），逐行 upsert；export 同列。行数上限沿 `pipeline.max_upload`（不另设）。

- [ ] **Step 1: 三个端点实现**（admin 角色门 + `db.table` 构造器；CSV 解析用最简 split——引号内逗号不支持，注释声明此限制）
- [ ] **Step 2: 手测验证**（serve 起样：create → list 明文不泄露 → revoke → binding import/export roundtrip → audit 分页）
- [ ] **Step 3: Commit** `git commit -m "feat(sample): admin 运营端点——svc-token 签发/列表/吊销、绑定 CRUD+CSV、换签审计查询导出"`

### Task 8: serve — admit async 化 + scopes/jti/act_mask 单点门

**Files:**
- Modify: `serve/src/dispatch.rs:227-340`（`admit` async 化 + 三检查）、`serve/src/dispatch.rs:167`（调用点 `.await`）
- Modify: `serve/src/lib.rs:180-209`（Pipeline 增 `act_mask_destructive: bool`）、`serve/src/lib.rs:111-130`（AppState 增 `svc_gate: Option<Arc<SvcGate>>`）、`serve/src/lib.rs:738`（blob 调用点 `.await`）
- Modify: `serve/src/mcp_http.rs:311`（调用点 `.await`）
- Create: `serve/src/svc_gate.rs`（`SvcGate { accessor: Arc<dyn DataAccessor>, cache: RwLock<Option<(Instant, HashSet<String>)>> }`，`async fn jtis(&self) -> BridgeResult<HashSet<String>>`（30s TTL）、`fn tier_of(verb: &str) -> Option<&'static str>`）
- Modify: `src/config.rs:134-147`（`McpCfg.act_mask_destructive: bool`，serde default false）+ serve 侧 `McpConfig`/`Pipeline::from_config` 装配（实现时 grep `McpConfig` 定义与 from_config 的 mcp 段）
- Modify: `oj/src/serve_cmd.rs`（装配：`connect_dbs` 附近取 default db DSN → `DbBackendRegistry` connect → `Arc<dyn DataAccessor>` 注入 app()；无 dbs 时 None + 启动 WARN 一行「svc tokens cannot be verified without a default db — svc-marked tokens will be denied」）
- Test: `serve/src/lib_tests.rs`（admit 三态）、`serve/src/dispatch.rs` 或 svc_gate.rs 单测

**Interfaces:**
- Consumes: Task 2（user.claims.scopes/svc/act 可读）、Task 5（oj_svc_token 表）。
- Produces（对 Task 9/10 的契约，逐字）：
  - `async fn admit(st, headers, verb, path_no_base) -> Result<AdmitOut, Box<Response>>`（其余三参签名不变；AdmitOut 不变）。
  - 检查顺序（guard 验签成功后、租户提取前）：① scopes——`user.claims.scopes` 为数组时（**缺席 = 跳过；present-but-empty = 全拒；未知串 = 档位不匹配拒**），`tier_of(verb)`：get→"read"、post/put/patch→"write"、delete→"destructive"；档位 ∉ scopes → `fail_response(403, "missing scope: {tier}")`；豁免：`path_no_base == "/auth/obo"`。② jti——`user.claims.svc` 为字符串时查 `SvcGate.jtis()`，不含该 jti 或查询 Err → `fail_response(403, "svc token revoked or unverifiable")`（fail-closed；svc_gate 为 None 同拒）。③ act_mask——`pipeline.act_mask_destructive == true && user.claims.act 存在 && verb=="delete"` → `fail_response(403, "destructive tools are masked for delegated sessions")`。
  - `tier_of` 为 `pub` 纯函数（Task 9/10 复用，DRY）。
  - WS 镜像路由不经 admit——不在本任务补检查，Task 11 进 SKILL 陷阱表。

- [ ] **Step 1: 写失败测试**（lib_tests.rs，仿 ：2597 样板构造 AppState；守卫用 oj-auth 假 guard 或既有测试 fake）

```rust
// Given token claims.scopes=["read"]（手工编码，同 Task 2 测试法）
// When admit GET → Ok；POST → Err 403 "missing scope: write"；DELETE → Err 403 "missing scope: destructive"；
// Given scopes=[] When admit GET → Err（Review Focus 2）；
// Given scopes 含垃圾串 ["banana"] When admit GET → Err "missing scope: read"；
// Given 无 scopes 旧 token When admit DELETE → Ok（Review Focus 1）；
// Given cookie 会话携带同 claims When admit → 与 Bearer 同判（Review Focus 3）；
// Given claims.svc 且 SvcGate 表含其 jti When admit → Ok；删行 + 缓存过期后 → Err 403；
// Given act_mask=true 且 claims.act 且 DELETE → Err 403 masked；false → Ok。
```

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p serve admit | tail -15`
Expected: 编译失败（admit 非 async）或断言 FAIL

- [ ] **Step 3: 实现**——按 Produces 顺序落地；`admit` 改 async 后三处调用点机械加 `.await`；SvcGate 按 sqlite `query_with_params("SELECT jti FROM oj_svc_token", &[])` 拉 jti 集（静态 SQL，非 JS 来源，沿 `_oj_migrations` 先例）；Row 取列 API 现场核对（`Row` 的列访问方法以 db.rs 定义为准）。

- [ ] **Step 4: 跑测确认通过**

Run: `set -o pipefail; cargo test --release -p serve | tail -10 && set -o pipefail; cargo test --release -p only-js | tail -5`
Expected: PASS（serve 全量 + 根 crate 零回归）

- [ ] **Step 5: Commit** `git commit -m "feat(serve): admit 单点 scopes/jti/act_mask 门（async 化；三面同规则）+ SvcGate 30s TTL"`

### Task 9: mcp_http — 展示层裁剪 + isError `[code=<n>] <msg>` 前缀

**Files:**
- Modify: `serve/src/mcp_http.rs:245-271`（`is_error`/`capture_to_result`）、`:258-266`（信封错误分支）、`:439-455`（rpc_handle：tools_for 后按 user 展示裁剪？否——/rpc 无列表语义，不动）、`handle_mcp` 的 tools/list 分支（裁剪）、`call_tool`（解析全集不变——确认 `-32601` 语义不回归）
- Test: `serve/src/mcp_http.rs` 测试模块（纯函数可测：构造 Capture 与 user Value）

**Interfaces:**
- Consumes: Task 4 的 tool 结构、Task 8 的 `tier_of`、endpoint admit 的 user（`read_inbound` 需把 user 带出——改 `Inbound::Request` 变体加 `user: Option<Value>` 字段，或 handle 层再取一次 endpoint admit 结果；实现取前者，read_inbound 已有 admit 结果在手，零额外开销）。
- Produces:
  - `fn filter_tools_for_display<'a>(tools: &'a [McpTool], user: Option<&Value>, act_mask: bool) -> Vec<&'a McpTool>`：scopes 缺席 → 全量；scopes present → 保留 `tier_of(method) ∈ scopes` 的 tool；act_mask && user.claims.act && method==delete → 排除。tools/list 分支调用它。
  - `capture_to_result` 错误分支统一：业务 code!=0 → `is_error(&format!("[code={}] {}", code, msg))`；非 2xx → 先尝试按信封解析取 code/msg 同前缀，解析不出 → 原文（现状）。`capture_to_rpc_result` 的业务错误 message 同加前缀（形状一致）。

- [ ] **Step 1: 写失败测试**

```rust
// Given cap 信封 code=403 msg="missing scope: read"
// When capture_to_result
// Then content[0].text == "[code=403] missing scope: read" 且 isError==true；
// Given cap 非 2xx 且 body 为信封
// When capture_to_result
// Then 同前缀形状；body 非信封 → 原文不变（raw 豁免语义保留）；
// Given tools [get,post,delete] 与 scopes=["read"]
// When filter_tools_for_display
// Then 仅 get；scopes=None → 三个全留；act_mask=true+act+scopes 全档 → 无 delete。
```

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p serve capture_to_result | tail -10`
Expected: FAIL

- [ ] **Step 3: 实现**（`[code=]` 前缀是冻结文案——Review 全局约束；modules 滤掉的 tool 回 -32601 的现状**不得改动**，补一条回归断言钉住）
- [ ] **Step 4: 跑测确认通过**

Run: `set -o pipefail; cargo test --release -p serve | tail -10`
Expected: PASS

- [ ] **Step 5: Commit** `git commit -m "feat(serve): tools/list 按 scopes/act_mask 展示裁剪 + isError 统一 [code=<n>] <msg> 前缀"`

### Task 10: e2e 全矩阵（oj/tests/e2e.rs）

**Files:**
- Modify: `oj/tests/e2e.rs`（追加 `mcp_scopes` / `mcp_obo` / `mcp_desc` 三组测试；token 手工编码复用既有 e2e 的 secret/签名助手——先 grep e2e.rs 既有 jwt 助手，无则在本文件加一个 `sign_hs256(secret, claims_json)`）

**Interfaces:**
- Consumes: Task 1-9 全部产物；sample 部署（Task 5-7 端点 + 表）。
- Produces: 回归门（后续任何改动的安全网）。

- [ ] **Step 1: 写三组 e2e（BDD 注释逐条）**

`mcp_desc`：
```rust
// Given sample 某模块 api.ts 暂加 .schema description（e2e fixture 内声明，不污染 sample 正式模块——若 e2e 用独立 fixture 目录则写在 fixture）
// When oj openapi / /mcp tools/list
// Then description==声明值；无声明路由 description==合成 summary（字节回归）。
```
`mcp_scopes`（Review Focus 2/4）：
```rust
// Given read svc token
// When tools/list → 无 write/destructive tool；
// When tools/call GET tool → 200；tools/call POST 业务 tool（POST-as-query fixture 路由）→ isError "[code=403] missing scope: write"；
// When 直连 REST DELETE（同 token，不经 /mcp）→ 403 信封同 msg（三面同规则）；
// When /rpc 同 operationId → JSON-RPC error 同语义；
// Given 无 scopes 旧 token → 三面全量（零破坏）。
```
`mcp_obo`（Review Focus 1/3 + 全失败码）：
```rust
// Given admin 建 svc token + 绑定行
// When obo 换签 → 600s token 含 act；以它 call GET → 按员工角色命中；
// When 四失败路径逐个打（未绑定/吊销/非 svc/停用）→ code 40310..40313 与 Task 6 契约一致；
// When jti 删行后 31s 内重试 direct /mcp GET → 403（30s TTL 上限；等待用 deadline 轮询禁 sleep）；
// When 租户部署矩阵（fixture 带 tenant 配置）：换签 token 带 claims.tenant → 业务 200。
// Then obo_audit 表每次调用落一行（经 admin 端点查证）。
```

- [ ] **Step 2: 跑全量 e2e**

Run: `set -o pipefail; cargo test --release -p oj --test e2e | tail -20`
Expected: 新三组 PASS + 既有 e2e 零回归（**尤其 MCP 既有用例**——isError 前缀若被既有断言锁定，同步更新断言并注明「文案冻结 v0.1.130 起」）

- [ ] **Step 3: Commit** `git commit -m "test(oj): e2e 三组——描述透传/scopes 三面矩阵/obo 全失败码与审计"`

### Task 11: 配置示范 + devkit 四件套 + CHANGELOG + 门禁

**Files:**
- Modify: `sample/config.yaml`（mcp 段示范：`enabled: true`、`modules` 排除 auth/admin 的白名单、`act_mask_destructive: true`——先确认 sample 现有 mcp 段与 auth 装配，缺 auth fail-fast 不许触发）
- Modify: `CHANGELOG.md`（v0.1.130 节：claims 平铺 + tenant 生效变化、scopes/act_mask/jti、obo 四码、isError 前缀冻结、description 键、WS 不经 admit、线路 A 不受收窄）
- Modify: `docs/devkit/api-manual.md`（jwt.sign 新签名 + ttl 上限 90d、claims 透传与 tenant 生效、obo 四错误码表、svc-token/绑定/审计端点、scopes 三档映射 + 「POST 按 write 档判定」限制与两条绕法、`server.mcp.act_mask_destructive`）、`docs/devkit/scenarios.md`（企微端到端一篇：回调→userid→obo→MCP→回复 + 5s ack/MsgId 去重/2048 截断 + 401→换签→单次重试→告警标准模式 + 首次使用知情话术 + bot handler 骨架指引指向 sample/src/bot/*——若骨架端点未在 Task 6 建，此处补 `sample/src/bot/wecom/api.ts` 最小 echo 骨架）、`docs/devkit/SKILL.md`（陷阱速查：WS 不经 admit、线路 A 不受收窄、scopes 缺席/空数组语义、svc scopes 不约束换签面、obo 10min、POST-as-query）、`docs/devkit/README.md`（交叉引用）
- Create: `sample/src/bot/wecom/api.ts`（若 Task 6 未建；最小回调骨架：ack → obo → tools/call → 中文模板回复，含 MsgId 去重占位）
- Test: `cargo test --release -p xtask`（devkit 契约用例校验产物与源一致）

- [ ] **Step 1: sample 配置 + bot 骨架 + 四件套 + CHANGELOG**（devkit 文档逐条对齐用户可见变更——仓规红线；api-manual 错误/限制表补：POST 档位、ttl 上限、`[code=]` 文案冻结、WS/线路 A 排除、tools/list 勿跨会话缓存指引；SKILL 速查按 Task 8/9 产物登记）**+ P1 验收 smoke**：固定 5-10 条自然语言任务对 sample MCP 面做加 description 前后对比（人工跑，结果记入 CHANGELOG v0.1.130 节）**+ 指标口径登记**（scenarios 或 README：description 覆盖率、obo 日换签数、绑定缺失错误占比、scope 拒绝按档位分布——全走既有日志，零新系统）
- [ ] **Step 2: 全量门禁**

Run: `set -o pipefail; cargo test --release -p xtask | tail -10 && cargo fmt --check && set -o pipefail; cargo clippy --release --workspace --all-targets -- -D warnings 2>&1 | tail -10 && cargo xtask build && cargo xtask smoke --bin bin/oj`
Expected: 全绿（fmt/clippy/smoke 三门禁 + devkit 契约）

- [ ] **Step 3: Commit** `git commit -m "docs+chore: v0.1.130——devkit 四件套对齐 + sample 配置示范 + CHANGELOG"`
