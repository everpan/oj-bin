# MCP server + JSON-RPC 双线路实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 从 oj 路由表一键产出 MCP tool 面——`oj mcp` 生成零依赖 stdio MCP server（线路 A），serve 内置 `{base}/mcp`（streamable HTTP）+ `{base}/rpc`（裸 JSON-RPC 2.0）（线路 B）。

**Architecture:** 唯一共享纯函数链 `RouteTable → serve::openapi::generate → serve::mcp::spec_to_tools`（spec v2 R1–R4）。B 每请求从活 `AppState.mounts` 现算 tool 列表、经从 `run_route` 抽出的 `run_capture` 助手进程内分发（validate=true、admit 传 tool 自身路径）；A 把同一份产物内嵌进生成的 Node 脚本，HTTP 回源。**SOLID**：每个文件单一职责（openapi.rs=spec 生成，mcp.rs=spec→tool 纯映射，mcp_http.rs=协议组帧与 handler）；**DRY**：dispatch 助手、generate、spec_to_tools 均只有一份实现，A/B 消费同一份。

**Tech Stack:** Rust（axum/serde_json，零新增依赖）+ 生成的 plain JS（node ≥18）。

**Spec:** `docs/superpowers/specs/2026-10-30-mcp-jsonrpc-design.md`（v2 评审修订版 + v3 安全增补 R9，本计划引用的 R1–R9 编号出自该文档）。

## Global Constraints

- 构建/测试/lint 一律 `--release`：禁裸 `cargo build`/`cargo test`/`cargo clippy`；gate = `cargo fmt --check` 与 `cargo clippy --release --workspace --all-targets -- -D warnings`。
- **零新增 crate**（spec 取舍 1）。
- R1 硬约束：`generate` 迁移后 openapi.json **字节级不变**——`oj openapi --check` CI 漂移门禁即验收。
- 异步测试一律 `#[tokio::test(flavor = "current_thread")]`（JsRuntime !Send）。
- env-gated 测试腿诚实登记（样板见 src/bridge/inspector.rs）。
- 版本红线：收尾任务须同步 CHANGELOG.md + `docs/devkit/` 四件套 + `cargo xtask build` + devkit 契约测试。
- commit 结尾加 `unix@vip.qq.com ai`。

## Review Focus

1. **无 `id` 的 JSON-RPC 消息**（通知/batch 元素）——必须 202 空响应、不得回 JSON-RPC result（spec R5）。
2. **净化后 tool 名碰撞**——`a.b_get` 与 `a_b_get` 净化同名，唯一性门禁必须在净化后的名字上重跑（R4）。
3. **catch-all 参数含 `/`**——B/A 编码规则相反（单段 encodeURIComponent、catch-all 原样），含 `/` 的单段参数必须正确地 miss（参数约定）。
4. **业务错误 vs 协议错误**——信封 `code!=0` 是 `isError:true`（HTTP 200），只有未知 tool/坏参数才是 JSON-RPC error（R3）；A 侧解析不得 gate 在 `res.ok`。
5. **鉴权粒度**——`admit` 必须收 tool 的 `path_no_base` 而非字面 `/mcp`，否则 per-route `anonymous_paths` 被摊平（R2）；且 **endpoint 本身也必须 admit**（字面 `/mcp`），否则匿名者枚举全路由目录（R9 B1）。
6. **stream/SSE 端点**——`cap.stream.is_some()` 必须返回 isError 而不是缓冲到挂起（R6）。
7. **modules 白名单 fail-closed**——tag 匹配不到 → 空列表，不得回落全集；无 tag 路由在白名单模式下排除（R9）。

---

## Phase 1：R1 迁移 `generate` 到 serve

### Task 1: `serve::openapi` 模块（generate 字节级迁移）

**Files:**
- Create: `serve/src/openapi.rs`
- Modify: `oj/src/openapi_cmd.rs`（删除 `generate` + `to_oa_path`/`contract_params`/`pattern_param_names`/`path_params`/`operation_id`/`envelope_schema` 及其专属测试，改为调用 `serve::openapi::generate`）
- Modify: `serve/src/lib.rs`（加 `pub mod openapi;`）

**Interfaces:**
- Produces: `pub fn generate(table: &crate::routes::RouteTable, base: &str) -> Result<serde_json::Value, String>`（签名与原 `oj/src/openapi_cmd.rs:160` 完全一致，含 `table.audit_schemas()?` 前置审计）。
- 注意：`generate` 仅依赖 `serde_json` + `serve::routes` + `only_js::contract::audit_schema`（serve 已有 `only_js` 依赖；若 Cargo.toml 缺则加 `only_js` 到 serve 依赖——非新增外部 crate，允许）。

- [ ] **Step 1: 迁移前基线快照**

Run: `./bin/oj openapi -c sample/config.yaml -d sample/src -o /tmp/oa-before.json`（若 bin/oj 非当前构建，先 `cargo xtask bin`）
Expected: 成功写出 spec（后续字节比对用）。

- [ ] **Step 2: 移动代码与测试**

把 `generate` 及 6 个 helper 原样搬到 `serve/src/openapi.rs`（保持 `pub` 性最小化：只有 `generate` pub）；把 `openapi_cmd.rs` 中只测 `generate` 的 10 个测试（`generate_produces_*`、`contract_emits_*`、`response_contract_*`、`del_verb_*`、`without_*`、`tags_carry_*`、`dev_files_*`、`duplicate_operation_ids_*`、`generate_fails_fast_*`、`contract_params_must_agree_*`、`release_entries_*`）整体搬到 `serve/src/openapi.rs` `#[cfg(test)]`，仅改 `use` 路径。`oj/src/openapi_cmd.rs` 的 `generate` 改为 `pub(crate) use serve::openapi::generate;` 式薄转发（保持 `openapi_cmd::generate` 调用点不动）。

- [ ] **Step 3: 跑迁移后测试**

Run: `cargo test --release -p serve openapi` 及 `cargo test --release -p oj openapi`
Expected: PASS（零行为改动）。

- [ ] **Step 4: 字节级 parity 验证**

Run: `./bin/oj openapi -c sample/config.yaml -d sample/src -o /tmp/oa-after.json && cmp /tmp/oa-before.json /tmp/oa-after.json`
Expected: 无输出（字节一致）。

- [ ] **Step 5: 全门禁**

Run: `cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings && cargo test --release --workspace`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add serve/src/openapi.rs serve/src/lib.rs serve/Cargo.toml oj/src/openapi_cmd.rs
git commit -m "refactor(serve): generate 自 oj 迁入 serve::openapi（字节级不变，MCP 共享前置）

unix@vip.qq.com ai"
```

**阶段小结点**：`oj openapi` 功能无损，serve 侧具备 spec 生成能力。

---

## Phase 2：共享核心 `serve::mcp::spec_to_tools`

### Task 2: tool 名净化与唯一性（R4 前半）

**Files:**
- Create: `serve/src/mcp.rs`
- Modify: `serve/src/lib.rs`（`pub mod mcp;`）

**Interfaces:**
- Produces:
```rust
pub struct McpTool {
    pub name: String,                          // 净化后唯一名（^[a-zA-Z0-9_-]{1,64}$）
    pub tool: serde_json::Value,               // client 可见对象（name/description/inputSchema/annotations/tags）
    pub dispatch: ToolDispatch,                // MCP payload 不携带
}
pub struct ToolDispatch { pub operation_id: String, pub method: String, pub oa_path: String }
pub fn sanitize_name(raw: &str) -> String    // [^a-zA-Z0-9_-]→_；>64 → 截 57 + "_" + 8hex(fnv1a64(raw))
pub fn spec_to_tools(spec: &serde_json::Value) -> Result<Vec<McpTool>, String>  // Task 3 填满；本任务仅名字 + 占位
```
- 冲突错误文案沿用 openapi 风格：`` `x` is derived from two different routes … disambiguate ``，且**碰撞在净化后检测**。

- [ ] **Step 1: BDD 失败测试**（Given/When/Then 注释风格）

```rust
// Given 两个净化后同名的 operationId（"a.b_get" 与 "a_b_get"）
// When spec_to_tools
// Then 确定性后缀消重（第二个名字追加短 hash），且 seen 门禁报不出误报
#[test]
fn sanitized_name_collisions_are_deduped_deterministically() { ... }
// Given >64 字符的深路径 operationId
// When sanitize_name
// Then 结果 ≤64 且以 "_" + 8hex 结尾，同输入同输出
#[test]
fn long_names_are_truncated_with_hash_suffix() { ... }
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --release -p serve mcp` Expected: FAIL（函数不存在）。
- [ ] **Step 3: 实现 `sanitize_name` + spec→(operationId,method,oa_path) 骨架与净化后唯一性门禁**（tool 对象其余字段 Task 3 补）。
- [ ] **Step 4: 跑测试确认通过 + fmt/clippy**。
- [ ] **Step 5: Commit** `feat(serve): MCP tool 名净化与净化后唯一性门禁（R4）`。

### Task 3: inputSchema 拼装、$ref 拒绝、annotations、描述标注（R4 后半 + 反目标 4）

**Files:**
- Modify: `serve/src/mcp.rs`

**Interfaces:**
- Consumes: Task 1 `serve::openapi::generate` 产物（parameters/in:path|query、requestBody.content.application/json.schema、responses.200…、tags、summary）。
- Produces: `McpTool.tool` 完整形态（字段见 Task 2），`spec_to_tools` 完整实现。

拼装规则（spec 共享核心节，逐字）：
- path/query parameters → inputSchema properties（`in:path` 恒入 `required`；query 按 parameter.required）；requestBody schema → properties 挂 `body` 键，其 `required` 数组并入顶层 required。
- 含 `$ref`（任意深度）→ Err("tool `x` inputSchema contains $ref — inline it in .schema")。
- `annotations`：method GET → `{"readOnlyHint":true}`；DELETE → `{"destructiveHint":true}`；其他无 annotations 键。
- **未声明契约的端点**（无 parameters 且无 requestBody）→ description 追加 `" (no declared input contract — inspect source)"`。
- tool 对象保留 `tags`（模块过滤用，Task 6 消费）。

- [ ] **Step 1: BDD 失败测试**

```rust
// Given 契约齐备的 POST（path id:integer + query page:required + body name:required）
// When spec_to_tools
// Then inputSchema.required == ["id","page","name"]，properties 含 body 且 id type integer
#[test]
fn contract_route_builds_merged_input_schema() { ... }
// Given 契约里任意深度嵌 $ref
// When spec_to_tools
// Then Err 且文案含工具名与 "$ref"
#[test]
fn ref_anywhere_fails_fast() { ... }
// Given GET 与 DELETE 端点
// Then 前者 annotations.readOnlyHint==true，后者 annotations.destructiveHint==true，POST 无 annotations
#[test]
fn method_annotations_follow_http_verb() { ... }
// Given 无契约端点
// Then description 以 "(no declared input contract — inspect source)" 结尾
#[test]
fn undeclared_contract_is_flagged_in_description() { ... }
// Given del 动词端点
// Then dispatch.method == "delete"（openapi 已归一，不得回退 del）
#[test]
fn dispatch_method_uses_normalized_delete() { ... }
```

- [ ] **Step 2: 跑测试确认失败**。
- [ ] **Step 3: 实现完整 `spec_to_tools`**（签名 Task 2 已钉）。
- [ ] **Step 4: 测试通过 + `cargo test --release -p serve mcp` + fmt/clippy**。
- [ ] **Step 5: Commit** `feat(serve): spec_to_tools——inputSchema 拼装/$ref fail-fast/method annotations（R3/R4）`。

**阶段小结点**：共享核心闭环，A/B 从此同源。

---

## Phase 3：线路 B（serve 内置端点）

### Task 4: 配置面与装配防线（R7/R8）

**Files:**
- Modify: `src/config.rs`（`server.mcp.*` / `server.rpc` 解析，仿 `app_path` 形态）
- Modify: `serve/src/lib.rs`（`Pipeline` 增 `pub mcp: Option<McpConfig>`、`pub rpc: bool`；`pub struct McpConfig { pub modules: Option<Vec<String>>, pub allow_anonymous: bool }`）
- Modify: `oj/src/serve_cmd.rs`（Config → Pipeline 接线；`enabled && auth.is_none() && !allow_anonymous` → 启动 fail-fast；disabled 且路由非空 → 启动日志一行提示；`/mcp`、`/rpc` 入 reserved-path 启动 WARN 清单）

**Interfaces:**
- Produces: `Pipeline.mcp: Option<McpConfig>`（None=disabled）、`Pipeline.rpc: bool`；装配期 `McpConfig` 到达 serve。
- Config YAML 形状（spec R8 逐字）：`server.mcp.enabled`（缺省 false）/`server.mcp.modules`（可选数组）/`server.mcp.allow_anonymous`（缺省 false）/`server.rpc.enabled`（缺省 false）。
- **白名单语义钉死（R9 fail-closed）**：`modules` 非空时，tool 的 tags 与名单**有交集才保留**；无 tags 的路由排除；名单匹配不到任何模块 → 空列表（不得视为「无过滤」）。
- **sample 纪律（R9 S6）**：`allow_anonymous: true` 与关鉴权 config 不得进 sample/ 或任何入库 config——作为本任务的断言（grep 门禁或评审清单项）留痕。

- [ ] **Step 1: BDD 失败测试**（config.rs 与 serve_cmd.rs 各一组）

```rust
// Given config 未写 server.mcp
// When 解析
// Then mcp == None 且 rpc == false
#[test]
fn mcp_defaults_to_disabled() { ... }
// Given server.mcp.enabled=true 且未装 auth 插件且 allow_anonymous 缺省
// When assemble
// Then Err 且文案含 "server.mcp" 与 "auth"
#[test]
fn mcp_without_auth_plugin_fails_fast() { ... }
// Given 同上但 allow_anonymous=true
// Then 装配通过
#[test]
fn mcp_allow_anonymous_permits_unauthenticated_assembly() { ... }
// Given mcp.enabled=false 且路由表非空
// Then 启动日志含 "server.mcp.enabled=true" 提示行
#[test]
fn disabled_mcp_emits_discovery_hint() { ... }
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**（config 解析 + Pipeline 字段 + serve_cmd 三条防线 + reserved WARN）。
- [ ] **Step 4: 测试通过 + fmt/clippy**。
- [ ] **Step 5: Commit** `feat(serve): server.mcp/server.rpc 配置面 + 无 auth fail-fast + 发现提示（R7/R8）`。

### Task 5: 抽取 `run_capture` 共享 dispatch（R2）

**Files:**
- Modify: `serve/src/lib.rs`

**Interfaces:**
- Produces:
```rust
struct RunFail { status: u16, msg: String }                 // 408/500/401/400 的合成载体
async fn run_capture(
    st: &AppState, headers: &HeaderMap, verb: &str, query: HashMap<String, String>,
    path_no_base: Option<&str>, full_path: &str, file: PathBuf,
    params: HashMap<String, String>, handler: String,
    body_bytes: Vec<u8>, files: Vec<only_js::bridge::UploadedFile>,
) -> Result<only_js::bridge::Capture, RunFail>
// 内含 admit + RequestInfo 合成 + route_timeouts 首个命中（full_path）+ run_module(validate=true)
```
- `run_route`（lib.rs:745）改为：体缓冲/multipart（原逻辑不动）→ 调 `run_capture` → `capture_response(cap)` + `apply_custom_headers`。

- [ ] **Step 1: 纯重构，零行为改动**——不加新功能。保护网 = 既有测试套件。
- [ ] **Step 2: `cargo test --release -p serve` + `-p oj` 全绿 + clippy**。
- [ ] **Step 3: Commit** `refactor(serve): 抽取 run_capture——admit/timeout/validate=true 单点（R2）`。

### Task 6: `POST {base}/mcp` handler（R3/R5/R6）

**Files:**
- Create: `serve/src/mcp_http.rs`
- Modify: `serve/src/lib.rs`（`pub mod mcp_http;`；`app()` 中 `pipeline.mcp.is_some()` 时挂 `post("{p}/mcp", mcp_http::handle)`）

**Interfaces:**
- Consumes: `serve::mcp::spec_to_tools`、`run_capture`、`RuntimeMount::Api{prefix, table}`、`Lookup`。
- Produces: `pub async fn handle(State<AppState>, Request) -> Response`。

协议组帧（全部在 `mcp_http.rs`，**建议抽出纯函数便于单测**）：
- **R9 B1：endpoint 级 admit**——handler 第一步 `admit(st, &headers, "POST", Some("/mcp"))`，Err → 直接 401 响应（不进 JSON-RPC 层）；tools/call 内层 per-tool admit 照 R2 叠加。
- **R9：请求体 `Content-Type: application/json` 校验**（否则 415）+ body 读取上限 `pipeline.max_upload`（超限 413）。**实现 `ping`**（返回 `{}`）。JSON 解析失败 → `-32700`（id:null）。
- POST-only（axum `post()` 天然 405 GET/DELETE）；`Accept` 不含 `application/json`/`text/event-stream` → 406（注意 contains 判定须拒绝 `*/*`）；响应恒 `Content-Type: application/json`。
- batch 数组 → `-32600`；无 `id` 的通知 → **202 空 body**（不回 JSON-RPC）。
- `initialize` → `{protocolVersion, capabilities:{tools:{}}, serverInfo:{name:"oj", version}}`；protocolVersion 协商：client 版本 ∈ {2024-11-05, 2025-03-26, 2025-06-18} 则回显，否则回 `"2025-06-18"`。
- `tools/list`：按请求路径定位 mount（仿 dispatch_meta_handler 前缀命中）→ `serve::openapi::generate(table, prefix)` → `spec_to_tools` → `pipeline.mcp.modules` 白名单按 tags 过滤 → 剥掉 `dispatch` 字段返回。
- `tools/call`：`params.name` 查 tool → 无 → JSON-RPC `-32601`；`params.arguments` 按约定拆 `{path,query,body}` → path 模板填充（单段 `encodeURIComponent`，catch-all 原样）→ `routes::normalize` → `table.lookup(path, dispatch.method)` → `run_capture(..., path_no_base=该 tool 路径剥掉 prefix, full_path=具体路径, body=JSON(body))`。
  - `cap.stream.is_some()` → `isError:true`「streaming endpoint, not supported via MCP」（R6）。
  - `cap.status` 非 2xx → `isError:true` + body 文本。
  - body 解析命中信封：`code==0` → `{content:[{type:"text",text:JSON.stringify(data)}], structuredContent:data}`；`code!=0` → `{isError:true, content:[{text:msg}]}`。
  - 未命中信封（json.raw）→ `{content:[{text:原始 body 字符串}]}`。

- [ ] **Step 1: BDD 失败测试**（组帧纯函数 + handler 单测）

```rust
// Given 请求体是数组（batch）
// When handle
// Then 单对象 -32600（不逐元素处理）
#[test]
fn batch_array_is_rejected_with_minus_32600() { ... }
// Given 无 id 的通知消息
// Then 202 且 body 为空
#[test]
fn notification_gets_202_empty_body() { ... }
// Given client 报 protocolVersion 2024-11-05
// Then initialize 结果回显 2024-11-05；报 2099-01-01 则回 2025-06-18
#[test]
fn initialize_negotiates_protocol_version() { ... }
// Given dispatch 返回 cap.stream.is_some()（SSE/stream 端点）
// Then 结果 isError:true 且 content 含 "streaming endpoint"，handler 不缓冲流
#[test]
fn stream_endpoint_returns_iserror_instead_of_hanging() { ... }
// Given 请求无 Bearer 且 auth 插件已装配、"/mcp" 不在 anonymous_paths
// When POST /mcp（initialize）
// Then HTTP 401（endpoint 级 admit，R9 B1）
#[test]
fn endpoint_admit_blocks_anonymous_catalog() { ... }
// Given modules 白名单配置为 ["不存在的模块"]
// When tools/list
// Then 空 tools 数组（fail-closed，不得回落全集）
#[test]
fn modules_whitelist_is_fail_closed() { ... }
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**（先纯组帧函数 + 单测，再 handler）。
- [ ] **Step 4: 测试通过 + fmt/clippy**。
- [ ] **Step 5: Commit** `feat(serve): {base}/mcp——streamable HTTP stateless 子集（R3/R5/R6）`。

### Task 7: `POST {base}/rpc`（裸 JSON-RPC 2.0）

**Files:**
- Modify: `serve/src/mcp_http.rs`（`pub async fn rpc_handle(...)`）、`serve/src/lib.rs`（`pipeline.rpc` 时挂 `post("{p}/rpc", ...)`）

**Interfaces:**
- Consumes: Task 6 的全部（method = **原始 operationId**，非净化名）。
- 语义差异（spec 线路 B 节）：`code!=0` → JSON-RPC error（裸 RPC 无 isError 概念）；其余同 Task 6 组帧。

- [ ] **Step 1: BDD 失败测试**：未知 method → `-32601`；业务 code!=0 → JSON-RPC error 且 error.message 含 msg；batch → `-32600`。
- [ ] **Step 2–4: 实现 + 测试 + 门禁**，同 Task 6 节奏。
- [ ] **Step 5: Commit** `feat(serve): {base}/rpc 裸 JSON-RPC 2.0（method=operationId）`。

### Task 8: B 线路 e2e（oj/tests/e2e.rs）

**Files:**
- Modify: `oj/tests/e2e.rs`

- [ ] **Step 1: BDD 用例**（Given/When/Then 命名，复用 harness 起真实 bin 的模式）

```rust
// Given sample 服务 + server.mcp.enabled=true（临时 config）
// When POST /v1/api/mcp initialize → tools/list
// Then tools/list 含 user/account 端点的 tool（净化名），inputSchema 含契约字段
#[tokio::test(flavor = "current_thread")]
async fn mcp_e2e_initialize_list_call() { ... }
// Given 同上
// When tools/call 调一个 code!=0 的端点
// Then HTTP 200 且 result.isError==true 且 content[0].text 含业务 msg
#[tokio::test(flavor = "current_thread")]
async fn mcp_e2e_business_error_is_iserror_result() { ... }
// Given 无 Bearer 且端点非匿名
// When tools/call
// Then isError:true 且内容含 401 语义（admit 拒绝被捕获）
#[tokio::test(flavor = "current_thread")]
async fn mcp_e2e_auth_failure_surfaces_as_iserror() { ... }
// Given 未开配置门
// When POST /v1/api/mcp
// Then 404
#[tokio::test(flavor = "current_thread")]
async fn mcp_e2e_disabled_gate_404() { ... }
// Given 开配置门 + auth 已装配 + 无 Bearer
// When POST /v1/api/mcp initialize
// Then HTTP 401（R9 B1 endpoint 级 admit，匿名目录不可用）
#[tokio::test(flavor = "current_thread")]
async fn mcp_e2e_anonymous_initialize_rejected_401() { ... }
// Given modules 白名单=["不存在"] 的临时 config
// When tools/list（带合法 Bearer）
// Then tools 为空数组（fail-closed）
#[tokio::test(flavor = "current_thread")]
async fn mcp_e2e_modules_whitelist_fail_closed() { ... }
```

- [ ] **Step 2: 跑用例（先红后绿，Task 6/7 已绿则直接绿——补的是覆盖）**。
- [ ] **Step 3: `cargo test --release -p oj --test e2e mcp` + fmt/clippy**。
- [ ] **Step 4: Commit** `test(oj): MCP e2e——list/call/isError/auth/门控`。

**阶段小结点**：线路 B 完整可用（URL 型 MCP + 裸 JSON-RPC）。

---

## Phase 4：线路 A（`oj mcp` codegen）

### Task 9: `oj mcp` 子命令与模板渲染

**Files:**
- Create: `oj/src/mcp_cmd.rs`
- Modify: `oj/src/args.rs`（`McpArgs` 仿 `OpenApiArgs`：`-c/--config`、`-d/--dir`、`--base`、`--module-root`、`-o/--out` 缺省 `mcp/mcp-server.js`）
- Modify: `oj/src/lib.rs`（`pub mod mcp_cmd;`）与 `oj/src/main.rs`（子命令分发）

**Interfaces:**
- Produces: `pub async fn run(a: &McpArgs) -> Result<i32, String>`；渲染纯函数
  `pub fn render_server(tools: &[serve::mcp::McpTool], spec_hash: &str, base: &str) -> String`（便于单测，不碰盘）。
- 模板纪律（spec 线路 A + R9 节，逐字）：NDJSON stdio；stdout 纯净（一切诊断 `console.error`）；方法集 initialize（含协商+capabilities+serverInfo）/ping/tools/list/tools/call；通知不回写；内嵌 `toolName → {method, path模板, operationId}` 表 + tool 数组 + spec_hash；path 单段 `encodeURIComponent`、catch-all 原样；query 值 String() 化；**解析不 gate 在 res.ok**；content-type `text/event-stream` 或 AbortSignal.timeout(30s) → isError「streaming endpoint, not supported via MCP」；token/base 全走 `process.env.OJ_TOKEN`/`OJ_BASE`；**R9：fetch 加 `redirect:"manual"`（3xx 当错误）；启动时 `OJ_BASE` 为非 localhost `http://` → stderr 警告 Bearer 明文；读 `-c config.yaml` 的租户头名（非密钥）内嵌，值取 `OJ_TENANT` env，未配置租户则不生成该头**。

- [ ] **Step 1: BDD 失败测试**（render_server 纯函数断言）

```rust
// Given 两个 tool（其一 catch-all）
// When render_server
// Then 产物含两表项；catch-all 参数的填充代码走 decodeURI 分支（不 encodeURIComponent）
#[test]
fn render_embeds_tool_map_with_catchall_branch() { ... }
// Given 渲染产物
// Then 含 "console.error" 且不含 "console.log"
#[test]
fn generated_script_never_writes_stdout_logs() { ... }
// Given 渲染产物
// Then 含 "structuredContent" 与 "isError" 与 "AbortSignal.timeout"
#[test]
fn generated_script_implements_result_and_error_shapes() { ... }
// Given 渲染产物
// Then 含 "redirect: \"manual\""（或等价），且含 OJ_TENANT 租户头 env 消费
#[test]
fn generated_script_hardens_fetch_and_supports_tenant_header() { ... }
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**（模板用 `format!`/write! 拼，不引模板引擎）。
- [ ] **Step 4: 测试 + fmt/clippy**。
- [ ] **Step 5: Commit** `feat(oj): oj mcp 子命令——零依赖 stdio MCP server 生成（线路 A）`。

### Task 10: A 线路 env-gated smoke

**Files:**
- Modify: `oj/tests/e2e.rs`（或独立 `oj/tests/mcp_smoke.rs`）

- [ ] **Step 1: BDD 用例**（`OJ_E2E_NODE` env-gated，缺省 skip 并打印提示——诚实登记）

```rust
// Given OJ_E2E_NODE 已设置且 node ≥18 可用
// When oj mcp 生成 → spawn node 进程 → 写入 initialize/tools/list NDJSON
// Then 收到合法 initialize 结果且 tools/list 含已知 tool
#[tokio::test(flavor = "current_thread")]
async fn generated_stdio_server_handshakes() { ... }
```

- [ ] **Step 2: 本机验证（有 node 则跑通并保留 env-gate）→ Step 3: fmt/clippy → Step 4: Commit** `test(oj): oj mcp stdio smoke（env-gated）`。

**阶段小结点**：线路 A 完整可用（stdio 文件型 MCP）。

---

## Phase 5：版本与文档门禁

### Task 11: CHANGELOG + devkit 四件套 + 归置校验

**Files:**
- Modify: `CHANGELOG.md`
- Modify: `docs/devkit/api-manual.md`、`docs/devkit/scenarios.md`、`docs/devkit/SKILL.md`、`docs/devkit/README.md`（新增 `server.mcp.*`/`server.rpc` 配置、`{base}/mcp`、`{base}/rpc`、`oj mcp` 子命令；scenarios 给「三步接入 Claude Code」可复制示例含 client JSON）
- 运行: `cargo xtask build`（归置 bin/devkit/）

- [ ] **Step 1: 写文档（新 API/配置进 api-manual 对应章节及错误/限制表；陷阱进 SKILL.md——含「stdout 纯净」「token 走 env」「快照语义须重新生成」）**。
- [ ] **Step 2: `cargo xtask build && cargo test --release -p xtask`**（devkit 契约测试校验产物与源一致）。
- [ ] **Step 3: `cargo test --release --workspace` 终验 + fmt/clippy**。
- [ ] **Step 4: Commit** `docs: vX.Y.Z——MCP 双线路 + JSON-RPC（devkit 四件套同步）`。

---

## 执行约定

- **分阶段执行，每阶段结束输出小结**：Phase 1/2/3/4/5 末尾各一段（完成了什么 / 门禁结果 / 下阶段入口）。
- **任务跟踪**：所有 Step 用 `- [ ]` 勾选推进；每 Task 完成即 commit（消息按上表）。
- 每 Phase 完成后如用户在场先报小结再继续；用户不在场则连续推进到自然卡点（门禁红 / 需拍板）。
