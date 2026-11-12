# MCP 工具分组（L1）与检索路由（L2）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `server.mcp.expose: grouped|routed` 两模式落地——grouped 给 tools/list 附 `_meta` 模块分组，routed 只出 `oj_search_tools`/`oj_list_groups` 两个元工具 + 调用面短路；两条线路（serve `/mcp` 与 `oj mcp` stdio 快照）同模式同语义。

**Architecture:** 全部改动是渲染层叠加：既有管线 `tools_for → filter_tools_for_display`（modules→scopes/act_mask 裁剪）之后按 mode 分三种 list 形态；`oj_*` 名在 call_tool 的 tools_for **之前**短路到内建 meta handler（meta handler 内部复用同款裁剪）。纯函数（build_groups/search_tools/check_reserved_names）集中可单测；Rust 与 JS shim 两份检索被同一份 JSON 一致性向量钉死。

**Tech Stack:** Rust（serve/oj/config crate）、serde_json、零依赖 Node.js 模板字符串。

**Spec:** `docs/superpowers/specs/2026-11-12-mcp-tool-grouping-routing-design.md`（本计划逐条对 spec 裁决；冲突以 spec 为准）

## Global Constraints

- 一切构建/测试/clippy 走 `--release`；禁 debug 构建（`.cargo/config.toml` alias 已是 release）。
- 提交尾行裸行 `unix@vip.qq.com ai`；**禁止** Co-Authored-By trailer。
- flat 模式 tools/list 输出**字节不变**（反目标）；既有 e2e 全绿是每 Task 的隐性验收。
- `oj_search_tools`/`oj_list_groups` 的 annotations 恒为 `{"readOnlyHint":true,"idempotentHint":true}`；search matches 透传源 tool 的 annotations。
- 检索集合 = `filter_tools_for_display` 输出（serve 侧）；JS shim 侧 = 产物内嵌全集（已知分歧，spec 专节）。
- groups 键路径：`_meta["com.only-js.groups"]`；元素 `{name, tool_count}`（v1 无 description）。
- `limit` 缺省 10、上限 50（超限钳 50）；空组省略；无命中 `{matches:[]}`。
- 版本 bump 到 v0.1.134 + CHANGELOG + devkit 四件同步是 Task 7 的一部分（CLAUDE.md 红线）。

## Review Focus

1. **routed 下空业务集仍出元工具**——既有 modules fail-closed e2e 断言空列表，routed 模式语义变了，旧用例需按模式分支（Task 6）。
2. **search 与 tools/list 同一裁剪集合**——防探测同形状（无权≡不存在）是核心安全性质（Task 3）。
3. **annotations 透传**——routed 下 destructiveHint 丢失=比 flat 更不安全（Task 3）。
4. **flat 零变化**——`_meta` 不得出现在 flat 响应；既有按 `result.tools` 索引的 e2e 是回归网（Task 2/6）。
5. **operationId 净化出 `oj_` 名**（如 `oj.search-tools` → `oj_search_tools`）——扫描必须基于净化后名，兜住 /rpc 与 /mcp 分歧（Task 1）。

---

### Task 1: ExposeMode 枚举 + 配置链路 + `oj_` 保留名 fail-fast

**Files:**
- Modify: `serve/src/mcp.rs`（ExposeMode、check_reserved_names、meta_tool_defs）
- Modify: `src/config.rs:135`（McpCfg.expose）
- Modify: `serve/src/lib.rs:222`（McpConfig.expose）
- Modify: `oj/src/app.rs:2359`（装配 expose）
- Modify: `oj/src/args.rs:132`（McpArgs --expose）
- Modify: `oj/src/serve_cmd.rs`（启动扫描接线，mount 表装配处）
- Modify: `oj/src/mcp_cmd.rs:255`（codegen 扫描接线）
- Test: `serve/src/mcp.rs` tests mod、`src/config.rs` tests、`oj/src/mcp_cmd.rs` tests

**Interfaces:**
- Produces:
  - `serve::mcp::ExposeMode { Flat, Grouped, Routed }`（`#[derive(Clone,Copy,Debug,PartialEq,Eq,Default)]`，Default=Flat；serde `#[serde(rename_all="lowercase")]`）
  - `serve::mcp::META_PREFIX: &str = "oj_"`
  - `serve::mcp::check_reserved_names(tools: &[McpTool]) -> Result<(), String>`（Err 文案含肇事净化后名 + operationId + oa_path）
  - `serve::mcp::meta_tool_defs() -> Vec<Value>`（两个 tool JSON，annotations 恒 readOnlyHint+idempotentHint）
  - `config::McpCfg.expose: Option<ExposeMode>`（缺省 None = flat）
  - `serve::McpConfig.expose: ExposeMode`
  - `args::McpArgs.expose: Option<ExposeMode>`
  - `mcp_cmd::run` 决议：`a.expose.or(cfg.server.mcp.expose).unwrap_or(Flat)`

- [ ] **Step 1: 写失败测试（serve/src/mcp.rs tests）**

```rust
// check_reserved_names: 无 oj_ 名 → Ok；净化后可撞名 → Err 且文案含 operationId
#[test]
fn reserved_prefix_rejects_sanitized_collisions() {
    let ok = spec(&[("/a", "get", op("user_list", json!({})))]);
    assert!(check_reserved_names(&spec_to_tools(&ok).unwrap()).is_ok());
    // "oj.search-tools" 净化后 = "oj_search_tools"，与元工具撞名
    let bad = spec(&[("/a", "get", op("oj.search-tools", json!({})))]);
    let err = check_reserved_names(&spec_to_tools(&bad).unwrap()).unwrap_err();
    assert!(err.contains("oj_search_tools") && err.contains("oj.search-tools"), "{err}");
}

// meta_tool_defs: 恰两个、名/annotations 逐字
#[test]
fn meta_tool_defs_shape() {
    let defs = meta_tool_defs();
    assert_eq!(defs.len(), 2);
    let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["oj_search_tools", "oj_list_groups"]);
    for d in &defs {
        assert_eq!(d["annotations"]["readOnlyHint"], true);
        assert_eq!(d["annotations"]["idempotentHint"], true);
    }
    // inputSchema 存在（MCP tool 必需键）
    assert!(defs[0].get("inputSchema").is_some() && defs[1].get("inputSchema").is_some());
}
```

- [ ] **Step 2: 写失败测试（config/args 解析）**

`src/config.rs` tests（无 tests mod 则建）：yaml `expose: grouped`/`routed`/`flat` 各解析为对应枚举；`expose: bogus` parse Err；缺省 None。`oj/src/args.rs`：`["mcp", "--expose", "routed"]` 解析为 Some(Routed)、非法值报错、`--expose` 缺省 None（args 测试模式参照 args.rs 既有 clap 测试）。

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test --release -p only-js --lib mcp::` 与 `cargo test --release -p serve --lib mcp::` 与 `cargo test --release -p oj args`
Expected: 编译失败（符号不存在）或断言失败。

- [ ] **Step 4: 实现**

- `serve/src/mcp.rs`：`ExposeMode`（serde lowercase）、`META_PREFIX`、`check_reserved_names`、`meta_tool_defs`（两个 tool JSON 内联）。
- `src/config.rs:135` McpCfg 加 `#[serde(default)] pub expose: Option<serve::mcp::ExposeMode>`（config crate 已依赖 serve？若无，则在本 crate 定义镜像枚举 `ExposeCfg` 并在 app.rs 转换——实现者按依赖现状二选一，以不改依赖图为先）。
- `serve/src/lib.rs:222` McpConfig 加 `pub expose: ExposeMode`（Default derive 自动 Flat）。
- `oj/src/app.rs:2359` McpConfig 装配补 `expose: mcp_cfg.expose.unwrap_or_default()`。
- `oj/src/args.rs` McpArgs 加 `pub expose: Option<ExposeMode>`（clap `value_enum` 或 `value_parser`，词表 flat/grouped/routed）。
- 启动扫描接线：`oj/src/serve_cmd.rs` mount 表装配处（RuntimeMount::Api 的 table 齐备后、serve 启动前），对每个 `pipeline.mcp.is_some()` 的 mount：`openapi::generate` + `spec_to_tools` + `check_reserved_names`，Err → 启动 fail-fast（文案透传）。
- `oj/src/mcp_cmd.rs::run`：spec_to_tools 后调 `check_reserved_names`，Err → 返回 Err（非零退出）。

- [ ] **Step 5: 跑测试确认通过 + clippy**

Run: `cargo test --release -p serve --lib mcp:: && cargo test --release -p oj --lib && cargo test --release -p only-js --lib mcp::` 后 `cargo clippy --release -p serve -p oj -p only-js --all-targets -- -D warnings`
Expected: 全绿。

- [ ] **Step 6: Commit**

```bash
git add serve/src/mcp.rs serve/src/lib.rs src/config.rs oj/src/app.rs oj/src/args.rs oj/src/serve_cmd.rs oj/src/mcp_cmd.rs
git commit -m "feat(mcp): ExposeMode 三态枚举 + 配置链路 + oj_ 保留名 fail-fast（L1/L2 基座）"
```
（尾行 `unix@vip.qq.com ai`）

---

### Task 2: serve L1 grouped——build_groups + `_meta` 接线

**Files:**
- Modify: `serve/src/mcp_http.rs`（build_groups + handle 的 tools/list 分支）
- Test: `serve/src/mcp_http.rs` tests mod

**Interfaces:**
- Consumes: Task 1 的 `ExposeMode`、既有 `filter_tools_for_display`、`McpTool.tool["tags"]`
- Produces:
  - `pub fn build_groups(tools: &[McpTool]) -> Value`（`[{name, tool_count}]`，按首次出现序，空组/无 tag 天然不会出现——输入即裁剪后集合）
  - tools/list result（grouped 模式）形状：`{"tools": [...], "_meta": {"com.only-js.groups": [...]}}`

- [ ] **Step 1: 写失败测试**

```rust
// build_groups: tags 聚合、计数、按出现序、无 tag 排除
#[test]
fn build_groups_aggregates_module_tags() {
    let mk = |name: &str, tags: Value| McpTool { name: name.into(), tool: json!({"name": name, "tags": tags}), dispatch: /* 同 mcp.rs 测试的构造 */ };
    let tools = vec![
        mk("a_get", json!(["wiki"])),
        mk("b_get", json!(["wiki"])),
        mk("c_get", json!(["user"])),
        mk("d_get", json!([])),        // 无 tag 不进组
    ];
    let g = build_groups(&tools);
    let arr = g.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["name"], "wiki");
    assert_eq!(arr[0]["tool_count"], 2);
    assert_eq!(arr[1]["name"], "user");
    assert_eq!(arr[1]["tool_count"], 1);
    assert!(arr.iter().all(|g| g.get("description").is_none()), "v1 无 description");
}
```

另加一用例：`handle` 级 grouped 响应含 `_meta["com.only-js.groups"]` 且 tools 与 flat 一致、flat 响应**无** `_meta` 键（用既有的 AppState 测试构造模式，参照 mcp_http.rs:714 起的测试）。

- [ ] **Step 2: 跑测试确认失败** — `cargo test --release -p serve grouped`
- [ ] **Step 3: 实现**

`build_groups`（纯函数，tags[0] 为组名——McpTool 的 tags 是单元素数组，取首个字符串；防御非字符串跳过）。`handle()` tools/list 分支（mcp_http.rs:465-477）：match `st.pipeline.mcp.as_ref().map(|m| m.expose)`：
- Flat → 现状 json 原样
- Grouped → 现状 json + 插入 `"_meta": {"com.only-js.groups": build_groups(&shown)}`（shown 即裁剪后集合）

- [ ] **Step 4: 跑测试 + 既有 serve 全量** — `cargo test --release -p serve` 全绿；flat 零变化由既有用例回归。
- [ ] **Step 5: Commit** — `feat(mcp): L1 grouped——tools/list _meta 分组呈现（裁剪后集合，flat 不变）`

---

### Task 3: serve L2 routed——meta handler + search + user 穿透

**Files:**
- Modify: `serve/src/mcp_http.rs`（search_tools、run_meta_tool、call_tool 签名 + 短路、handle 的 user 穿透与 list 分支）

**Interfaces:**
- Consumes: Task 1 `meta_tool_defs`、Task 2 `build_groups`、既有 `filter_tools_for_display`
- Produces:
  - `pub fn search_tools(tools: &[McpTool], query: &str, group: Option<&str>, limit: usize) -> Value`（`{matches: [{name, description, inputSchema, group, annotations?}]}`；limit 钳 50；**固定全量扫描后排序**，不提前退出）
  - `fn run_meta_tool(name: &str, args: Option<&Map<String,Value>>, tools: &[McpTool], act_mask: bool, user: Option<&Value>) -> Result<Value, (&'static str, String)>`
  - `async fn call_tool(..., user: Option<&Value>)`（签名加参；`oj_` 短路在 `tools_for` **之前**）

- [ ] **Step 1: 写失败测试**

```rust
// search_tools: 大小写不敏感子串、name 命中优先于 description、limit 钳 50、
// group 精确过滤、无命中 {matches:[]}、annotations 透传、matches 含 inputSchema+group
#[test]
fn search_tools_matches_and_ranks() { /* 构造 4-5 个带 tags/annotations 的 McpTool：
   query "user" 命中 name 的排在仅 description 命中的前面；
   query "zzz-no-hit" → matches 为空数组；
   limit 100 → 实际 ≤50；
   group "wiki" 只回 wiki 组；
   源 tool 有 annotations → match 项携带同名键，无 annotations → 键缺席 */ }
```

防探测同形状用例：`filter_tools_for_display` 裁掉的工具不得出现在 search_tools 结果（构造 scopes 裁剪场景，meta handler 级）。

- [ ] **Step 2: 跑测试确认失败** — `cargo test --release -p serve search`
- [ ] **Step 3: 实现**

1. `search_tools`：对 `tools` 全量扫描（固定路径），每个 tool 算 score（name 含 query 子串=2，description 含=1，group tag 含=1；都大小写不敏感；可叠加），score>0 且 group 过滤（Some 时 tags 含该组）者入候选；按 (score desc, name asc) 排序，取前 limit（>50 钳 50）；match 项 `{name, description, inputSchema, group: tags[0]?, annotations?}`——annotations 原样 clone，无则缺席。
2. `run_meta_tool`：`oj_search_tools` → 解析 query（必填，缺 → -32602）/group/limit（缺省 10）；内部 `tools_for` + `filter_tools_for_display(tools, user, act_mask)` 得裁剪集，调 search_tools。`oj_list_groups` → `{groups: build_groups(裁剪集)}`。未知名 → -32601。
3. `call_tool` 加 `user: Option<&Value>` 形参；函数体第一句：`if name.starts_with(META_PREFIX) { return run_meta_tool(name, args, /* tools 由 run_meta_tool 内部算 */, act_mask, user); }`——**在 tools_for 之前**（openapi generate 报错不得把元工具打成 -32603；act_mask 从 `st.pipeline` 取）。`handle()` 的 tools/call 分支把 `user.as_ref()` 传进（mcp_http.rs:479）。
4. `handle()` tools/list 分支：Routed → `{"tools": meta_tool_defs()}`（**不**附 _meta；组面走 oj_list_groups）。

- [ ] **Step 4: 跑测试 + clippy + serve 全量** — `cargo test --release -p serve`；`cargo clippy --release -p serve --all-targets -- -D warnings`
- [ ] **Step 5: Commit** — `feat(mcp): L2 routed——oj_search_tools/oj_list_groups 元工具 + call 面短路（annotations 透传，防探测同形状）`

---

### Task 4: 共享一致性向量 fixtures

**Files:**
- Create: `serve/src/fixtures/mcp_search_vectors.json`
- Modify: `serve/src/mcp_http.rs` tests（向量驱动用例）

**Interfaces:**
- Produces: `serve/src/fixtures/mcp_search_vectors.json` 形状
  `{"cases":[{"name":"...","tools":[{"name","description","tags"}],"query":"...","group":null,"limit":10,"expect":["name1","name2"]}]}`——expect 为命中名**有序**列表。

- [ ] **Step 1: 写向量文件**（≥6 例：name 命中优先序、description 命中、大小写混合、group 过滤、无命中空数组、limit 钳制）
- [ ] **Step 2: 写 Rust 向量测试**

```rust
#[test]
fn search_tools_honors_shared_vectors() {
    let v: Value = serde_json::from_str(include_str!("fixtures/mcp_search_vectors.json")).unwrap();
    for c in v["cases"].as_array().unwrap() {
        let tools: Vec<McpTool> = /* 由 c["tools"] 构造（inputSchema 占位） */;
        let out = search_tools(&tools, c["query"].as_str().unwrap(),
            c["group"].as_str(), c["limit"].as_u64().unwrap_or(10) as usize);
        let got: Vec<&str> = out["matches"].as_array().unwrap().iter()
            .map(|m| m["name"].as_str().unwrap()).collect();
        let want: Vec<&str> = c["expect"].as_array().unwrap().iter()
            .map(|e| e.as_str().unwrap()).collect();
        assert_eq!(got, want, "向量 {} 不符", c["name"]);
    }
}
```

- [ ] **Step 3: 跑通** — `cargo test --release -p serve vectors`
- [ ] **Step 4: Commit** — `test(mcp): search 共享一致性向量（Rust/JS 双实现同钉）`

---

### Task 5: 线路 A——`oj mcp --expose` + 模板三模式

**Files:**
- Modify: `oj/src/mcp_cmd.rs`（render_server 签名、TEMPLATE、run 决议+扫描+groups）

**Interfaces:**
- Consumes: Task 1 `check_reserved_names`/`meta_tool_defs`/ExposeMode、Task 2 `build_groups`（serve::mcp_http 导出纯函数）
- Produces:
  - `render_server(tools: &[McpTool], spec_hash: &str, base: &str, tenant_header: Option<&str>, expose: ExposeMode, groups: &Value) -> String`
  - 模板新占位：`__EXPOSE__`（`"flat"|"grouped"|"routed"`）、`__GROUPS_JSON__`

- [ ] **Step 1: 改既有失败测试（签名变化）+ 写新失败测试**

`mcp_cmd.rs` 既有 render_server 调用点补 `ExposeMode::Flat, &json!([])`（flat 默认组——既有 5 个渲染断言应保持原样通过，钉住 flat 产物字节不变）。新增：

```rust
// grouped: 产物 __EXPOSE__ 为 grouped、__GROUPS_JSON__ 内嵌、tools/list 分支带 _meta
#[test] fn render_grouped_exposes_meta_groups() { /* js.contains("\"grouped\"")（EXPOSE 行）；
   js.contains(r#""com.only-js.groups""#)；js.contains("\"_meta\"")（list 分支） */ }

// routed: 产物只列元工具 + callTool 的 oj_ 前缀拦截先于 DISPATCH 查找 + 本地 searchTools 在场
#[test] fn render_routed_lists_only_meta_tools() { /* js 含 "oj_search_tools" 与 "oj_list_groups"（META 定义）；
   js 含 "function searchTools" 或等值本地过滤函数；
   callTool 入口含 oj_ 拦截分支（字符串断言拦截在 "const d = DISPATCH" 之前——用索引比较 position） */ }
```

- [ ] **Step 2: 跑测试确认失败** — `cargo test --release -p oj --lib mcp`
- [ ] **Step 3: 实现**

1. TEMPLATE：
   - 头部加 `const EXPOSE = "__EXPOSE__";` 与 `const GROUPS = __GROUPS_JSON__;`
   - `searchTools(query, group, limit)` 本地函数：对 `TOOLS` 全量扫描，匹配规则与 Task 3 Rust 版**逐条一致**（大小写不敏感子串；name=2/description=1/group=1；score desc, name asc；limit 钳 50；match 项同键集）。
   - `callTool` 入口：`if (name.indexOf("oj_") === 0) return callMeta(name, args);`（**先于** `DISPATCH[name]`）；`callMeta`：`oj_search_tools` → 本地 searchTools 回 `{content:[{type:"text",text:JSON.stringify(result)}], structuredContent: result}`；`oj_list_groups` → GROUPS；未识别 → isError。
   - tools/list 分支：`EXPOSE === "routed"` → `{tools: [META_SEARCH, META_LISTGROUPS]}`；`grouped` → `{tools: TOOLS, _meta: {"com.only-js.groups": GROUPS}}`；flat → 现状 `{tools: TOOLS}`。
   - `META_SEARCH`/`META_LISTGROUPS` 两个 const（由 `__META_JSON__` 注入或模板内联，与 meta_tool_defs 输出逐字一致——选模板内联 + Rust 侧字符串断言对照，省去新占位）。
2. `render_server` 加两参，`run()`：`expose = a.expose.or(cfg.server.mcp.expose).unwrap_or(Flat)`；`check_reserved_names(&tools)?`；`groups = build_groups(&tools)`；传参渲染。epilogue 打印模式。

- [ ] **Step 4: 跑测试** — `cargo test --release -p oj --lib`
- [ ] **Step 5: Commit** — `feat(oj-mcp): --expose 三模式渲染（grouped _meta / routed 元工具本地检索）`

---

### Task 6: e2e——三模式端点闭环 + 既有用例模式分支

**Files:**
- Modify: `oj/tests/e2e.rs`（mcp 端点用例区 ~3304/3349/3401 一带）

- [ ] **Step 1: 写失败 e2e**

参照既有 mcp e2e 的 serve 驱动模式：
1. **flat 回归**：既有用例原样全绿（改动后跑，零修改通过即回归证据）。
2. **grouped**：临时 config `expose: grouped` 起 serve → `tools/list` 响应含 `_meta["com.only-js.groups"]`、`tools` 数组与 flat 同集；flat 响应无 `_meta` 键。
3. **routed**：`expose: routed` → list 恰好 2 工具（oj_search_tools/oj_list_groups）；`tools/call oj_search_tools {query:"user"}` → matches 非空且各项含 inputSchema；`tools/call oj_list_groups` → groups；`tools/call <业务 tool>`（list 之外的 name）照常执行成功（调用面不变钉）；scopes 裁剪的 token search 不返回被裁工具（防探测钉）。
4. **既有 modules fail-closed 用例按模式分支**：routed 下空业务集 list 仍含 2 元工具（改断言或加 routed 变体）。
5. **stdio 向量腿**（若 e2e.rs:3401 驱动模式可复用）：node 跑 Task 5 产物，feed `tools/call oj_search_tools` 与 `serve/src/fixtures/mcp_search_vectors.json` 同向量，比对 matches 名序——node 不可用则诚实 skip（env 探测，同 repo 惯例）。

- [ ] **Step 2: 跑测试确认失败** — `cargo test --release -p oj --test e2e mcp`
- [ ] **Step 3: 实现** — e2e 用例代码
- [ ] **Step 4: 全量回归** — `cargo test --release -p oj --test e2e` + `cargo test --release --workspace`（接管道 set -o pipefail；不可并发两个 cargo test）
- [ ] **Step 5: Commit** — `test(mcp): e2e 三模式闭环 + fail-closed 按模式分支 + stdio 向量腿`

---

### Task 7: no-store + 版本/CHANGELOG/devkit 收口

**Files:**
- Modify: `serve/src/mcp_http.rs`（/mcp 响应头）
- Modify: `Cargo.toml`/`oj/Cargo.toml` 版本 → 0.1.134、`CHANGELOG.md`、`docs/devkit/` 四件

- [ ] **Step 1: 失败测试** — handle 响应含 `cache-control: no-store`（既有 handle 测试模式加断言）
- [ ] **Step 2: 实现** — `handle()` 与 `rpc_handle()` 响应统一加（评审 minor：防中介缓存按凭据裁剪结果）
- [ ] **Step 3: CHANGELOG v0.1.134 节**（expose 三模式 + `oj_` fail-fast 潜在破坏性明写 + routed 客户端策略限制 + 线路 A 已知分歧）
- [ ] **Step 4: devkit 四件**——api-manual「MCP」节加 expose 配置/三模式/元工具/限制表（spec 允许+客户端策略、子串匹配、线路 A 分歧、防探测口径=上下文占用非存在性）；SKILL 陷阱表两行（routed 客户端兼容、search 子串非语义）；scenarios 场景 77（照抄：grouped 折叠 + routed 检索两步走）；README 摘要一行
- [ ] **Step 5: 门禁** — `cargo fmt --check`；`cargo clippy --release --workspace --all-targets -- -D warnings`；`cargo test --release --workspace`；`cargo xtask build`（归置 bin/ + devkit）；`cargo test --release -p xtask`（devkit 契约 10/10）；`./bin/oj test -c sample/config.yaml -d sample/src`（L1 66/66 回归）
- [ ] **Step 6: Commit**（可分两个：docs(devkit) 一个）— `feat(mcp): v0.1.134——expose 三态落地 + no-store`

---

## Self-Review 记录

- spec 逐节覆盖：配置面✓T1、逐模式表✓T2/T3/T5、L1✓T2、L2✓T3、线路 A✓T5、错误限制（no-store✓T7、_meta✓T2、口径文档✓T7）、改面清单✓T1-T7、一致性向量✓T4/T6。
- Review Focus 5 条已分配到 T6(1,4)、T3(2,3)、T1(5)。
- 类型一致：ExposeMode 全程单一定义（config 若因依赖需镜像枚举，转换只在 app.rs）；build_groups/search_tools/meta_tool_defs 在 Task 2-5 间签名一致。
