# `_name_` 目录段 → `{name}` 动态参数 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 文件系统目录段 `_name_` 映射为 URL 动态参数 `{name}`，覆盖 dev 建表、dev 兜底、oj build 三条链路，并同步 CHANGELOG 与 devkit 四件。

**Architecture:** 单 helper（`fs_seg_to_pattern`）+ 三个拼接点（`RouteTable::build`、`rel_pattern`、`Routes::resolve` 回溯下降）+ 构建期 pattern 试插校验。matchit / decode_params / 冲突裁决全部复用。设计文档：`docs/superpowers/specs/2026-09-25-underscore-route-params-design.md`。

**Tech Stack:** Rust（matchit 0.8、axum）、现有测试设施（`cargo test --release`）。

## Global Constraints

- **禁止 debug 构建/测试**：一律 `cargo build --release` / `cargo test --release` / `cargo clippy --release --all-targets -- -D warnings`。
- `cargo fmt` 门禁：每个任务提交前跑 `cargo fmt`。
- 提交信息末尾加属性行：`unix@vip.qq.com ai`。
- `bootstrap.js` 7-bit ASCII 红线：本计划不触碰 JS 运行时文件。
- 测试用 `tokio::test(flavor = "current_thread")`（如需要异步）。
- 版本即 v0.1.27（`oj/Cargo.toml` 从 0.1.26 递增）；**不打标签**，CHANGELOG 注明「未打标签」。
- `.route` 值内 `_name_` **不转换**（保持 matchit 语法），只做告警——勿在 rel_pattern 的 route 分支里调用转换。

---

### Task 1: `fs_seg_to_pattern` helper（转换谓词） ✅ 已完成（a93b6d1 后首个提交）

- [x] **Step 1: 写失败测试**
- [x] **Step 2: 跑测试确认失败**（E0425 编译失败，符合预期）
- [x] **Step 3: 实现**
- [x] **Step 4: 跑测试确认通过 + fmt + clippy**（1 passed，clippy 干净）
- [x] **Step 5: 提交**

---

### Task 2: `RouteTable::build` 转换 + `.route` `_name_` 告警 ✅ 已完成

- [x] **Step 1: 写失败测试**（6 个新用例）
- [x] **Step 2: 跑测试确认失败**（4 个 FAILED，符合预期）
- [x] **Step 3: 实现 build 转换**
- [x] **Step 4: 实现 `.route` 告警**
- [x] **Step 5: 改 app.rs 打印循环**
- [x] **Step 6: 跑测试确认通过 + fmt + clippy**（server 99 passed，oj app 25 passed，clippy 干净）
- [x] **Step 7: 提交**

**Interfaces 已交付**：`RouteTable::build` 的 `failures` 中 `warning: ` 前缀条目为告警（app.rs 分流打印、不计 skipped）；无前缀仍是错误。

---

### Task 3: `Routes::resolve` 回溯下降 + 参数提取 ✅ 已完成

- [x] **Step 1: 更新既有 resolve 测试 + 写新失败测试**（4 个新用例；既有 `mirrors_directory_tree_any_depth` 改元组断言）
- [x] **Step 2: 跑测试确认失败**
- [x] **Step 3: 实现 resolve 重写 + `descend` + `underscore_child`**（过程中按 clippy question_mark 改 `?`）
- [x] **Step 4: 改 lib.rs 调用点**（签名 `Option<(PathBuf, HashMap<String,String>)>`，唯一调用点）
- [x] **Step 5: 跑测试确认通过 + fmt + clippy**（server 103+1 passed，clippy 干净）
- [x] **Step 6: 提交**

**执行期偏差记录**：新测试「字面 `_id_` URL」原断言兜底返回参数——实测兜底**快路径**命中磁盘字面目录（无参数）。生产上路由表查询在兜底之前，注册过的 `_id_` 目录由表内 `{id}` 参数吃掉；兜底只服务表外文件，保持字面优先为正确语义。测试已改为断言该行为并注释说明，表侧语义由 `table_underscore_dir_becomes_param` 覆盖。

---

### Task 4: `oj build` — `rel_pattern` 转换 + 构建期 pattern 校验 + 告警 ✅ 已完成

- [x] **Step 1: 写失败测试**（`check_patterns_rejects_invalid_and_conflicting` + `rel_pattern_converts_underscore_dirs_and_module`）
- [x] **Step 2: 跑测试确认失败**
- [x] **Step 3: 实现 `check_patterns`**（过程中修正：matchit 对完全相同 path 重复 insert 也报 Conflict——先按字符串去重，与 RouteTable slots 同口径）
- [x] **Step 4: 实现 `rel_pattern` 转换**
- [x] **Step 5: `build_one` 生成段重构**（收集→告警→校验→写盘）
- [x] **Step 6: 跑测试确认通过 + fmt + clippy**
- [x] **Step 7: 构建产物集成测试**（过程中修正断言：file 字段相对模块根，无模块段 → `"_id_/api.js"`）
- [x] **Step 8: 提交**

**执行期偏差记录**：见 Step 3 / Step 7 的两处测试断言修正（matchit 重复 insert 语义、file 字段相对路径口径），均为测试校准，实现语义与设计一致。

---

### Task 5: e2e — dev 起服 + build→release round trip ✅ 已完成

- [x] **Step 1: 读现有 e2e 形态**（`tmp_project` / `base_cfg` / `MANIFEST` / `BuildArgs` 夹具）
- [x] **Step 2: 写测试** `underscore_dir_param_dev_serves_and_release_round_trips`（dev 表内参数 + 字面 `_id_` 被参数吃掉 + build→release round trip）
- [x] **Step 3: 跑测试**（e2e 全量 22 passed）
- [x] **Step 4: fmt + clippy + 提交**

**执行期说明**：dev→release 两阶段共用临时项目，`drop(JoinHandle)` 为 detach（任务续跑），与既有用例语义一致。

---

### Task 6: 文档与版本（CHANGELOG + devkit 四件 + dev-guide + sample） ✅ 已完成

- [x] **Step 1: 版本递增 + CHANGELOG**（oj 0.1.27 + v0.1.27 节含 breaking-adjacent 升级注意，注明未打标签）
- [x] **Step 2: api-manual.md 路由章**（`_name_` 一级规则 + `.route` 组合 + 解析顺序更新）
- [x] **Step 3: api-manual §13 限制表**（3 行：谓词/同位异名冲突/WS 不转换）
- [x] **Step 4: SKILL.md 陷阱速查**（2 行：`.route` 不转换 / 字面 `_aa_` 被吞）
- [x] **Step 5: scenarios.md 场景 9** + devkit README 两行 + dev-guide 目录镜像节 + sample `user/_id_/api.ts` + sample README curl
- [x] **Step 6: 归置 + 全量校验**（`cargo xtask build` 归置 bin/devkit/；xtask 契约 7 passed；workspace 0 失败；fmt/clippy 干净；sample user 模块产物重建，routes.js 含 `user/{id}` + `_id_/api.js`）
- [x] **Step 7: 提交**

**执行期说明**：sample/dist 为 gitignore 产物（不跟踪），重建仅本地生效；场景编号为 9（现有最大 8）。

---

## Self-Review 记录

- **Spec 覆盖**：§2 helper→T1；§3 挂接点 1/2→T2、3/4→T3、build 侧→T4；§4 校验/告警→T2+T4；§6 测试→T2/T3/T4/T5；§7 文档→T6。无缺口。
- **类型一致**：`resolve` 新签名 `(PathBuf, HashMap<String,String>)` 在 T3 实现与 lib.rs 改法中一致；`warning: ` 前缀约定在 T2 生产、app.rs 消费；`check_patterns(&[String]) -> Result<(), String>` 在 T4 定义与消费一致。
- **占位符**：T4 Step 7 的集成测试允许从邻居夹具复制（文件内既有模式，非 TBD）；T6 Step 5 要求先读目标文件确认形态——均为"读既有模式"而非留空。
