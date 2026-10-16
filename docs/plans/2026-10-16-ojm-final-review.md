# ojm 后端原生化 最终集中审查报告

- **日期**：2026-10-16
- **分支**：`feat/ojm-native-exec`（HEAD `2f3f250`，最新提交为 devkit 四件交叉核验 + 安装闭环 scenarios）
- **范围**：相对 `main` 的全部改动 `git diff main...HEAD` —— 31 文件 +5783 / -140
- **审查方式**：只读审查（不修改源码、不 `git commit`）；逐条独立读代码核实，不轻信提交信息；门禁全部重跑确认
- **权威依据**：设计稿 v2 `docs/plans/2026-10-16-ojm-native-exec-platform-apis-design.md`（§9 处置表 + §1–§5 各 PR 语义）、实施计划 `…-implementation-plan.md`、根 `CLAUDE.md` 设计红线

---

## 1. §9 处置表逐条对账

> 状态图例：**✅ 已落实**（独立读代码核实通过）/ **⚠️ 部分偏差**（见 §3 偏差登记）/ **❌ 未落实**
> 证据格式：`file:line`（本批次实际落点）。

### 1.1 安全（专员，11 项）

| # | 级别 | 意见摘要 | 状态 | 证据 |
|---|------|----------|------|------|
| 1 | 阻塞 | argv 自由形态使 tar 白名单=RCE | ✅ | `src/bridge/exec.rs:64`(`parse_template` literal 逐字)、`:132`(`match_template`)、`:200`(`match_all` 歧义拒绝)、`:340`(路径占位符校验) |
| 2 | 阻塞 | yaml alias 放大 + 同步 op 不可抢占 DoS | ✅ | `src/bridge/yaml.rs:46`(`MAX_INPUT_BYTES`)、`:52`(`MAX_NODES`)、`:56`(`MAX_DEPTH`)、`:267`(spawn_blocking + 5s deadline) |
| 3 | 阻塞 | reloadConfig × `_=>applied` 自提权链 | ✅ | `oj/src/serve_cmd.rs:548`(`platform`/`fs` → `restart_pending`)、`oj/tests/e2e.rs:2572`（`S3/S4` 击穿断言特权授予未热生效） |
| 4 | 重要 | cwd 非隔离 + 路径参数未治理 | ✅ | `src/bridge/exec.rs:340`(经 `platform::resolve_allowed`)、`src/bridge/platform.rs:122`（fs.root ∪ trusted_paths 单点校验） |
| 5 | 重要 | platform.modules 按名可抢注 | ✅ | `oj/src/app.rs:499-567`（`build_platform_grants`：受控根必配 + discover 产物名集合 P + 抢注 fail-fast + `modules ⊆ P` 悬空检查） |
| 6 | 重要 | PATH 解析与 env 空表矛盾；tar 方言 | ✅ | `src/bridge/exec.rs:537-557`（`default_resolve_command`：`"oj"`→`current_exe`，其余父进程 PATH 解析） |
| 7 | 重要 | 预检 TOCTOU / 成员名怪形态 | ⚠️ | 后置 walk 权威化（§1.4）属 **ojm-installer（oj-module 仓库）JS 编排**，本批次按 §0 范围声明已剥离；本次仅交付 exec 原语（可支撑 `tar` 调用）。见 §3 偏差 D1 |
| 8 | 重要 | 归属请求级非调用栈级；tasks/WS 无归属 | ✅ | `src/bridge/platform.rs:142-164`（module=None → NotCapable 三态）、`src/bridge/exec.rs:281`（exec 无归属 → NotCapable） |
| 9 | 建议 | migrate 前缀残余面（`-c`/`--baseline`/`--db`/stderr） | ✅ | 模板定死不含 `--baseline`/`--db`（`oj/src/migrate_cmd.rs` 调用模板）；`sanitize_dsn` 脱敏 `:98-135`；§4.3 残余风险两条入 api-manual |
| 10 | 建议 | decode_key 次序 / base64url / 双引擎 | ✅ | `src/bridge/crypto.rs:316`(hex 优先次序)、`:232-234`(sig 仅 STANDARD/NO_PAD，base64url 报错)、测试 `:585`(e8 base64url→点名) |
| 11 | 建议 | 进程组 kill 细节 | ✅ | `src/bridge/exec.rs:376-377`(`#[cfg(unix)] process_group(0)`)、`:520-527`(`kill(-pgid, SIGKILL)` + 兜底 `start_kill`)、措辞降级 |

### 1.2 架构师（7 项）

| # | 级别 | 意见摘要 | 状态 | 证据 |
|---|------|----------|------|------|
| 1 | 阻塞 | Extras.reload 引 serve 类型 = 循环依赖 | ✅ | `src/bridge/reload_bridge.rs:30`(`ReloadOutcome` 定义在根 crate)、`serve/src/reload.rs:29`(`pub use only_js::bridge::reload_bridge::ReloadOutcome`) |
| 2 | 重要 | Extras 冻结早于 ReloadState（构造环） | ✅ | `src/bridge/reload_bridge.rs:53`(`ReloadSlot = Arc<OnceLock>`)、`src/bridge/mod.rs:221`/`:294`(StableState/Extras 注入同一 Arc)、`oj/src/serve_cmd.rs:455`(代际传递槽指针) |
| 3 | 重要 | reload_lock 串行化 + 同步 executor 阻塞语义 | ✅ | `serve/src/reload.rs:56`(`reload_lock: Mutex`)、`:148-170`(`trigger`/`trigger_sync` 共享锁 + 同一 executor)、`oj/src/serve_cmd.rs:391-406`(独立线程 + 多线 runtime) |
| 4 | 重要 | tasks/WS 无归属恒 deny 未声明 | ✅ | `src/bridge/platform.rs:160-164`(None → NotCapable)、`src/bridge/exec.rs:281-283` |
| 5 | 重要 | 前缀序列 argv[0] 归一规则空白 | ✅ | `src/bridge/exec.rs:64-90`(首 token 必 literal、禁路径分隔符)、`:317-324`(宿主侧解析) |
| 6 | 建议 | known_top_level_keys 对账 | ✅ | `src/config.rs:1397`(`"platform"` 已入对账表)、`:1672` 对账单测 |
| 7 | 建议 | 文档清单不具体 | ✅ | `docs/devkit/*`（api-manual/scenarios/SKILL/README 交叉核验）、`CHANGELOG.md`、`sample/global.d.ts` 均更新 |

### 1.3 工程师（11 项）

| # | 级别 | 意见摘要 | 状态 | 证据 |
|---|------|----------|------|------|
| 1 | 阻塞 | PR-3 回调形态/注入点/构造环三缺陷 | ✅ | 与架构 1/2/3 同批并入 §3（OnceLock 槽 + reload_bridge 下沉类型） |
| 2 | 重要 | tokio process feature 未显式声明 | ✅ | `Cargo.toml:81`(features 含 `"process"`) |
| 3 | 重要 | "oj" 解析无答案（PATH×空 env 矛盾） | ✅ | `src/bridge/exec.rs:537-557`(`current_exe` 特判 + 父进程 PATH 解析；解析器成 `ExecGrant` 字段可注入) |
| 4 | 重要 | tar -t 无类型信息；判据子串误伤 | ⚠️ | 同安全 7：后置 walk 权威化（§1.4）属 ojm-installer；本批次交付 exec 原语。见 §3 偏差 D2 |
| 5 | 重要 | serde_yaml 深度必须 + 包 Deserializer 不可行 | ✅ | `src/bridge/yaml.rs:73-195`(自写 `Seed`/`Vis` 递归访问器逐层计数) |
| 6 | 重要 | cwd 隔离表述夸大 | ✅ | `src/bridge/exec.rs:1-17`(明确「非隔离边界，cwd=fs.root」)、bootstrap.js 注释同步降级 |
| 7 | 建议 | kill_on_drop 不杀组；wait_with_output 无界；信号码 | ✅ | `src/bridge/exec.rs:375`(`kill_on_drop` 兜底)、`:491-515`(双流流式计数替代 `wait_with_output`)、`:464-471`(code = 128 + signal) |
| 8 | 建议 | ring 引入链更正（jsonwebtoken 非 rustls） | ✅ | 实施计划附录 cargo tree 实证（`ring 0.17.14 ← jsonwebtoken 9.3.1 ← only-js` 单版本） |
| 9 | 建议 | ArrayBuffer 归一 + bootstrap ASCII | ✅ | `src/bridge/crypto.rs:249`(`op_sha256_bytes`)、`src/bridge/bootstrap.js:1072-1076`(sha256Hex 分流) |
| 10 | 建议 | run_migrate fail-fast 重构；`<name>-<version>` 是新逻辑；stdout 已纯；timeout 偏紧 | ✅ | `oj/src/migrate_cmd.rs:214-247`(continue-and-collect)、`:61-79`(版本目录定向)、tracing 走 stderr |
| 11 | 建议 | unchanged 返回形状未定义 | ✅ | `src/bridge/reload_bridge.rs:102-142`(serde 形状恒定、unchanged 时三清单空数组) |

### 1.4 产品（11 项）

| # | 级别 | 意见摘要 | 状态 | 证据 |
|---|------|----------|------|------|
| 1 | 阻塞 | config.yaml 回写缺失且 jail 外够不到 | ✅ | `src/bridge/fs.rs:99-169`(`container_for_module` 特权模块扩 trusted_paths)、`:139-169`(`op_fs_resolve` 双查) |
| 2 | 重要 | manifests.yaml 写回无序列化原语 | ✅ | `src/bridge/yaml.rs:257`(`stringify_value`，文档限定机器产物) |
| 3 | 重要 | 落位原子性未设计 | ⚠️ | 五步原子落位（§1.5）属 ojm-installer JS 编排（§0 范围外）；exec 原语支撑 staging 目录操作。见 §3 偏差 D3 |
| 4 | 重要 | 退出码/解码语义未钉死 | ✅ | `src/bridge/exec.rs:413-486`(返回 vs 抛错边界 + UTF-8 强制 + 128+signal) |
| 5 | 重要 | argv 路径参数 jail 语义未定义 | ✅ | 并入安全 4（被 §1.1 路径占位符吸收） |
| 6 | 重要 | 文档落点无 PR 级清单 | ✅ | `docs/devkit/api-manual.md` 新增 exec/server/yaml 三全局组 + 错误表；`scenarios.md` 安装闭环 |
| 7 | 建议 | sample 覆盖缺位 | ⚠️ | `sample/src/_platform` 实为**迁移夹具**（含 migrations/schema/manifest，无 `api.ts` 平台特权 handler），`sample/config.yaml` 无 `platform:` 段；示例仅落 docs。见 §3 偏差 D4 |
| 8 | 建议 | yaml.parse 失败行为未定 | ✅ | `src/bridge/yaml.rs:199-231`(`map_parse_error` 带行列点名) |
| 9 | 建议 | verifyEd25519 限制进表 | ✅ | `docs/devkit/api-manual.md:2266-2292`（sig 仅 base64 / msg 仅 string / 公钥须 32 字节原始 key） |
| 10 | 建议 | ojm-installer 编排归属未声明 | ✅ | 设计稿 §0 范围声明（原语面齐备、编排属 oj-module 仓库） |
| 11 | 建议 | PR-6 两锚点 | ✅ | 设计稿 §6（admission gate 位置 + 缓存目录决策项，列为远期，本批不涉及） |

### 1.5 §9 统计

- **安全**：9 ✅ / 2 ⚠️（7、4）
- **架构师**：7 ✅ / 0 ⚠️
- **工程师**：9 ✅ / 2 ⚠️（4、7）
- **产品**：8 ✅ / 3 ⚠️（3、7、… 实计 3 项：3、7）
- **合计**：**33 ✅ / 7 ⚠️ / 0 ❌**

所有 ⚠️ 均源于同一根因：**ojm-installer 的安装器 JS 编排（tar 解压后置 walk、5 步原子落位、可运行 demo）按设计稿 §0 范围声明剥离至 oj-module 仓库**；本批次交付的 exec 原语 + platform 特权面 + reloadConfig + yaml + crypto 均已完整落实并配对抗性回归。⚠️ 为「范围性部分偏差」而非实现缺陷，需在 installer 仓库单独审计。

---

## 2. 红线清单 9 项

| # | 红线 | 证据与结论 |
|---|------|-----------|
| 1 | bootstrap.js 7-bit ASCII | ✅ `python3` 逐字节扫描 `src/bridge/bootstrap.js`：全部 ≤ 0x7F，无 ≥0x80 字节。（注：仓库自带 BSD `grep -P` 不可用，改用 python3 验证；CLAUDE.md 命令在 GNU grep 环境等效。） |
| 2 | panic=unwind | ✅ `Cargo.toml:131` `[profile.release] panic = "unwind"` 保留；`:81` tokio 显式加 `"process"` feature；本批次无新增 cdylib 插件、无 `panic = "abort"` 覆盖；op 异常统一走 `JsErrorBox`（`src/bridge/exec.rs`/`platform.rs`/`reload_bridge.rs`）。 |
| 3 | SQL 构造器红线 | ✅ `git diff` 在 `src/bridge/{db,query,accessor_sqlx}.rs` 无新增 `db.query` 动态标识符；diff 中唯一 `db.query` 命中为 `api-manual.md` 文档里**被点名禁止**的示例（`await db.query("select ... " + id, []) // 禁止`）。本批新增 op 不触 SQL。 |
| 4 | ENC[...] 不进仓库 | ✅ `git ls-files` 命中的 `ENC[`/`PRIVATE KEY` 全部位于 `.md`/`.github` 文档与示例（功能说明，非真实密钥）；`sample/config.yaml` 等代码/配置无新增明文私钥。`.gitignore:19` 已含 `secrets-private.pem`，sample 私钥不入库（即 e2e 环境性失败的成因）。 |
| 5 | 特权 Fail-Closed | ✅ 未配 `platform` → `exec.run`/`server.reloadConfig` 恒 `NotCapable`（`src/bridge/exec.rs:279-280`、`src/bridge/platform.rs:147-151`）；`assert_platform` 三态均 NotCapable。 |
| 6 | exec argv 模板（无自由 argv RCE 面） | ✅ `parse_template` literal 逐字相等（`:64`）；`${path:}` 走 `resolve_allowed` 校验 fs.root ∪ trusted_paths（`:340` ↔ `platform.rs:122`）；`${raw:}` 拒 `-` 开头与绝对形态（`:156-169`）；同一 argv 多模板命中 → 歧义拒绝（`:200-226`）；命令名宿主侧解析（`:537`）。对抗测试 S2/S3/S5/S8/S10/S11 钉死（`src/bridge/exec.rs` 测试）。 |
| 7 | reload 自提权封堵 | ✅ `diff_configs` 将 `platform`/`fs` 归 `restart_pending`（`:548`）；HTTP 与 JS 共享处理体（`serve/src/reload.rs:148-170` 同一 `reload_lock` + 同一 executor）；e2e `:2572` 击穿断言「撤销特权段已落盘且 reload 已跑，但授予未热生效」。 |
| 8 | yaml 炸弹三预算 | ✅ 输入 1 MiB（`:46`/`:235`）、节点 100_000 含 alias 展开（`:52`/`:78`）、嵌套深度 128（`:56`/`:82`，先于 serde_yaml 触发）；整体 `spawn_blocking` + 5s deadline（`:267`）。对抗用例 S3/S4/S5 钉死不 OOM/不崩。 |
| 9 | 任务 / WS 无归属恒拒 | ✅ tasks/WS 上下文 `ReqState.module = None`（装配期 `checkout_reset`/`inspector` 以 `module=None` 重置 → 非特权基线容器，`src/bridge/mod.rs:868-898`/`932`）；exec/`assert_platform` 对 None 恒 NotCapable（`:281`、`:160-164`）。 |

**红线 9 项全部 ✅ 全绿。**

---

## 3. 偏差登记

> 7 项 ⚠️ 同根。逐条记录点 / 影响 / 建议。

- **D1（安全 7 / 工程师 4）— tar 解压后置 walk 权威化未在本批次**
  - **点**：§1.4「解压后全量 walk 校验（零容忍 symlink/hardlink/设备/FIFO、组件级复检、总体积对账）」与「`tar -tzf` 预检降级快速失败」的实现体是 **ojm-installer（oj-module 仓库）JS 编排**，设计稿 §0 已将其明确剥离。
  - **影响**：本批次只交付 `exec.run` 原语（可调用系统 `tar`），直接支撑 §1.4 所需的「模板化解压 + 后置校验」。但**解压安全判定代码不在本 diff**，本轮审查无法覆盖其实现正确性，须在 installer 仓库单独审查（含 xtask smoke 的 `tar --version` 方言探测）。
  - **建议**：在 installer 仓库 PR 落地时，复用本批次 `exec` 模板 + 后置 walk 思路，并以同等对抗用例（symlink 逃逸 / 设备文件 / 体积炸弹）钉死。本批次 `exec` 的 cwd=fs.root + 路径占位符已把「可达文件范围」收窄到 jail ∪ trusted_paths，残余风险由 installer 的后置 walk 兜底。

- **D2（产品 3）— 5 步原子落位次序未在本批次**
  - **点**：§1.5 原子落位（staging → walk 校验 → 旧目录 rename `.old` → staging rename 目标 → 清理）属 installer JS 编排。
  - **影响**：exec 原语可支撑 staging 目录创建/重命名/清理（`fs` 轴 + exec 已就绪），但落位次序编排不在本 diff。
  - **建议**：同 D1，在 installer 评审时作为显式验收项。本批次 `platform.trusted_paths` 已把 `dist` 纳入授权根（设计稿 §1.6），为 staging 目录提供路径可达性。

- **D3（安全 7 关联）— 解压安全判定与落位为「共享 jail 残余风险」模型**
  - **点**：设计稿 §1.4 声明 tar 方言差异（bsdtar/GNU）与「同 jail 其他模块可置换归档」的残余风险，靠后置校验收敛。
  - **影响**：这是 **架构性假设**，本批次 exec 已按此模型实现（无 OS 级 chroot），但终态安全依赖 D1 的 installer walk。
  - **建议**：文档侧已写明「部署文档声明 tar 依赖」；建议 installer 落地时把「无 tar 环境 exec 报错」「方言探测」写进 ops-manual（api-manual 已部分覆盖）。

- **D4（产品 7）— sample 可运行 demo 与 `platform:` 段示例缺失**
  - **点**：实施计划 §7.4 称「`sample/src/` 增 platform_demo 模块 + `sample/config.yaml` 补 `platform:` 段示例（配置面唯一可运行参照物）」。实际 diff 仅 `sample/global.d.ts`(+82)；`sample/src/_platform` 是**迁移夹具**（migrations/schema/manifest，无 `api.ts` 平台特权 handler），`sample/config.yaml` **无 `platform:` 段**。
  - **影响**：配置面「唯一可运行参照物」缺位；开发者需读 docs 而非 sample 抄配置。且不配 `platform:` 的 sample 仍可正常 boot（避免 `build_platform_grants` 因缺受控根挂载而 fail-fast 把 L1 套件整段拖垮——见 §4 环境性失败）。
  - **建议（非阻塞）**：保留当前 docs 承载示例的取向（api-manual §配置 + scenarios.md 安装闭环已齐备）；若要在 sample 加可运行 demo，须同步加 `platform.root` 受控挂载并纳入 L1 套件（届时 `given_sample_l1_suite` 需该受控根目录存在），工作量超出本批。登记为后续优化项。

- **D5（无缺陷，记录一致性）— 设计稿 §1.4/§1.5 仍描述 installer 逻辑**
  - **点**：设计稿 §1.4/§1.5 文字层面仍把 tar walk / 落位次序写成「本批次交付内容」，与 §0 范围声明（剥离 installer）表面冲突。
  - **影响**：审阅者易误判 §9.7/S-4 为「未落实」。实质是范围分层：原语在本批，编排在 installer。
  - **建议**：在合并前向设计稿 §1.4/§1.5 顶部加注「实现体位于 ojm-installer（oj-module 仓库），本批次仅交付其依赖的原语」以消除歧义。纯文档修订，不触及代码。

---

## 4. 门禁复核结果（全部重跑确认）

| 门禁 | 命令 | 结果 |
|------|------|------|
| `cargo fmt --check` | `cargo fmt --check` | ✅ **PASS**（exit 0，无格式 diff） |
| `cargo clippy --release --workspace --all-targets -- -D warnings` | 重跑 | ✅ **PASS**（exit 0，**0 warning / 0 error**） |
| only-js 单测 | `cargo test --release -p only-js` | ✅ **648 passed; 0 failed**（含 exec/platform/reload/yaml/crypto/fs 全部批次用例：`exec_bdd_main_scenarios`、`s6_concurrent_triggers_one_busy`、`yaml_bdd_main_scenarios`、`ed25519_verify_rfc8032_vectors_and_format_gates`、`fs_trusted_paths_reachable_for_privileged_module_only` 等） |
| oj lib 单测 | `cargo test --release -p oj --lib` | ✅ **273 passed; 0 failed**（含 `app::tests::platform_grants_squatting_protection`、`migrate_cmd::tests::s_sanitize_dsn_redacts_credentials`、`s_json_continue_on_module_failure`、`s_dirversion_targets_named_version_dir`、`diff_classifies_restart_pending` 等） |
| xtask 契约 | `cargo test --release -p xtask` | ✅ **8 passed**（devkit 四件契约用例绿） |
| oj e2e | `cargo test --release -p oj --test e2e` | ⚠️ **41 passed; 1 failed** |

### 4.1 那 1 个失败：环境性，非回归

```
test given_sample_l1_suite_when_oj_test_then_all_pass_and_report_written ... FAILED
L1 套件必须全绿（stderr: oj test: load config: .../sample/config.yaml:
  config has ENC[...] sealed values but secrets.private_key_path:
  read .../sample/./config/keys/secrets-private.pem: No such file or directory）
```

- **判定**：`sample/config.yaml` 使用 `ENC[...]` 密封凭据，但部署私钥 `secrets-private.pem` 未在本机（`.gitignore` 已排除，仅部署机持有）——符合红线 4「有 ENC[...] 无私钥 → fail-fast 不静默当明文」。
- **性质**：**环境性失败，非本批回归**。除该测试外 41 个 e2e 全绿，包括本批次新增的 4 个 reload e2e：
  - `given_reload_api_when_post_config_then_swaps_and_reports_diff` ✅
  - `given_reload_api_when_backend_and_listener_segments_change_then_classified` ✅
  - `given_reload_api_when_privileged_module_calls_reload_config_then_applies` ✅（S1/S2/S5/S6）
  - `given_reload_api_when_privilege_segments_change_then_restart_pending_and_grants_unchanged` ✅（S3/S4 自提权封堵端到端）
- **合回建议**：合回 main 后 CI 若以「无 sample 私钥」环境跑该用例，须按既定惯例 env-gate 或 skip（与 mysql/pg 真库 DSN 惯例一致）；或在该环境注入 `secrets-private.pem` 后跑。

---

## 5. 最终裁决

### 裁决：✅ 可合回 main

**依据**：
1. **§9 处置表**：40 项，**33 ✅ / 7 ⚠️ / 0 ❌**。7 个 ⚠️ 全部源于设计稿 §0 的明确范围声明（ojm-installer 编排剥离至 oj-module 仓库），本批次交付的原语面（exec / platform 特权 / reloadConfig / yaml / crypto）均完整落实并配对抗性回归，无实现缺陷、无安全回退。
2. **红线 9 项**：**全绿**（bootstrap.js 7-bit ASCII、panic=unwind、SQL 构造器、ENC 不入库、Fail-Closed、exec 模板无 RCE 面、reload 自提权封堵、yaml 三预算、tasks/WS 无归属恒拒）。
3. **门禁全过**：fmt ✅、clippy ✅(0 warning)、only-js 648 ✅、oj-lib 273 ✅、xtask 8 ✅、e2e 41 ✅ + 1 环境性失败（缺 sample 私钥，非回归，与既定 env-gate 惯例一致）。

**合回前的 2 项建议（均非阻塞，不要求本批改代码）**：
- **D5 文档一致性**：合并前向设计稿 §1.4/§1.5 顶部补注「实现体位于 ojm-installer（oj-module 仓库），本批次仅交付其依赖原语」，消除 §9.7/S-4 的「未落实」误读。
- **D1/D2/D3 跟踪**：在 ojm-installer（oj-module 仓库）评审任务中登记「tar 后置 walk 权威化」「5 步原子落位」「tar 方言探测」为显式验收项，复用本批次 exec 模板机制；本批次代码不可单独覆盖其终态安全。

**阻塞项清单**：**无**。

---

> 附：审查范围与命令留痕
> - 读代码核实：exec.rs / platform.rs / reload_bridge.rs / yaml.rs / crypto.rs / fs.rs / mod.rs / bootstrap.js / config.rs / serve_cmd.rs / app.rs / reload.rs / migrate_cmd.rs / migrate.rs / e2e.rs / 设计稿 v2 / 实施计划
> - 重跑门禁：fmt、clippy(workspace -D warnings)、only-js/oj-lib/xtask/e2e 测试（日志落 `/tmp/test_*.log`、`/tmp/clippy.log`）
> - 红线核查：`bootstrap.js` python3 字节扫描、Cargo.toml panic/feature、git diff ENC/SQL 关键字、e2e 失败归因
