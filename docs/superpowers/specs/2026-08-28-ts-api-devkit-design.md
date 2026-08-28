# TS API DevKit 设计（业务项目开发者手册 + agent skill，随版本发布）

日期：2026-08-28
状态：已与用户逐节确认（§1–§6 均获批准）

## 1. 背景与目标

oj 的业务逻辑以 JS/TS handler（`api.ts` / `WS.ts`）编写，开发面分散在
`docs/user-manual.md`（对外行为）、`docs/testing.md`（两层测试）、`docs/ops-manual.md`
（运维）中，且面向本仓库验收载体而非下游业务项目。目标：

1. 一份**完备大手册**（模块开发 + config 全字段 + 部署运维），面向「用 oj 开发业务项目」
   的开发者与 agent；作为唯一事实源按章节组织。
2. 一个 **skill**（agent 工作流入口），精简、指向手册章节，供 Claude Code 等 agent 在
   业务项目中对照手册开发 api 模块。
3. 交付物落在 **`bin/` 随版本打包发布**（`scripts/deploy.sh` 产物链）。
4. 顺带把 `sample/global.d.ts` 补齐到 v0.2 实际 API（当前缺 `http.tenantId/user/files/file`、
   `blob`、`bus`、`es`、`db.tx`、`kv.expire/incr` 等），并**随包发布**（用户裁决 2026-08-28：
   `global.d.ts` 也一起发布）。

### 非目标

- 不改运行时行为、不改 CLI、不动现有 docs（`user-manual.md` 等保持原位，仅新增指针）。
- 不做手册版本号戳记（版本由 tarball 名 `oj-v<version>` 携带）。
- 不内置到 oj 二进制（纯数据交付物，`bin/devkit/` 目录级拷贝）。

## 2. 交付物布局与发布链

```
docs/devkit/                  # 源（git tracked）
├── README.md                 # 是什么 / 怎么安装到业务项目 / 怎么更新
├── SKILL.md                  # agent 入口（frontmatter: name/description）
└── api-manual.md             # 完备大手册（唯一事实源）
sample/global.d.ts            # 全局对象 TS 声明源（单一事实源，仍在 sample/）

cargo xtask build  ──►  bin/devkit/           # docs/devkit 拷贝 + 从 sample/ 拾取 global.d.ts
scripts/deploy.sh  ──►  dist/oj-v<version>.tar.gz 内
                        devkit/{README.md,SKILL.md,api-manual.md,global.d.ts}
```

`global.d.ts` 的**源**保留在 `sample/global.d.ts`（sample 的编辑器提示与 L2 vitest 依赖它），
xtask 拷贝时一并拾取进 `bin/devkit/`——仓库内不存两份，无漂移。

下游安装：`SKILL.md`、`api-manual.md`（README 视情况）拷入业务项目
`.claude/skills/oj-api-dev/`；`global.d.ts` 拷入业务项目源码根获得全局对象类型提示。
SKILL.md 以**同目录相对路径**引用 `api-manual.md`（不依赖仓库结构，天然可移植）。
本仓库自身 agent 使用时拷贝/软链 `docs/devkit/` 到 `.claude/skills/oj-api-dev/`
（`.claude/` 被 gitignore，不入库）。

## 3. api-manual.md 章节结构（12 章）

以 `user-manual.md` 为底，合并 `testing.md`、`ops-manual.md` 的开发视角内容，按模块开发
动线重组。每章开头一行「何时读我」，便于 agent 按需跳章。仓库内部实现细节指向
`dev-manual.md`，不在本手册展开。

| # | 章节 | 内容要点 | 主要来源 |
|---|---|---|---|
| 1 | 总览与快速开始 | oj 是什么；最小闭环：oj-cert 生成证书 → config → server → curl | user-manual §1、dev-manual §5.1 |
| 2 | 项目结构与模块约定 | src 树（首层=模块、manifest 必配、name=目录名）、`_shared` 工具目录、node_modules 裸 specifier、dist 产物布局 | user-manual §4/§5 |
| 3 | 编写 api.ts | 动词→方法名映射（**DELETE→`del`**）、同步函数+Promise 链 / async、`http.body` 解析规则、`.route` 参数路由（matchit 语法、镜像替换语义、build 剥离）、`WS.ts` 帧循环 | user-manual §6/§7、route-params-design |
| 4 | 导入解析 | 相对导入补全（.ts→.js→index）、裸 specifier（module→main→index.js）、CJS 互操作、root 钳制、v0.2 限制（相对 require 不支持等） | user-manual §8 |
| 5 | 全局对象 API 参考 | json/http/db/DB/kv/redis/blob/bus/es/log/fetch/ws 全表；db.tx 语义（单事务、自动回滚）；查询构造器；鉴权字段（http.user）、上传（http.files/file）、租户（http.tenantId）；SQL 占位符方言 | user-manual §9 |
| 6 | 响应信封与错误码 | `{code,msg,data}`、HTTP 状态映射、404/405/408/413/500 场景表 | user-manual §10 |
| 7 | 鉴权与多租户 | auth 块、内置 /auth/* 路由表、Bearer 守卫、anonymous_paths、user 表最小 schema、tenant 头；auth_demo 走读 | user-manual §9、testing.md 约束 |
| 8 | 测试 | L1 `oj test`（client/describe/it/expect、junit、退出码门禁）+ L2 vitest mock（invoke/installGlobals）+ 选型表 + CI 片段 | testing.md 全量 |
| 9 | 配置 config.yaml 全字段 | server/db/redis/es/blob/broker/plugins/plugins_dir/auth/tenant/seed.sql；证书必配不可绕过语义；fail-fast 行为清单 | user-manual §3 |
| 10 | 构建与发布 | `oj build`（版本目录、minify、routes.js、manifests.yaml 锁、确定性 tgz、跨模块导入改写）、release 聚合加载、部署布局 | user-manual §2、dev-manual §8 |
| 11 | 运维要点 | 证书 gen/renew（tools/oj-cert）、`/health` 探测、宽限期行为、插件四级发现/装配/升级回滚、日志 | ops-manual §3/§4/§7 |
| 12 | 安全红线与已知限制 | **SQL 注入红线**（动态标识符只来自构造器白名单、值只走绑定参数）；v0.2 已知限制表；常见陷阱 | user-manual §12、CLAUDE.md 设计红线 |

**一致性要求**：所有 API 行为声明以 `user-manual.md` §9 与 `src/bridge/bootstrap.js` 实际
挂载为准逐条核对；手册与 user-manual 冲突时以 user-manual 为准并修手册（发现 user-manual
本身过时则另立任务，不顺手改）。

## 4. SKILL.md 结构（目标 ~120 行）

1. **frontmatter**：`name: oj-api-dev`；`description` 写明触发时机——在 oj 框架业务项目中
   新增/修改 `api.ts`、`WS.ts`、`manifest.yaml`、模块测试，或排查信封/路由/鉴权行为时使用。
2. **工作流 5 步**（每步标注手册章节号，按需读章，不盲读全文）：
   1. 读章：§2 结构 → §3 写法 → §5 API；
   2. 脚手架：目录 + `manifest.yaml`（name 必须等于目录名）；
   3. 写 handler（红线内联在 skill 中，不依赖读章）：
      - 动态 SQL 标识符**只**来自 `db.table()` 构造器；值**只**走绑定参数，绝不拼接；
      - DELETE 的方法名是 `del`；
      - 响应一律 `json.ok` / `json.fail` 收口；
   4. 测试：先 L2 vitest 测逻辑，再 L1 `oj test` 测端到端（§8）；
   5. 发布检查：`oj build` → 确认 dist 锁与产物（§10）。
3. **新模块 checklist**：manifest 在、目录映射对、方法名映射对、`.route` 的 build 剥离
   影响、信封统一、两层测试跑过。
4. **常见陷阱速查表**（~10 行）：`del` 映射、build 剥 `.route`、release 下 WS URL 含版本段、
   postgres 占位符 `$1`、seed.sql 不得含分号字面量、上传 413 双闸、408 超时、`{id}.json`
   类混字面 pattern 非法、路径参数不拼文件路径、尾斜杠均可。
5. **手册引用约定**：`api-manual.md` 与本文件同目录。

## 5. 构建链集成

- `tools/xtask/src/main.rs`：仅 `build` 子命令（全量归置）在插件拷贝后新增
  `copy_devkit()`（约 12 行：`fs::copy_dir_all(docs/devkit, bin/devkit)` +
  `fs::copy(sample/global.d.ts, bin/devkit/global.d.ts)`；任一源缺失报错退出）。
  `bin` / `plugin` 单体子命令**不**拷贝（避免单插件构建拖文档）。
- `scripts/deploy.sh`：装配 `TEMP_DIR` 时加 `cp -R "${PROJECT_ROOT}/bin/devkit" "${TEMP_DIR}/devkit"`；
  产物校验处补 `bin/devkit/api-manual.md` 与 `bin/devkit/global.d.ts` 存在性检查。
- `CLAUDE.md`：docs 列表处加一行指针（`docs/devkit/` = 业务项目开发者手册 + skill 源，
  经 `cargo xtask build` 归置到 `bin/devkit/` 随包发布）。

## 6. sample/global.d.ts 补齐清单

对齐 user-manual §9（v0.2）：

- `HttpApi`：补 `tenantId: string | null`、`user: { id; roles: string[]; claims } | null`、
  `files: { field; filename; content_type; size }[]`、`file(i: number): Promise<Uint8Array>`；
  `param` 默认值行为对齐（路径参数优先，query 兜底）。
- `KVApi` / redis：补 `expire(key, ttlSec)`、`incr(key)`。
- 新增：`BlobApi`（put/get/del/url/contentType）、`BusApi`（publish/subscribe）、
  `EsApi`（search/index/del）、`DBInstance.tx`（`tx.query/exec/table` 同签名）、
  `blob`/`bus`/`es` 全局常量。
- `fetch` 签名与现状核对修正。
- 不删除现有声明（L1/L2 测试 SDK 部分原样保留）。

## 7. 验收方式

1. `cargo xtask build` 后 `bin/devkit/{README.md,SKILL.md,api-manual.md,global.d.ts}` 齐全，
   前三件与 `docs/devkit/` 内容一致，`global.d.ts` 与 `sample/global.d.ts` 一致。
2. `bash scripts/deploy.sh` 产出的 tarball 解包含 `devkit/` 四件（手工验证一次）。
3. 手册一致性核对：第 5 章每条 API 对照 user-manual §9 与 `bootstrap.js` 挂载（spec 附录
   清单逐项打勾，见 §8）。
4. SKILL.md 可被 Claude Code 识别：拷入 `.claude/skills/oj-api-dev/` 后 `/oj-api-dev` 可触发。
5. `sample/global.d.ts` 补齐后 `cd sample/test && npm ci && npx vitest run` 全绿（类型不破坏
   现有 L2 测试）。
6. 门禁：`cargo fmt --check`、`cargo clippy --all-targets -D warnings`（xtask 改动受检）。

## 8. 附录：第 5 章 API 一致性核对清单

以 `src/bridge/bootstrap.js` 挂载 + user-manual §9 为准，手册 API 表逐项核对：
`json.ok/fail/header`；`http.method/query/headers/body/params/param/tenantId/user/files/file`；
`db.query/exec/table/tx`；`DB(name)`；`kv.get/set/del/expire/incr`；`redis.*` 同源；
`blob.put/get/del/url/contentType`；`bus.publish/subscribe`；`es.search/index/del`；
`log.debug/info/warn/error`；`fetch(url, options?)`；`ws.send/close`。共 13 组，
每组在手册行内标注来源章节，核对时逐项打勾。
