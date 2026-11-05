# ojm reload force 设计：删除 `ojm_install_seq` 哨兵（2026-11-05）

## 动机

安装器 reloading 段被迫对用户 config.yaml 自增 `vars.ojm_install_seq`，因为 reloadConfig
的 unchanged 闸门（config 文档 sha + manifests 锁，`oj/src/serve_cmd.rs:497`）需要观察到
差异才换代。哨兵键随模板发布、用户删行即安装失败、每次安装无意义改写用户 config。

v0.1.122 D1 已把 manifests 锁纳入闸门：**版本升级由锁 diff 自然触发**，seq bump 对触发
已非必需。残余缺口只剩：同版本重装（锁、config 全同）恒 unchanged。

## 语义基石（协商裁决）

1. **unchanged 的真实语义 = 「活代已与盘上一致」**：闸门状态（config sha/锁/doc）只在
   commit 成功时更新，三者一致 ⟹ 线上装配就是从这套盘上内容装配的。同版本重装收
   unchanged = 正确的幂等成功，不是故障。
2. **partial 修复重装不需要 force**：partial 时盘锁已被 lock_commit 改写而活代闸门状态
   未更新 → 盘锁≠活锁 → reload 自然触发。幂等重装与修复重装由现有闸门自动区分。
3. **版本可变（允许重发同版本）**：重发检出责任在调用层（市场页/CLI 手持两侧
   sha256，可提示或自动带 force）；安装器不比 sha、不自动 force。不带 force 的重发重装
   收「已是最新」，旧 bits 继续跑——此风险已知并接受。

## 方案

### oj-bin：reload 触发面加 force 旁路

- `ReloadExecutorLike`（`src/bridge/reload_bridge.rs:48`）签名 `Fn() -> …` → `Fn(bool) -> …`。
- `ReloadState::trigger(force)` / `trigger_sync(force)`（`serve/src/reload.rs`）透传。
- HTTP：`POST {base}/_system/config?force=1`（body 仍必须为空——既有 400 校验不动；
  force 取 query）。
- JS：`server.reloadConfig(force?)` → `op_server_reload_config(state, force: Option<bool>)`；
  `bootstrap.js` 挂载点透传（**保持 7-bit ASCII**）。
- `do_reload(ctx, force)`（`oj/src/serve_cmd.rs:467`）：force=true 跳过 unchanged 早退，
  照常影子装配 + commit 换代；此时 applied 可为空、`unchanged` 恒 false。
- 向后兼容：不传 force 行为逐字节不变。

### 安装器（ojm-lesson-scaffold + ojm-lesson-remote，两仓文件逐字相同、改动保持同步）

- 删 `bumpInstallSeq`（text-edit.ts）+ 模板 config.yaml 安装序号段（注释 + 键行）。
- `reloadStage`（installer.ts）：`patchConfigAt` 只保留 anonymous_paths 接线（语义改动，
  非触发手段）；两个文件（config.yaml + `.ojm-preview.yaml` 派生件）同口径；**仅在
  added 非空时落盘**（无意义重写会平白改变文档 sha）。
- `InstallRequest.force?: boolean`；market/api.ts `installBody` 从 `body.force === true`
  透传；reload 面调用改 `reloadConfig(force)`。
- reload.ts 断言改写（R18 修订：unchanged 仅在预期幂等时合法）：
  - forced → 要求 `!unchanged`，**不做 applied 段断言**（无 diff 时 applied 恒空）；
  - 非 forced + unchanged → ok（「已是最新装配」幂等成功）；
  - 非 forced + 变更 → ok；缺省不再断言段集（smoke 段已是行为级效果检查）；
    `ctx.expected` 覆盖机制保留（调用方显式要求时仍逐一 ∈ applied）。

## 触发表（终稿）

| 场景 | 触发方式 | reload 调用 | 终态 |
|---|---|---|---|
| 版本升级 | 锁 diff 自然触发（v0.1.122 已有） | 普通 | done |
| 同版本重装（不带 force） | 闸门判 unchanged | 普通 | done（已是最新） |
| 同版本重装（带 force） | `reloadConfig(true)` | force | done（强制换代） |
| partial 修复重装 | 盘锁≠活锁自然触发 | 普通 | done |

## 非目标

- 不做安装器侧 sha 自检 / 自动 force（调用层责任）。
- 不动 mtime/hash 指纹、锁格式、dev 树、`ensureReloadApi`（reload_api: true 注入保留）。
- anonymous_paths 接线语义不动。

## 执行范围（2026-11-05 裁决）

**本次只改 oj-bin**。安装器侧（ojm-lesson-scaffold / ojm-lesson-remote：删
`bumpInstallSeq`、`force` 透传、reload.ts 断言改写、模板 config.yaml 哨兵段移除）
**本次不执行**，上文安装器小节保留为契约文档——oj-bin 落地 force 后，安装器按
该契约另行采纳；在此之前现状不变（安装器继续 bump seq，force 对它是纯增量能力）。

**builder-check 摸出的延后交付情报**（后续采纳任务的范围清单，此处仅登记）：
- 真正随新项目发布的模板在 `oj-module/packages/cli/templates/`（config.yaml 哨兵段 +
  安装器副本 + 三个 spec），且其 text-edit.ts 与 scaffold 副本已有分叉（181 vs 111 行），
  采纳时须先定 divergence baseline。
- `ojm-config-verify`、`oj-registry` 两个脚手架项目携带同款文件+哨兵。
- `ojm-lesson-scaffold/config.yaml`（seq=4）/`ojm-lesson-remote/config.yaml`（seq=7）
  是活部署态（含 db.sqlite/logs/bin/dist，非 git 仓），哨兵行清理由运维侧执行；
  残留无害（语义基石 1）。
- 调用层 force 生产者（市场页 useMarket.ts + `__client/api.schemas.ts` 契约再生成 +
  CLI install 路径）随安装器采纳一并交付；本次 force 无生产者属预期（安装器未改前
  不受影响）。

## 约束

- `bootstrap.js` 7-bit ASCII；插件 `panic="unwind"`；测试 `current_thread` + 有界轮询
  禁固定 sleep；测试/clippy 一律 `--release`。
- oj-bin 发版红线：`oj/Cargo.toml` 0.1.122 → 0.1.123 + CHANGELOG + devkit 四件逐条对齐
  （api-manual 进 reloadConfig/HTTP 新参 + 错误表，SKILL 陷阱速查，scenarios 可照抄场景）
  + `cargo xtask build` 归置 devkit + xtask 契约用例。

## 留给 builder 的自由度（builder-check 后已裁决项见下）

- HTTP query 解析：`force` 取 `1`/`true`（不区分大小写）视为 true，其余忽略。
- 测试切面：op 形状单测补 force 参；serve_cmd 热更测试补「force 跳过 unchanged」用例；
  e2e 视既有夹具成本决定（不强求）。
- 断言缺省放宽（非 forced 变更路径不再强制段集断言）为设计裁决，非 builder 自由度。
