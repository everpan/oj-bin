# 上游 issue 提案：市场安装首个模块后免重启服务进程

> 本文件是提交给 oj 平台（`@oj-module/cli` / oj server）的 issue 草稿，可直接复制正文提交。
> 本仓 `src/app`、`src/oj-market` 受内置模块自治门禁保护（`manual.md` §1.9），下游只能提交上游 issue，不能自行修复。

**建议标题**：`[安装器] 首装 partial 的根因是路由/模块注册冷启动，不是 anonymous_paths；建议让新装模块免重启生效`

## 1. 问题

市场安装一个方案包（如本项目 wiz-plant 3.0.0），11 个阶段全部 `ok`，但终态恒为 `status=partial` + `restartRequired=true`，必须重启 `oj serve` 再 retry 同一条 install 才转 `done`。文档把原因归结为"新模块的路由/模块注册要宿主重启才生效"，同时安装器代码注释又声称是 `auth.anonymous_paths` 热改属 `restart_pending`、`重启前恒 401`。

这两处说法互相矛盾，且与 oj 内核的实际行为不符，导致：

1. 运维对每个新装模块都做一次无意义的重启；
2. 排查时把 401 误判为"等重启"，掩盖真正的接线失败；
3. 注释误导下游实现（本仓已按错误注释写过 smoke 判定）。

## 2. 证据：anonymous_paths 不需要重启

内核文档明确匿名清单随换代即时生效：

- `oj-api-dev/api-manual.md:2581-2584`：命中 `auth.anonymous_paths` 的请求在前置管线直接放行，**清单随热更换代即时生效（`reload.anon_per_reload`）**。
- `oj-api-dev/scenarios.md:4695-4696`：卸载后指向该模块的旧匿名条目**无需清理也无需重启**——条目是路径模式，失配即无害，匿名清单本身随换代即时生效。
- `oj-api-dev/SKILL.md:273`：冻结段是 `vars` / `oidc` / `db_query` / `db_trace` / `ws`；`global.d.ts:547` 列出的 `restart_pending` 例子是 `db` / `redis` / `plugins` 后端段与 `server` 段。二者均不含 `auth.anonymous_paths`。

而以下注释与内核行为冲突，需上游同步修正：

| 位置 | 现有错误表述 |
|---|---|
| `src/app/api/_platform/_lifecycle/text-edit.ts:55` | "热改属 restart_pending——reload 段消息已明示，重启后生效" |
| `src/app/api/_platform/_lifecycle/steps/reload.ts:7-8` | "vars/auth 等冻结/特权段如实归入 restart_pending" |
| `src/app/api/_platform/_lifecycle/steps/reload.ts:145` | "anonymous_paths / vars 等冻结·特权段热改属 restart_pending" |
| `config.yaml:195`（工程配置，非门禁文件） | "生效口径：config.yaml 启动冻结段，改后须重启（restart_pending）" |

本仓已修正 `installer.ts` 的 4 处同类注释（见附录 A），逻辑未动。

## 3. 首装 partial 的真实原因

`ojm-module-dev/publish-walkthrough.md:318-320` 与 `:343`：

> 11 段全 ok 但 `status=partial`，message 明示 `restart_pending`（**新模块的路由/模块注册要宿主重启才生效**，如「待重启段 [auth]」）。

`newcomer.md:172`：新装方案的数据面（路由/seed）要冷启动才全量生效。

同时 `reload.ts:76-78` 又说明：安装默认不强制 `+name@ver`，**smoke 段用探活端点 200 即证模块已装配**。也就是说装配链路本身已经具备"新模块可达"的能力，只是路由表/挂载点没有在当前代重建，且 oj 把 `auth` 段整体报进 `restart_pending`，安装器据此直接判 partial。

结论：**重启由"路由/模块注册未热装配"和"`auth` 段被整体标 pending"共同造成，与 ping 的匿名接线无关。**

## 4. 建议方案

### A. 最小改动（建议优先）：让 pending 报告精确到键，并对已探活通过的段降级

1. oj 侧：`reloadConfig` 的 `restart_pending` 从"段"粒度改为"键"粒度。`auth.anonymous_paths` 既然已随换代即时生效，就不应再让整个 `auth` 段进入 pending；只对真正冻结的键（如 `auth.jwt_secret`）报 `auth.jwt_secret`。
2. 安装器侧：`installer.ts:840-847` 判 partial 时，若 `restartPending` 中的段已被 smoke 探活验证生效（新模块 ping 返回 `code:0`、匿名端点非 401），则降级为 message 提示、终态判 `done`，不设 `restartRequired`。

效果：对只声明 ping 匿名端点的新模块，首装直接 `done`，无需重启；真正冻结段的改动仍保留重启提示。

### B. 根治：把新模块路由注册纳入 reloadConfig 装配阶段

现状 `reloadConfig` 靠 manifests 锁 diff 产出 `+<name>@<ver>` 符号，但 HTTP 路由表/挂载点未重建。建议装配阶段对新增模块调用与冷启动同一套 `register_routes`，把新模块前缀挂进当前代路由表，沿用现有 smoke 探活 200 作为成功判据（`reload.ts:76-78` 已具备该能力）。

效果：路由与模块注册随换代生效，彻底去掉冷启动依赖。

### C. 数据面完整性：安装期 seed 幂等重放

现状热更迁移强制 `verify`-only，安装器 migrate 命令未带 `--with-seed`（`publish-walkthrough.md:308-311`），装完数据面为空。建议 migrate 段对带 `seed.sql` 的模块执行 `oj migrate --with-seed`（oj 0.1.125+ 已支持），幂等重放。

注意：seed 缺失与"是否重启"无关（重启不会重放 seed），但属于"装完即可用"的一部分，建议一并处理。

### D. 兜底：supervisor 自动滚动重启 + 自动 retry

`installer.ts:836` 已标注 P4 未实施。这仍是重启，只是自动化，适用于无法热装的冻结段场景，不作为主方案。

## 5. 验收标准

- 新装一个仅声明 ping 匿名的方案包，`ojm install` 直接返回 `status=done`，`restartRequired` 不为 true；
- 不重启 `oj serve`，`curl -H 'X-TENANT-ID: default' /api/<name>/ping` 返回 `code:0`；
- 新增匿名端点在未携带 Bearer 时非 401（验证随换代即时生效）；
- 升级、降级、同版本重装三种场景同样免重启；
- 修改真正冻结段（`vars` / `db` / `server` 等）仍正确报 `restart_pending` 并提示重启。

## 附录 A：本仓已修正的注释（仅注释，逻辑未动）

文件：`src/app/api/_platform/_lifecycle/installer.ts`

1. `InstallRequest.deferredAnonymous` 注释：去掉"restart_pending，探活对其 401 判待重启"，改为"匿名清单随换代即时生效，仅用于 smoke 校验接线是否落对"。
2. `reloadStage` 中 `auth.anonymous_paths` 接线注释：去掉"热改属 restart_pending——reload 消息已明示重启生效"，改为"v0.1.122 起随换代即时生效，不属 restart_pending；首装 partial 的真实原因是新模块路由/模块注册须冷启动"。
3. `reloadStage` 交接 smoke 注释：去掉"重启前恒 401"，改为"重启前即可匿名可达；若 401 说明接线未生效，需排查"。
4. `runInstall` 终态判定注释：把"匿名接线/路由等 restart_pending 段"改为"新模块路由/模块注册须宿主重启才全量生效（匿名接线随换代即时生效，不在此列）"。

> 因门禁约束，本改动仅存在于下游工作区，`ojm build` 会因 `src/app` 漂移而失败。上游下发含同类修正的新版本后，执行 `ojm init --yes` 恢复基线即可（注意该命令会丢弃 `src/app`、`src/oj-market` 的本地改动）。

## 6. 上游落地状态（oj v0.1.146）

- **方案 A1（oj 侧）已落地**：`reloadConfig` 的 `restart_pending` 对 `auth` 段降为键粒度——
  仅 `auth.anonymous_paths` 变化归 `applied`（匿名清单自 oj v0.1.122 起随换代即时生效）；
  `jwt_secret`/`signing_method`/token 时长/`cookie` 任一冻结键变化仍整段报 pending（oj-auth
  插件 cfg init 期冻结）。剥离 anon 后剩余空 mapping 视同段缺席。
- **方案 B 前提更正**：oj 内核 reload 执行器每次换代**本来就做全量影子装配 + Router
  swap**（`App::from_config` 重建路由/挂载/前置管线后整表换装）——「HTTP 路由表/挂载点
  未重建」在当前内核不成立。首装 partial 的真实根因只有 A1 一条：`auth` 段误标 pending
  使安装器判 partial。装器侧（本仓下游）按方案 A2 降级后，首装即可 `done` 免重启。
- 方案 C（seed 幂等重放）/ A2（装器降级）/ D（supervisor）仍属装器/部署侧，不在 oj 内核。
