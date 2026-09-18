# oj DevKit——TS API 开发手册 + agent skill

面向用 oj 框架开发业务项目的开发者与 AI agent。本目录是发布交付物
（`oj-v<version>-<triple>.tar.gz` / `.zip` 内 `devkit/`），由仓库 `docs/devkit/` 经
`cargo xtask build` 归置到 `bin/devkit/` 产出。

| 文件 | 用途 |
|---|---|
| `api-manual.md` | 完备开发手册（13 章）：模块开发、全局对象 API（含 §6 命名 MQ 客户端 Kafka/RabbitMQ 与长任务池、**邮件投递 `Mail`/`mail`**、**大整数与 i64（`toBigInt`/`toDouble`，雪花 id 必读）**、ext_boot.js 运行时扩展）、鉴权租户、测试、配置、构建发布、运维、安全红线 |
| `scenarios.md` | **场景速查（照抄就能跑）**：公开分享页按租户读数据（`db.asTenant`）、SPA 深链回落与每页 meta、`oj test` 测试库隔离、LIMIT 分页陷阱、匿名路径通配形态、多库项目按库迁移与对账（`--db`，v0.1.21）、雪花 id 大整数的生成与精确回写（`toBigInt`，v0.1.22）——每篇给「配置 + 代码 + 验证 + 常见坑」 |
| `SKILL.md` | Claude Code 等 agent 的 skill 入口：工作流、红线、checklist、陷阱速查，按章节号引用手册 |
| `global.d.ts` | handler 全局对象（json/http/db/kv/blob/bus/es/mail/Kafka/RabbitMQ/tasks…）的 TS 类型声明；拷进项目源码根即获得编辑器/agent 类型提示。来源为 `sample/global.d.ts`，经 `cargo xtask build` 与本目录文档一同归置到 `bin/devkit/` |
| `oj-modules.d.ts` | **`#` 导入别名的 ambient 兜底**（`declare module "#*"`）：oj 的 `#x`/`#/m/x` 是「引用方模块」相对别名，TS `paths` 无法表达，无法静态定位的 `#` 导入会被兜底为 `any`（不再报 TS2307）。**非模块** `.d.ts`，须与 `global.d.ts` 一并拷入项目并纳入 tsconfig `include` |

## 安装（业务项目）

```sh
# npm 安装（v0.1.13 起）：postinstall 把对应平台的 oj / plugins / devkit/ 落盘 <项目根>/bin/
npm i @oj-bin/oj
mkdir -p .claude/skills/oj-api-dev
cp bin/devkit/SKILL.md bin/devkit/api-manual.md bin/devkit/scenarios.md .claude/skills/oj-api-dev/   # agent 用
cp bin/devkit/global.d.ts bin/devkit/oj-modules.d.ts .                       # 类型提示 + `#` 别名兜底
# oj-modules.d.ts 须被 tsconfig 收录（如 include 里含 "**/*.d.ts" 或显式列出），
# 否则 `#` 开头的导入在编辑器里仍会报 TS2307「Cannot find module」。
# 业务项目还需按 oj 的别名规则在 tsconfig paths 里补 `#/*`/`#*`（见 sample/tsconfig.json）。

# 或从发行包解包后取 devkit/，路径同上
```

安装后 agent 里说"用 oj-api-dev 开发 xxx 模块"，或 Claude Code 里 `/oj-api-dev` 触发。

## 更新

手册与 skill 随 oj 版本一起发布；升级 oj 后用新包内 `devkit/` 覆盖旧拷贝。
源文件与反馈入口在仓库 `docs/devkit/`。
