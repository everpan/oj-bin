# oj DevKit——TS API 开发手册 + agent skill

面向用 oj 框架开发业务项目的开发者与 AI agent。本目录是发布交付物
（`oj-v<version>-<triple>.tar.gz` / `.zip` 内 `devkit/`），由仓库 `docs/devkit/` 经
`cargo xtask build` 归置到 `bin/devkit/` 产出。

| 文件 | 用途 |
|---|---|
| `api-manual.md` | 完备开发手册（12 章）：模块开发、全局对象 API、鉴权租户、测试、配置、构建发布、运维、安全红线 |
| `SKILL.md` | Claude Code 等 agent 的 skill 入口：工作流、红线、checklist、陷阱速查，按章节号引用手册 |
| `global.d.ts` | handler 全局对象（json/http/db/kv/blob/bus/es…）的 TS 类型声明；拷进项目源码根即获得编辑器/agent 类型提示 |

## 安装（业务项目）

```sh
# agent 用：拷入项目的 Claude Code skill 目录
mkdir -p .claude/skills/oj-api-dev
cp devkit/SKILL.md devkit/api-manual.md .claude/skills/oj-api-dev/

# 类型提示：拷进项目源码根（与 src/ 平级即可）
cp devkit/global.d.ts .
```

安装后 agent 里说"用 oj-api-dev 开发 xxx 模块"，或 Claude Code 里 `/oj-api-dev` 触发。

## 更新

手册与 skill 随 oj 版本一起发布；升级 oj 后用新包内 `devkit/` 覆盖旧拷贝。
源文件与反馈入口在仓库 `docs/devkit/`。
