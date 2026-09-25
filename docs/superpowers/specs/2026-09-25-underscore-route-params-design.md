# `_name_` 目录段 → `{name}` 动态参数 设计

日期：2026-09-25
需求来源：`docs/prds/params.md`
评审：架构师 APPROVE WITH NITS + 工程师 NEEDS CHANGES，意见已全部处置（见文末）

## 1. 需求

文件系统中的动态参数目录段用 `_name_` 整段表示，映射 URL pattern 时转换为 `{name}`。
例：`/v1/api/_aa_/bb/_cc_/dd/ee/ff/api.ts` → 基础路由 `/v1/api/{aa}/bb/{cc}/dd/ee/ff/`。
动态参数必须完整路径段（禁止 `/a{cc}b/`）。目的：避免文件路径出现 `{}`，降低 shell 转义成本。

## 2. 核心 helper

`server/src/routes.rs` 新增 **pub** fn（`oj` crate 已依赖 `server`，`build_cmd.rs` 复用零新依赖）：

```rust
/// fs 段 → URL pattern 段。仅整段 `_name_` 转换：len>2、首尾各一个 `_`、
/// inner 不以 `_` 开头/结尾（`__x__`/`___`/`_a__` 不转，宁枉勿纵）、
/// inner 不含 `{`/`}`（`_a{b}_` 不转，提前挡掉而非留给 matchit）。
/// `_`、`__`、`_a`、`a_`、`_shared`、`a_bb_c` 均保持字面。
pub fn fs_seg_to_pattern(seg: &str) -> String
```

## 3. 挂接点（3 处 + 1 调用点适配）

| 位置 | 改动 |
|---|---|
| `RouteTable::build`（routes.rs ~L194） | rel 目录（已 `replace('\\',"/")` 归一）按 `/` 分段逐段转换后拼 `dir_base`。matchit / `decode_params` / 静态优先 / 冲突裁决全部复用 |
| `rel_pattern`（build_cmd.rs ~L662） | `module` 段与 `rel_dir` 逐段转换（PRD「所有类似 `_name_` 的路径段」；`_aa_` 示例正是模块位）。routes.js 的 `file` 字段保留 `_name_` 磁盘路径，dist 目录名不动 |
| `Routes::resolve`（dev 兜底，routes.rs ~L32） | 快路径（直接 join）不变；miss 时**带回溯的 DFS 逐层下降**：每层候选序 = 字面子树 → 排序后首个 `_x_` 候选，子树失败回溯。**每层只试第一个 `_x_`**（同层 `_aa_`/`_bb_` 在 matchit 本是结构性冲突、后者被丢弃；试第二个会命中 release 永远服务不到的幽灵路由） |
| `server/src/lib.rs` ~L421 兜底调用点 | `resolve` 返回 `Option<(PathBuf, HashMap<String,String>)>`：下降经过 `_x_` 段时收集 `(name, 实参段)`，**必须经 `decode_params`**（percent-decode + `.`/`..`/`\`/NUL/`%2F` 走私校验，不过则整次 404）。表内命中本就走 `decode_params`，语义对齐 |

兜底下降加 `ponytail:` 注释：每请求每层一次 read_dir，dev only、仅表外 miss 路径。

### 解析语义（已核对）

- `a/_b_/api.ts` → `/v1/api/a/{b}`；`a/_b_/c/_d_/api.ts` → `/v1/api/a/{b}/c/{d}`，共存无冲突。
- `{b}` 是单段参数（matchit 默认）：`/a/42/c` 对两文件均 404。
- URL 字面 `_b_` 被 `{b}` 当实参吃掉（`b="_b_"`），不再有静态 `_b_` 路由 → SKILL.md 陷阱。
- matchit 自身回溯（静态 `x` 死路回退 `{b}`），兜底 DFS 与之同语义（评审 M2 依据）。

## 4. 构建期校验与告警（build_cmd.rs）

1. **`oj build` pattern 试插校验**：聚合 routes.js 前把每条 pattern 插入临时 `matchit::Router`，非法（参数名含 `{}`、catch-all 非末尾等）**构建即失败**——此前要到部署启动才爆（release 启动 Err）。约十行。
2. **`.route` 值 `_name_` 形态告警**：`.route = "_id_"` 注册为字面段（不转换，保持 matchit 语法纯度高），但极易误写。build 与 dev 建表对匹配 `_name_` 形态的 `.route` 值打 **warn**（dev 走日志；build 走 eprintln；**不进 release 致命 failures 通道**）。
3. **`_name_` 形态模块名告警**：模块名 `_mod_` 合法且正常转换，但两个 `_a_`/`_b_` 模块会同位异名结构冲突 → release 启动整体失败。`oj build` 打 warn 提示。

## 5. 非目标

- WS 路由（`ws.ts`）不支持 `_name_`——`ws.rs:mirror_routes` 仍字面挂载，`_name_/ws.ts` 暴露字面 URL。进 api-manual 限制表。
- `.route` 值内不转换 `_name_`。
- 不禁用 `{id}` 真花括号目录（今天已意外可用）：与 `_id_` 同 pattern 时现有 Conflict 机制钉 500，文档写明 `_name_` 是正典。

## 6. 测试（全部 `cargo test --release`）

现有用例零破坏（仓内无 `_*_` 目录，仅 `_shared`/`_platform`，不以 `_` 结尾）。

- **routes.rs tests**：
  - `fs_seg_to_pattern` 边界表：`_aa_`/`_a_b_` 转换；`_`/`__`/`_a`/`a_`/`__x__`/`___`/`_a{b}_`/`_shared`/`a_bb_c` 不转。
  - 建表：`_aa_/bb/_cc_/api.ts`（PRD 原例扩到 api.ts）pattern + params 提取；`_id_` 与静态 `me` 兄弟同动词优先级（对标 `table_static_sibling_beats_same_verb_param` 的 fs 版）；同层 `_aa_`/`_bb_` 冲突进 failures；`_id_` 目录 + 相对 `.route="{sub}"` → `{id}/{sub}`。
  - resolve 下降：字面优先、回溯命中（`a/_x_/c/api.ts` + 空壳 `a/b/` 请求 `/a/b/c`）、参数提取 + `%2e%2e`/`a%2Fb` 拒、深层混合段、多 `_x_` 候选只试排序首个。
  - `{id}` 字面目录与 `_id_` 同 pattern → Conflict 500 钉死。
- **build_cmd tests**：`rel_pattern` 加 `_id_` rel_dir 与 `_mod_` 模块段用例；产物断言 routes.js pattern 为 `{id}`、`file` 保留 `_id_`；非法 pattern（如 inner 含 `{}` 侥幸漏网时）构建 fail-fast；`.route="_id_"` warn 不致命。
- **e2e（oj/tests/e2e.rs）**：dev 起服 `_id_` 目录 curl 断言 `http.param`；`oj build` → release 直载 round trip。

## 7. 文档（CLAUDE.md 红线交付清单）

- `CHANGELOG.md`：新版条目，**标注 breaking-adjacent**——存量字面目 `_x_` 目录升级后静态段变参数段（URL 仍可达，handler 收到非空 params）。启动 banner 打印转换后 pattern 已天然可见。
- devkit 四件（逐条对齐后 `cargo xtask build` 归置 `bin/devkit/`，`cargo test --release -p xtask` 契约校验）：
  - `api-manual.md` 路由章：`_name_` 映射规则、与 `.route` 组合示例、限制表三条（同层异名 `_x_` 冲突启动丢弃 / `_name_` 必须整段 / WS 目录镜像不转换）、`{...}` 目录兼容说明。
  - `SKILL.md` 陷阱速查三条：`.route` 值不转换（`_name_` 写进 `.route` 是字面量）；字面 `_aa_` URL 被 `{aa}` 吞成实参；`_shared` 无尾下划线不受影响。
  - `scenarios.md`：`_id_` 目录照抄场景。
  - `README.md`：同步可见变更。
- `docs/dev-guide.md`：路由节与管线描述（兜底现在可能带参数）同步。
- `sample/`：加 `sample/src/user/_id_/` 示例 + README 一行 curl。

## 8. 评审意见处置记录

| 来源 | 级别 | 意见 | 处置 |
|---|---|---|---|
| 工程师 | major | 兜底必须提取参数 + 复用 `decode_params` | §3 挂接点 4 |
| 工程师 | major | 下降带回溯 DFS、每层只试首个 `_x_` | §3 挂接点 3 |
| 架构师 | major | 同上两条（独立命中） | 同上 |
| 架构师 | major | 文档红线须入交付清单 | §7 |
| 架构师 | minor | `__x__`/`___` 未定义 | §2 谓词收紧 |
| 架构师/工程师 | minor | inner 含 `{}` 太晚暴露 | §2 拒转 + §4.1 构建期试插 |
| 架构师 | minor | `.route="_x_"` 陷阱 | §4.2 warn + SKILL.md |
| 架构师 | minor | 存量 `_x_` 静默迁移 | §7 CHANGELOG breaking-adjacent |
| 架构师 | minor | `{id}` 目录撞车 | §5 非目标 + 测试钉 500 |
| 架构师 | minor | 模块段转换写死 | §3 + manifests 链路测试 |
| 架构师 | minor | `_mod_` 模块名 | §4.3 warn |
| 工程师 | minor | `ponytail:` 注释 / e2e / dev-guide / sample | §3、§6、§7 |
