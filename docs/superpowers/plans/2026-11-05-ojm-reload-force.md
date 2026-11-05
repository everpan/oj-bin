# oj-bin reload force 旁路 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 reload 触发面（HTTP `POST {base}/_system/config` 与 JS `server.reloadConfig`）加 `force` 旁路——force=true 跳过 unchanged 幂等闸门强制换代，为 ojm 安装器去 `ojm_install_seq` 哨兵铺路（安装器侧本次不改）。

**Architecture:** `ReloadExecutorLike` 闭包签名 `Fn()` → `Fn(bool)` 贯通三层（JS op → serve `ReloadState::trigger/trigger_sync` → `oj/src/serve_cmd.rs` 的 `do_reload`）；`do_reload` 的 unchanged 早退处加 `!force` 条件；HTTP 侧 force 取 query（body 仍强制空）。

**Tech Stack:** Rust（tokio/deno_core op2/axum）、deno_core `#[op2]`、oj e2e（真 HTTP 回环）。

**Spec:** `docs/superpowers/specs/2026-11-05-ojm-reload-force-design.md`

## Global Constraints

- 不传 force 时行为逐字节不变（向后兼容红线；`outcome_response` 线上形状不动）。
- `src/bridge/bootstrap.js` 必须保持 7-bit ASCII。
- 构建/测试/clippy 一律 `--release`；测试 `tokio::test(flavor = "current_thread")`；条件等待 deadline/有界轮询，禁固定 sleep。跑 cargo test 接管道必须 `set -o pipefail`；不得并发两个 cargo test。
- HTTP POST body 仍必须为空（既有 400 校验不动）；busy/409 串行化不被 force 绕过。
- 提交信息结尾加署名行：`unix@vip.qq.com ai`。

## Review Focus

1. JS 调用 `server.reloadConfig()` 不带参 / 带非布尔参 → op 收到 `None` → force=false（不得报参数错误）——Task 1 的 S8 测试钉死（default 腿）。
2. HTTP query `force=0` / `force=` / `force=yes` / 其它参数 → 一律视为未强制，行为等同无 query——Task 2 的 `query_flag` 单测 + e2e `?force=0` 腿钉死。
3. force + 零 diff → 仍影子装配换代：generation +1、`unchanged=false`、`applied=[]`；随后无 force 再 POST 恢复 unchanged（force 不污染闸门基线）——Task 2 e2e 钉死。
4. `?force=1` 在场时空 body 400 校验仍生效——Task 2 e2e 腿钉死。
5. force 与并发触发：仍走同一把 `reload_lock`（撞锁 409）——Task 1 改造后 S6 保持全绿即钉死。

---

### Task 1: executor 签名 `Fn(bool)` 贯通 + op/bootstrap force 参

**Files:**
- Modify: `src/bridge/reload_bridge.rs`（`ReloadExecutorLike` 定义 :48、`op_server_reload_config` :66-95、tests S1/S5/S6 闭包、新增 S8）
- Modify: `src/bridge/bootstrap.js`（`globalThis.server.reloadConfig` :371）
- Modify: `serve/src/reload.rs`（`trigger` :164、`trigger_sync` :175）
- Modify: `oj/src/serve_cmd.rs`（`build_reload_executor` 闭包 :449-464、slot 闭包 :357-358）

**Interfaces:**
- Produces: `pub type ReloadExecutorLike = Arc<dyn Fn(bool) -> Result<ReloadOutcome, (u16, String)> + Send + Sync>`；`op_server_reload_config(state, force: Option<bool>)`；`trigger(&self, force: bool)` / `trigger_sync(&self, force: bool)`；bootstrap `reloadConfig: (force) => op_server_reload_config(force)`。Task 2 依赖这些签名。
- 编译联动：本任务后全 workspace 必须仍编译通过（所有 executor 闭包调用点同步适配 `|force|` / `|_force|`）。

- [ ] **Step 1: 写失败测试 S8（force 透传）**

`src/bridge/reload_bridge.rs` tests：`api_src()` 增两个 handler：

```js
reload_force_true: async () => json.ok(await server.reloadConfig(true)),
reload_force_default: async () => json.ok(await server.reloadConfig()),
```

新增用例（形态照 S1）：slot 闭包用 `Arc<std::sync::Mutex<Vec<bool>>>` 记录每次收到的 force 值，闭包返回 `unchanged:false, generation:1, applied:["vars"]`；依次调 `run(&b,&base,"inst","reload_force_true")` 与 `reload_force_default`，断言记录序列 `== [true, false]`。

- [ ] **Step 2: 跑 S8 确认失败**

Run: `set -o pipefail; cargo test --release -p only-js s8_force 2>&1 | tail -20`
Expected: 编译失败（`reloadConfig` 不接受参数 / 闭包签名不匹配）——这就是失败态。

- [ ] **Step 3: 实现签名贯通**

- `reload_bridge.rs:48`：`Fn()` → `Fn(bool)`。
- `op_server_reload_config` 加第二参 `force: Option<bool>`；`spawn_blocking` 处改 `let f = force.unwrap_or(false);` 后 `exec(f)`（借 `st` 段不变）。
- `bootstrap.js:371`：`reloadConfig: (force) => op_server_reload_config(force),`（缺省 undefined → None；注释补一句 ASCII 英文说明 force 语义）。
- `serve/reload.rs`：`trigger(&self, force: bool)` / `trigger_sync(&self, force: bool)`，闭包调用 `exec(force)`；doc 注释同步（HTTP `?force=1` 语义、force 不绕 `reload_lock`）。
- `oj/src/serve_cmd.rs:357`：slot 闭包 `Arc::new(move |force: bool| rs_for_slot.trigger_sync(force))`；`:449` `build_reload_executor` 闭包改 `move |force: bool|`，线程体内 `do_reload(&ctx)` 暂传 `force` 占位（`do_reload(ctx, force)` 签名本任务先加参数、body 不用——Task 2 消费；为免 warning 可 `let _ = force;` 由 Task 2 移除）。
- tests 内既有闭包适配：S1/S6 的 `move ||`/`move |_|` → `move |_force|`，S5 `move ||` → `move |_force|`。

- [ ] **Step 4: 跑 reload_bridge 全部用例确认通过**

Run: `set -o pipefail; cargo test --release -p only-js reload_bridge 2>&1 | tail -20`
Expected: PASS（含 S1-S8）；`cargo fmt` 后无 diff。

- [ ] **Step 5: workspace 编译 + 提交**

Run: `set -o pipefail; cargo clippy --release --workspace --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 零告警。

```bash
git add -A && git commit -m "feat(reload): server.reloadConfig 增加 force 参——executor 闭包贯通 Fn(bool)

unix@vip.qq.com ai"
```

### Task 2: `do_reload` force 旁路 + HTTP `?force=1` + e2e

**Files:**
- Modify: `oj/src/serve_cmd.rs`（`do_reload` :467 签名加 `force: bool`；unchanged 早退 :497 加 `!force`；移除 Task 1 占位 `let _ = force;`）
- Modify: `serve/src/reload.rs`（`handle_config` POST 分支 :310-346；新增 `query_flag` 纯函数 + tests）
- Test: `oj/tests/e2e.rs`（`given_reload_api_when_post_config_then_swaps_and_reports_diff` :2140 起追加 force 腿）

**Interfaces:**
- Consumes: Task 1 的 `trigger(force)` / executor `Fn(bool)`。
- Produces: `do_reload(ctx: &HotReloadCtx, force: bool)`；`fn query_flag(query: Option<&str>, key: &str) -> bool`（serve/reload.rs，私有纯函数）。

- [ ] **Step 1: 写 `query_flag` 失败测试**

`serve/src/reload.rs` 既有 `#[cfg(test)] mod tests` 追加：

```rust
#[test]
fn query_flag_parses_force_values() {
    assert!(super::query_flag(Some("force=1"), "force"));
    assert!(super::query_flag(Some("force=true"), "force"));
    assert!(super::query_flag(Some("FORCE=TRUE"), "force")); // 值不区分大小写
    assert!(!super::query_flag(Some("force=0"), "force"));
    assert!(!super::query_flag(Some("force="), "force"));
    assert!(!super::query_flag(Some("force=yes"), "force"));
    assert!(!super::query_flag(Some("other=1&x=2"), "force"));
    assert!(!super::query_flag(None, "force"));
}
```

- [ ] **Step 2: 跑测确认失败**

Run: `set -o pipefail; cargo test --release -p serve query_flag 2>&1 | tail -10`
Expected: FAIL（`query_flag` 未定义）。

- [ ] **Step 3: 实现 `query_flag` + 接线 POST + do_reload 旁路**

- `serve/src/reload.rs`：

```rust
/// query 布尔解析：`force` 取 `1`/`true`（值不区分大小写），其余/缺省 = false。
fn query_flag(query: Option<&str>, key: &str) -> bool {
    query.unwrap_or("").split('&').any(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        k == key && matches!(v.to_ascii_lowercase().as_str(), "1" | "true")
    })
}
```

`handle_config` POST 分支：空 body 400 校验**之后**取 `let force = query_flag(req.uri().query(), "force");`——注意 `req` 已被 `into_body()` 消耗，须在 `axum::body::to_bytes` **之前**先取 `req.uri().query()` 存局部变量；`rs.trigger().await` → `rs.trigger(force).await`。
- `oj/src/serve_cmd.rs`：`do_reload(ctx: &HotReloadCtx, force: bool)`；`:497` 闸门 `if !force && sha_same && locks_same {` 返回 unchanged；删占位行；doc 注释补「force=true：跳过幂等闸门强制换代（applied 可空）」。

- [ ] **Step 4: 跑单测确认通过**

Run: `set -o pipefail; cargo test --release -p serve query_flag 2>&1 | tail -5`
Expected: PASS。

- [ ] **Step 5: 写 e2e force 腿（失败测试）**

`oj/tests/e2e.rs` 该用例内「再 POST（无改动）→ 仍 unchanged，代际保持 1」（~:2254-2257）之后追加（`req` helper 原样复用，路径带 query）：

```rust
// force 旁路（v0.1.123）：?force=1 零改动仍强制换代——applied 空、代际推进。
let (st, body) = req(addr, "POST", "/v1/api/_system/config?force=1", None).await;
assert_eq!(st, 200, "force post ok");
assert_ne!(body["msg"].as_str(), Some("unchanged"), "{body}");
assert_eq!(body["data"]["applied"], serde_json::json!([]), "{body}");
let gen_forced = body["data"]["generation"].as_u64().unwrap();
assert!(gen_forced > 1, "generation must advance on force");
// force 不污染闸门基线：随后无 force 无改动 → 恢复 unchanged。
let (st, body) = req(addr, "POST", "/v1/api/_system/config", None).await;
assert_eq!(st, 200);
assert_eq!(body["msg"].as_str(), Some("unchanged"), "{body}");
// 非 1/true 值 = 未强制。
let (st, body) = req(addr, "POST", "/v1/api/_system/config?force=0", None).await;
assert_eq!(body["msg"].as_str(), Some("unchanged"), "{body}");
// ?force=1 不豁免空 body 校验。
let (st, _) = req(addr, "POST", "/v1/api/_system/config?force=1", Some(serde_json::json!({"x":1}))).await;
assert_eq!(st, 400, "force must not bypass empty-body rule");
```

（`req` 的 body 参数形态以文件内既有调用为准——先看 ~:2241 行的写法再照抄。）

- [ ] **Step 6: 跑 e2e 用例确认通过**

Run: `set -o pipefail; cargo test --release -p oj --test e2e given_reload_api_when_post_config 2>&1 | tail -10`
Expected: PASS。

- [ ] **Step 7: 全量门禁 + 提交**

Run: `set -o pipefail; cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 零输出/零告警。

```bash
git add -A && git commit -m "feat(reload): force 跳过 unchanged 闸门——HTTP ?force=1 + do_reload 旁路

unix@vip.qq.com ai"
```

### Task 3: 文档 + 版本 bump（v0.1.123 发布点）

**Files:**
- Modify: `CHANGELOG.md`（顶部新增版本节）
- Modify: `oj/Cargo.toml:3`（`0.1.122` → `0.1.123`；`cargo build` 自动同步 `Cargo.lock`）
- Modify: `docs/devkit/api-manual.md` / `docs/devkit/SKILL.md` / `docs/devkit/scenarios.md` / `docs/devkit/README.md`（四件对齐）

**Interfaces:**
- Consumes: Task 1/2 的最终用户可见行为：`server.reloadConfig(force?)`、`POST {base}/_system/config?force=1`。

- [ ] **Step 1: CHANGELOG 新节**

`CHANGELOG.md`「下一版（未发布）」语义：新节标题 `## v0.1.123（2026-11-05）`（若暂不打标签注明「未打标签」，发布时再补 `git tag -a v0.1.123`），内容：
- `feat(reload)`：HTTP `POST {base}/_system/config?force=1` 与 JS `server.reloadConfig(force)` 支持 force 旁路——force=true 跳过 unchanged 幂等闸门强制影子装配换代（applied 可为空；busy/空 body 校验不豁免）；为 ojm 安装器去 `ojm_install_seq` 哨兵铺路（安装器侧采纳另行交付）。
- fix 组：无则不设。

- [ ] **Step 2: devkit 四件逐条对齐**

- `api-manual.md`：`server.reloadConfig` 签名处加 `force?: boolean`（语义：跳过幂等闸门强制换代；forced 无 diff 时 `applied=[]`、`unchanged=false`）；HTTP 端点节加 `?force=1`（取值 `1`/`true` 不区分大小写）；**错误/限制表**加两行：force 不豁免 busy（并发 409）与空 body（400）、force 不改变闸门基线（后续无 force 仍 unchanged）。
- `SKILL.md` 陷阱速查：加一条——「想强制热更不要改 config 凑 diff：`reloadConfig(true)` / `?force=1`；unchanged 从此是幂等成功语义（活代已与盘上一致），不再是失败」。
- `scenarios.md`：加「强制重载配置」可照抄场景（JS 一段 + curl 一段）。
- `README.md`：若含 API/版本索引则同步 reloadConfig 行为描述，无则跳过（执行时以文件实际结构为准）。

- [ ] **Step 3: 版本 bump + 归置 + 契约校验**

```bash
# oj/Cargo.toml version = "0.1.123"
cargo xtask build            # 归置 bin/oj + bin/devkit（Cargo.lock 同步）
cargo test --release -p xtask 2>&1 | tail -5   # devkit 契约用例：产物与源一致
```
Expected: xtask 测试 PASS。

- [ ] **Step 4: 全量门禁**

```bash
set -o pipefail; cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings && cargo test --release --workspace 2>&1 | tail -15
```
Expected: 全绿（注意：不并发第二个 cargo test）。

- [ ] **Step 5: 发布点提交 + 打标签**

```bash
git add -A && git commit -m "chore(release): v0.1.123——reload force 旁路（HTTP ?force=1 / reloadConfig(force)）+ devkit 四件同步

unix@vip.qq.com ai"
git tag -a v0.1.123 -m "v0.1.123: reload force 旁路——强制热更不再需要凑 config diff"
# push 由用户决定；未 push 前文档「未打标签」口径以实际 git tag 为准
```

（设计/计划文档随 Task 1 提交前单独入一个 docs commit：`docs(spec): ojm reload force 设计 + 实施计划`，署名行同。）
