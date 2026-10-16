# ojm 后端原生化实施计划（exec 地基 + 平台 API，BDD/TDD 分阶段）

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal**：按设计稿 v2 交付 6 个 PR 的平台侧原语与 CLI 改形，使 ojm 安装闭环可在后端 JS 原生完成。

**Architecture**：exec 全局以「argv 模板 + 路径占位符校验」为唯一外部命令通道；`platform:` 段统管特权（modules / trusted_paths / exec 矩阵）；PR-3 经 `OnceLock` 槽位回调复用 serve 热更处理体；PR-4 复用 exec 调增强后的 `oj migrate` CLI。

**Tech Stack**：ring 0.17（Ed25519）、tokio（process feature）、serde_yaml 0.9（既有）、deno_core op2（既有）。

**权威依据**：`docs/plans/2026-10-16-ojm-native-exec-platform-apis-design.md`（v2，下称「设计稿」）。语义冲突以设计稿为准。

**执行约定**（对 writing-plans 模板的两处仓库适配）：
1. **提交纪律**：模板的 per-task commit 改为**阶段级提交**，且须经用户确认后执行（禁自动 commit，提交信息中文、无 Co-Authored-By）。
2. **worktree**：实施前按 brainstorming/worktree 惯例建 worktree，并把主仓 `target/` **软链**进 worktree（勿用 `CARGO_TARGET_DIR`，夹具会红）。所有测试一律 `--release`。

**任务追踪**：本会话任务列表 #1~#9 与阶段 0~8 一一对应。

---

## 阶段 0：依赖 spike + feature 声明（任务 #1）

### Task 0.1：实证 ring 可达性

**Files**：无代码改动；结论写进本计划附录。

- Step 1: `cargo tree -i ring@0.17` —— 预期两条链：`ring ← jsonwebtoken ← only-js`（根 crate 直接链）与 serve 侧链；确认 lock 单版本 0.17.14。
- Step 2: 验证 Ed25519 无 feature 开关：`grep -n "ED25519" ~/.cargo/registry/src/*/ring-0.17.14/src/signature.rs`（确认 `ED25519` 常量存在）。
- Step 3: 若根链不存在（预期存在）→ 备选 `ed25519-dalek = "2"`（纯 Rust），并在附录记录差异。

### Task 0.2：声明依赖

**Files**：Modify 根 `Cargo.toml`。

- Step 1（失败验证）：`grep -n 'tokio = ' Cargo.toml` —— features 无 `"process"`（现状即"失败"基线）。
- Step 2：tokio features 追加 `"process"`；`[dependencies]` 增 `ring = "0.17"`。
- Step 3: `cargo check --release` —— 预期通过、无版本统一冲突（lock 不出现第二个 ring 版本）。

**阶段提交**：`chore(deps): ring 直依 + tokio process feature 显式声明`（用户确认后）。

---

## 阶段 1：PR-1 crypto（任务 #2）

**改动面**：`src/bridge/crypto.rs`、`src/bridge/bootstrap.js`、`src/bridge/mod.rs`（op 注册）、`sample/global.d.ts`。

### Task 1.1：verifyEd25519 测试先行（BDD 场景 1–8）

**Files**：Test `src/bridge/crypto.rs` `mod tests`。

- Step 1（写失败测试）：仿 `aes_gcm_roundtrip_and_bad_key` 造 `verify_ed25519_bdd_scenarios`，一个测试覆盖 8 场景（RFC 8032 Test Vector 1 的 pk/sig 常量内联）：

```rust
// Scenario 1: Given RFC8032 向量 When verifyEd25519(pk,msg,sig) Then true
// Scenario 2/3: 篡改 msg 一字符 / 换公钥 → false
// Scenario 4/5/6: sig="!!!"、公钥31/33字节、sig解码后63字节 → 各自抛错点名
// Scenario 7: 偶长全hexdigit串走hex分支（钉死 decode_key 次序）
// Scenario 8: base64url 公钥 → 抛错点名（限制表来源）
```

- Step 2：`cargo test --release -p only-js verify_ed25519` —— 预期 FAIL（函数不存在 → 编译失败即为红）。
- Step 3（最小实现）：

```rust
/// crypto.verifyEd25519(pk, msg, sigBase64)：格式错抛错、验签不过 false（双层语义）。
#[op2]
pub fn op_ed25519_verify(
    #[string] public_key: String,
    #[string] msg: String,
    #[string] sig_base64: String,
) -> Result<bool, JsErrorBox> {
    let pk = decode_key(&public_key)?; // DRY：复用既有 hex/base64 解码
    if pk.len() != 32 {
        return Err(JsErrorBox::generic(format!(
            "verifyEd25519: public key must be 32 raw bytes (got {})", pk.len())));
    }
    let sig = STANDARD.decode(sig_base64.trim())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(sig_base64.trim()))
        .map_err(|e| JsErrorBox::generic(format!("verifyEd25519: signature not base64: {e}")))?;
    if sig.len() != 64 {
        return Err(JsErrorBox::generic(format!(
            "verifyEd25519: signature must be 64 bytes (got {})", sig.len())));
    }
    let v = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &pk);
    Ok(v.verify(msg.as_bytes(), &sig).is_ok())
}
```

- Step 4：同测试文件补 `mod.rs` 注册项后 `cargo test --release -p only-js verify_ed25519` —— 预期 PASS。

### Task 1.2：sha256Hex 扩二进制（场景 9）

- Step 1（失败测试）：`Given Uint8Array([1,2,3]) / ArrayBuffer / 非UTF8字节` → `sha256Hex` 结果与等价 hex 向量一致。
- Step 2：实现 `op_sha256_bytes(#[buffer] data: JsBuffer) -> String`（先例 `src/bridge/blob.rs:546`）。
- Step 3：`bootstrap.js` 分流（**保 7-bit ASCII**）：

```js
crypto.sha256Hex = (data) => {
  if (typeof data === "string") return op_sha256_hex(data);
  if (data instanceof ArrayBuffer) data = new Uint8Array(data); // JsBuffer 只收 View
  return op_sha256_bytes(data);
};
```

- Step 4：测试 PASS + `cargo fmt` + clippy 门禁。

**阶段提交**（用户确认后）：`feat(crypto): Ed25519 验签原语 + sha256Hex 二进制扩面`。
文档：global.d.ts JSDoc；api-manual 错误/限制表三条（§2.4）可并入阶段 7 批量落。

---

## 阶段 2：地基 platform 段（任务 #3）

**改动面**：`src/config.rs`、`src/bridge/platform.rs`（新）、`src/bridge/mod.rs`、
`oj/src/serve_cmd.rs`（装配）、`src/bridge/fs.rs`（trusted_paths 并入）。

### Task 2.1：PlatformCfg 解析 + 对账单测

**Files**：Modify `src/config.rs`。

- Step 1（失败测试）：`known_top_level_keys_covers_all_typed_fields` 增 `platform` 期望 → FAIL。
- Step 2：实现（deny_unknown_fields 对齐既有段风格）：

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformCfg {
    #[serde(default)] pub modules: Vec<String>,
    #[serde(default)] pub trusted_paths: Vec<String>,
    #[serde(default)] pub exec: BTreeMap<String, ExecAllowCfg>, // 模块 × 授权
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecAllowCfg {
    pub allow: Vec<String>,           // argv 模板
    #[serde(default)] pub env: Vec<String>,
}
```

Config 增 `pub platform: Option<PlatformCfg>`；对账表加 `"platform"` → PASS。

### Task 2.2：grants + assert_platform（DRY：路径校验单点）

**Files**：Create `src/bridge/platform.rs`；Modify `src/bridge/mod.rs`。

- Step 1（失败测试，BDD）：
  - S1 Given 未配 platform When assert Then `NotCapable: platform not configured`；
  - S2 Given modules 含 "a" When 模块 a Then Ok / 模块 b Then `NotCapable: module 'b' is not platform-privileged`；
  - S3 Given trusted_paths=["./config.yaml"] When path 占位符校验 "config.yaml"/"../config.yaml"/绝对越界 Then Ok/拒/拒。
- Step 2：实现 grants（装配期 canonicalize，缺省 deny）+ `resolve_allowed(root, p)`（复用 `fs.rs` 的 `join_jailed`/`resolve_existing`，校验落在 `fs.root ∪ trusted_paths`）+ `assert_platform(state)`。
- Step 3：StableState 增 `platform: Arc<PlatformGrant>`（构造期注入，同 FsGrant 惯例）→ PASS。

### Task 2.3：装配 + 抢注 fail-fast

**Files**：Modify `oj/src/serve_cmd.rs`。

- Step 1（失败测试，BDD S4/S5）：Given 特权名单含 "ojm-installer" 且该模块目录位于安装树（mounts 解析产物）内 → 装配 Err 点名；位于受控目录 → Ok。
- Step 2：装配期构建 grants；抢注校验（特权名 ∩ 安装树模块名 → fail-fast）。
- Step 3：fs 轴特权扩展——`StableState.fs` 裁决对特权模块追加 trusted_paths（在 `op_fs_resolve` 前置裁决点实现，改 `fs.rs` 判定入口）→ PASS。

**阶段 2 实施偏差记录**（评审后修订）：
1. **装配位置**：grants 构建/抢注防护落在 `oj/src/app.rs assemble_backend`（`build_platform_grants`），非计划原文的 `serve_cmd.rs`——StableState 唯一构造点在该处，注入路径最短。
2. **checkout_reset 换装**：deno fs op 有第二道防线（PermissionsContainer，per-runtime），trusted 可及性必须按请求换装容器；`Bridge::checkout_reset` 是全部借出路径（含 inspector 挂载前借出）的唯一重置点，`inspector()` 因此从裸 checkout 改走 checkout_reset（防特权容器经调试器求值触达）。
3. **platform.root 受控挂载**（设计稿 §1.2 修订）：抢注检查豁免受控根 discover 产物，`modules` ⊆ 受控根产物（悬空 fail-fast）；受控根与任何其它 api 挂载树同径或互为父子（前缀重叠）→ fail-fast（防抢注绊线被静默穿透）；不引入隐式 mount——受控根以既有 mounts 条目接入（部署方把 `platform.root` 目录写进 `mounts[].api`），校验挂在「挂载树 discover 产物 + 受控根 discover 产物」两份集合上，零新装配面。

**阶段提交**：`feat(platform): platform 段（modules/trusted_paths/exec 授权）+ 特权裁决 + 抢注防护`。

---

## 阶段 3：exec 全局（任务 #4）

**改动面**：`src/bridge/exec.rs`（新）、`src/bridge/bootstrap.js`、`mod.rs`。

### Task 3.1：模板解析与匹配（纯函数，SRP 独立可测）

**Files**：Create `src/bridge/exec.rs`（先写模板子模块 + 单测，无 IO）。

- Step 1（失败测试）：
  - `parse_template("tar -xzf ${path:archive} -C ${path:dest}")` → tokens 正确；
  - 匹配：literal 逐字等、占位符吸收恰一 token；多 token 被 `${path:}` 吞两段 → 失配；
  - `${raw:module}` 收 `-x` / 以 `/` 起手 → 拒（option/绝对形态注入）；
  - 双模板同时命中 → 歧义错。
- Step 2：实现 `Token{Lit,Path,Raw}` + `parse_template` + `match_template`（返回绑定表）→ PASS。

### Task 3.2：exec.run op（spawn + 治理）

- Step 1（失败测试，BDD S1–S12，`bridge` 集成形态）：
  - S1 合法模板 + jail 内路径 → 解压成功 `code=0`；
  - S2 literal 被替换（argv 夹 `--use-compress-program`）→ NotCapable 失配；
  - S3 path 占位符绝对/越界 → NotCapable；
  - S4 env 白名单外变量不可见（子进程 `printenv`）/ 白名单内可见；
  - S5 输出 >10MB → 抛错点名 + 进程被杀；
  - S6 timeoutMs 到期 → 抛错点名（unix 组杀验证无残留子进程）；
  - S7 非 0 退出 → **返回** `{code:非0}` 不抛；信号终止 → code=128+n；
  - S8 非特权模块 → NotCapable；S9 `"oj"` 模板 → current_exe 解析；
  - S10 歧义双模板 → 报错；S11 未配 platform.exec → NotCapable；
  - S12 stderr 强制 UTF-8，非法序列抛错。
- Step 2：实现（设计稿 §1.3 全条款）：`tokio::process::Command`；stdout/stderr **流式计数**（禁 `wait_with_output`）；超时 `tokio::time::timeout` + `libc::kill(-pgid, SIGKILL)` 兜底（`#[cfg(unix)]` + `process_group(0)`）；命令解析器 `resolve_cmd(name)` 做成 `ExecGrant` 字段（DIP：`"oj"`→`current_exe()`，其余父进程 PATH 解析，测试可注入）。
- Step 3：bootstrap.js 挂 `globalThis.exec`（ASCII 红线）→ 全 PASS。

**阶段提交**：`feat(exec): exec.run（argv 模板 + 路径治理 + 流式上限 + 组杀）`。

---

## 阶段 4：PR-5 yaml（任务 #5）

**改动面**：`src/bridge/yaml.rs`（新）、`bootstrap.js`、`mod.rs`。

### Task 4.1：三预算解析器（先测试后实现）

- Step 1（失败测试，BDD）：
  - S1 合法 YAML（嵌套/list/map/锚点）→ 结构正确；
  - S2 非法 YAML → 抛错带行列（serde_yaml location）；
  - S3 **alias 链式放大样本**（`a1:[*a0,*a0] × N` 压缩进 1MB 内）→ 节点/alias 预算超限点名，不 OOM；
  - S4 20 万 `[` 深嵌套 → 深度超限报错，**进程不崩**（SIGSEGV 即失败）；
  - S5 >1MB → 输入超限；
  - S6 大整数 → `jsnum::sanitize_js_numbers` 护栏后 `json.ok` 不 500；
  - S7 `yaml.stringify(manifest 形对象)` → parse 回读语义相等；
  - S8 大样本（预算内）parse 期间并发 handler 不饿死（deadline 测试，证 spawn_blocking 生效）。
- Step 2：实现自写递归 `Deserialize`（`BoundedValue`，每层 visit 计数 nodes/aliases/depth，超限即 `de::Error::custom` 点名）；解析整体 `tokio::task::spawn_blocking` + deadline；出口护栏挂 `jsnum`；stringify 直接 serde_yaml（文档限定机器产物）。
- Step 3：重复键行为先跑实际用例，把观测结果写进 api-manual 草稿注释 → 全 PASS。

**阶段提交**：`feat(yaml): yaml.parse（三预算防炸弹）+ yaml.stringify（机器产物）`。

---

## 阶段 5：PR-3 reloadConfig（任务 #6）

**改动面**：`src/bridge/`（outcome 下沉 + Extras 槽 + op）、`serve/src/reload.rs`、`oj/src/serve_cmd.rs`、`oj/src/app.rs`（from_config 穿参）。

### Task 5.1：类型下沉 + 槽位

- Step 1：根 crate 新 `src/bridge/reload_bridge.rs`：`ReloadOutcome{generation,unchanged,applied,restart_pending,kept_listener}` + `pub type ReloadExecutorLike = Arc<dyn Fn() -> Result<ReloadOutcome,(u16,String)> + Send + Sync>`；Extras 增 `pub reload: Arc<OnceLock<ReloadExecutorLike>>`。
- Step 2：serve 侧 `ReloadOutcome` 改 re-export/`From`（DRY 单一真源）；`App::from_config` 穿参槽指针；每代新 App 复带槽（防热更一次后失效）。
- Step 3：单测：槽空 → op 报 `reload not available`；槽有 → 透传 outcome。

### Task 5.2：共享处理体 + diff 扩面（自提权封堵）

- Step 1（失败测试，BDD）：
  - S1 改盘上 auth/vars 段 → 特权触发 `applied` 含段名、generation+1；
  - S2 unchanged → `unchanged=true`、generation 不动；
  - S3 **改 platform 段** → `restart_pending` 含 `platform`、**不热生效**；
  - S4 改 fs 段 → `restart_pending`；
  - S5 非特权 → NotCapable；S6 并发双触发 → 一方 busy 明确报错；
  - S7 `oj test` → not available；S8 既有 `POST _system/config` e2e 全绿（行为不变）。
- Step 2：`diff_configs` 的 restart 臂增 `platform`/`fs`；HTTP POST 处理体抽 `serve` 层共享函数（try_lock→executor→outcome 映射），JS op 经 `spawn_blocking` 调同一 executor（**共享 reload_lock**）。
- Step 3：bootstrap.js 挂 `globalThis.server.reloadConfig` → 全 PASS。

**阶段提交**：`feat(reload): server.reloadConfig 进程内热更 + platform/fs 段 restart 隔离`。

---

## 阶段 6：PR-4 migrate CLI（任务 #7）

**改动面**：`oj/src/args.rs`、`oj/src/migrate_cmd.rs`、`oj/src/migrate.rs`。

### Task 6.1：--format json + continue-collect + 定向目录

- Step 1（失败测试）：
  - S1 双模块空库 `--format json` → 两模块 applied 列表齐全、退出码 0、stdout 纯 JSON（tracing 已在 stderr）；
  - S2 模块 B 迁移损坏 → A applied、B error 带 seq+原因、退出码非 0、json 完整；
  - S3 重跑 → applied 空（幂等）、退出码 0；
  - S4 **脱敏**：构造含 DSN 的连接失败 → json error 不含凭据（击穿用例）；
  - S5 `<module>-<version>` 定向 → 仅该模块（按锁解析版本目录，新增匹配逻辑）。
- Step 2：`run_migrate` 改 continue-and-collect + 聚合退出码；`apply_module` 返回 `Vec<AppliedMigration>`；错误格式化统一过 `sanitize_dsn()`；args 增 `--format json` + 版本目录 positional。
- Step 3：全 PASS。

### Task 6.2：exec 模板全链路 e2e

- Step 1（e2e，BDD S6–S8）：安装器形态特权模块经 `exec.run(["oj","migrate","-c","${path:config}","--format","json","-d","${path:root}","${raw:module}"])` 走通 sqlite；白名单未含该模板 → NotCapable；argv 注入 `--baseline` → 模板失配拒绝。
- Step 2：mysql/pg 用例按 `OJ_TEST_MYSQL`/`OJ_TEST_PG` env-gated + 静默 skip 惯例。

**阶段提交**：`feat(migrate): --format json + 定向版本目录 + 错误脱敏（安装器经 exec 触发）`。

---

## 阶段 7：文档 / 样例 / 版本（任务 #8）

1. **devkit 四件**逐条对齐：api-manual（总表 27 组 → 新增 `exec`/`server`/`yaml` 组；错误/限制表：NotCapable 文案、模板失配、输出超限、YAML 预算、unchanged 形状、tasks/WS 无特权、sig 仅 base64、公钥非 PEM、PEM 不支持等，**另登记**：`sha256Hex` 非 string/二进制入参现抛 TypeError（旧版 `String(s)` 强转，fail-loud 行为变更））、SKILL.md 陷阱速查（config.yaml 在 jail 外须 trusted_paths、大 DDL 须显式 timeoutMs、base64url 公钥不支持）、scenarios.md（安装闭环场景：下载→校验→解压→迁移→回写→热更）、README.md。
2. `sample/global.d.ts`：`exec`/`server`/`yaml` interface + `crypto.verifyEd25519`/`sha256Hex` 重载。
3. `docs/dev-guide.md`（platform 段走读）、`docs/ops-manual.md`（tar 依赖与方言、`OJ_SECRET_KEY` 透传、无 tar 环境 exec 报错）、`docs/bridge.md`（op↔global 对应面）。
4. `sample/src/` 增 `platform_demo` 模块 + `sample/config.yaml` 补 `platform:` 段示例。
5. 版本 bump + CHANGELOG（逐条用户可见变更）；`cargo xtask build` 归置 + `cargo test --release -p xtask` devkit 契约用例绿。

**阶段提交**：`docs(devkit): exec/server/yaml/verifyEd25519 对外文档与样例（v0.1.x）`。

---

## 阶段 8：最终集中审查（任务 #9）

**协议**（用户点名「最终集中审查」）：

1. **全门禁**：`cargo fmt --check`、`cargo clippy --release --workspace --all-targets -- -D warnings`、`cargo test --release --workspace`、`cargo xtask build` + `cargo xtask smoke --bin bin/oj`。
2. **设计对账**：code-reviewer 对全量 diff 逐条核对设计稿 v2（重点 §9 处置表 24 项逐一「已落实」）+ 红线清单：bootstrap.js 7-bit ASCII、panic=unwind、失败 runtime 丢弃、SQL 构造器红线、ENC[...] 不进仓库、`known_top_level_keys` 对账。
3. **偏差处置**：设计↔实现不符 → 只订正文档 + 登记待做项（禁夹带实现变更）；发现真缺陷 → 修复 + 补对抗性回归。
4. 产出审查报告（逐项通过/偏差/待办）→ **用户确认后**阶段级提交与推送（显式 origin）。

---

## 附录：spike 结论

（阶段 0 执行后回填：ring 链证据、Ed25519 API 确认、serde_yaml deprecated 评估、tar 方言探测。）

### Task 0.1 + 0.2 实证结论（2026-10-16，worktree feat/ojm-native-exec）

**ring 链证据**（`cargo tree -i ring@0.17`，Exit 0）：

- 根链（预期存在，已证实）：`ring 0.17.14 ← jsonwebtoken 9.3.1 ← only-js`（根 crate 直接链）。
- serve 侧链：`ring 0.17.14 ← serve`（`serve/Cargo.toml:25` 直依 `ring = "0.17"`）。
- `Cargo.lock` 单版本：仅一条 `name = "ring"`，`version = "0.17.14"`（Cargo.lock:5577）。非 rustls 途经，与评审结论一致。

**Ed25519 API 确认**（ring 0.17.14 源码，`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ring-0.17.14/`）：

- `src/ec/curve25519/ed25519/verification.rs:34` 定义 `pub static ED25519: EdDSAParameters`，实现 `signature::VerificationAlgorithm`（`UnparsedPublicKey::new(&signature::ED25519, pk)` 用法见 `src/signature.rs:158` 文档示例）。
- 经 `src/signature.rs:261-263` re-export 为 `ring::signature::ED25519`；该模块无任何 `cfg(feature)` 门控，ring 0.17 的 feature 仅影响汇编/后端选择，Ed25519 恒可用。签名侧 `signature::Ed25519KeyPair`（signing.rs）同在。
- 结论：无需备选 `ed25519-dalek`，Step 3 不触发。

**tokio process 现状**：

- 基线（修改前）：根 `Cargo.toml:76` `[dependencies]` tokio features 为 `["macros", "rt", "sync", "net", "io-util", "time", "fs"]`，无 `"process"`——process 此前仅靠 deno/reqwest 上游 feature 统一隐式启用，证实「必须显式声明」的风险真实存在。
- 已修：features 追加 `"process"`；`[dev-dependencies]` tokio（`Cargo.toml:111`）保持不动（最小改动）。

**依赖声明**：根 `[dependencies]` 增 `ring = "0.17"`（紧邻 jsonwebtoken，含注释）；`cargo check --release` 通过，`Cargo.lock` 自动同步后仍单版本 0.17.14，无版本统一冲突。

**serde_yaml / tar 方言探测**：待后续阶段任务执行后回填。
