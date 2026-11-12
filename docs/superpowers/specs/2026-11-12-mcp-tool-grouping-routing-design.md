# MCP 工具分组与检索路由设计（L1 分组 + L2 检索式路由）

日期：2026-11-12 · 状态：评审稿（三方评审已并入：MCP 协议专家 / 架构师 / 工程师）
· 动机来源：lobehub smart-router（warden）调研 + 「方案市场装得越多，MCP tool
越多」的上下文/查找双重疼痛

## 意图与为什么

方案市场增长带来两个已确认的疼痛：

1. **上下文疼痛**：tools/list 平铺全部工具，每个工具的 schema+description 常驻
   模型上下文。业界经验阈值 30–50 个可见工具后选择准确率衰减。
2. **查找疼痛**：模型在几百个平铺工具里找不到/选错该用的工具。

业界 2026 共识解法：渐进披露/工具检索（-85% 工具定义占用）、按会话/角色门控、
网关侧 manifest 路由、元工具两级寻址（smart-router 常驻 5 个元工具，
search/route 后再 call）。

**为什么我们能做得比 generic MCP 便宜**：工具单一来源（路由表 → openapi →
tools，`serve/src/mcp_http.rs::tools_for`），两条线路（`oj mcp` stdio 快照 +
serve `/mcp`）同源；每个 tool 自带模块 tag（`tags[0]` = `module_of(file)`，
v0.1.70）；`server.mcp.modules` 白名单 + scopes/act_mask 按凭据裁剪已存在；
**调用面本就不裁**（tools/call 走 modules 后全集、admit 单点拦）——这正是
smart-router「route 后再 call」所需的形状，零改动已具备。

## 目标 / 非目标 / 反目标

**目标**

- L1：tools/list 按「解决方案模块」（= 现有 tags 模块名）分组呈现，客户端可
  折叠；分组信息对两条线路一致可用。
- L2：`server.mcp.expose: routed` 模式下，tools/list 只出常驻元工具 +
  `oj_search_tools` 检索，模型按需取 top-K 工具全量 schema；调用面语义不变。
- 两条线路（serve `/mcp` 与 `oj mcp` 快照）同模式同语义（本期语义=
  同一份一致性向量钉住的子串匹配规则；线路 A 的已知分歧见专节）。

**非目标**

- 不做描述/字段瘦身（用户明确砍掉 L0）。
- 不动 scopes/act_mask/modules 白名单的既有语义（只在其上叠加）。
- 不动 `/rpc` 裸 JSON-RPC 线（无列表语义，天然免疫；但见「调用面」节的
  operationId 撞名说明）。
- 不做服务端主动推送——但把 `notifications/tools/list_changed` 点名预留为
  扩展点（per-session manifest 的数据面已通：read_inbound 已带 user）。

**反目标**

- 不允许检索面泄权：search 结果必须经与 tools/list 相同的展示裁剪
  （scopes/act_mask），routed 模式下 search 返回形状与「不存在」不可区分。
- 不允许破坏 flat 模式零行为变化（默认模式，tools/list 输出字节不变）。

## 设计

### 配置面

`server.mcp` 段新增一个字段（与既有 `modules`/`act_mask_destructive` 并列）：

```yaml
server:
  mcp:
    expose: flat | grouped | routed   # 缺省 flat = 现行为，零破坏
```

- 配置链路：`src/config.rs` 的 McpCfg 加字段（三值 serde 枚举，非法值 parse 期
  天然 fail-fast）→ `oj/src/app.rs` 装配进 serve McpConfig →
  `serve/src/lib.rs` McpConfig.expose。`oj/src/args.rs` 的 McpArgs 加
  `--expose`（flag 显式给时覆盖 config，缺省跟随 config）。
- 工具名保留前缀 `oj_`：装配期扫描**所有 mount** 的 tools  fail-fast
  （报错点名肇事 route）；**扫描必须基于 spec_to_tools 净化后的 tool 名**
  （`oj.search-tools` 这类 operationId 会净化成 `oj_search_tools`，扫描原始
  operationId 兜不住）；`oj mcp` codegen 跑同一检查（否则 serve 能起、产物
  静默撞名）。这是潜在破坏性变更（存量 operationId 可净化出 `oj_` 名），
  CHANGELOG 明写。dev 热重载新增 `oj_*` 会绕过装配扫描被静默遮蔽——接受为
  已知限制（dev 面，随装配 fail-fast 的既有语义）。

### 逐模式 list 形态（写死，防双实现各自脑补）

| 模式 | tools/list 的 tools | result 附加 | list 出元工具？ |
|---|---|---|---|
| flat | 全量（现行为，字节不变） | 无 | 否（call 面仍可 call，见下） |
| grouped | 全量（同 flat） | `_meta` groups | 否 |
| routed | 仅 `oj_search_tools` + `oj_list_groups` | 无（组面经 `oj_list_groups`） | 是 |

### L1：分组呈现（grouped）

groups 放 **result._meta**（spec 认可的扩展位，反域名点分键），不放 result
顶层——严格反序列化客户端（Jackson FAIL_ON_UNKNOWN 有真实翻车先例）对顶层
未知键不容忍；tool 对象内的非标准 `tags` 键是既有事实且未遭投诉，保持不动。

```json
{
  "tools": [ ... 与 flat 完全相同（含 tags） ... ],
  "_meta": { "com.only-js.groups": [ {"name": "wiki", "tool_count": 12} ] }
}
```

- `name` = 模块 tag 值；无 tag 的工具不进任何组。
- **v1 省略 `description` 键**（评审裁决）：serve 侧 ModuleEntry 只有
  root/version/prefix，manifest `desc` 只在 oj 侧解析，无现成通道；要支持需
  动 src/bridge/module_scan.rs + 加访问器，收益不成比例。未来要加再立项。
- 裁剪后 0 可见工具的组省略。
- groups 与 tools 经过同一展示裁剪（scopes/act_mask/modules 白名单）——
  分组信息不放大暴露面。
- 消费方式完全在客户端：折叠/渲染由 LobeHub、Claude 等自行决定；oj 只保证
  信息在场。未知键忽略属客户端义务（写进限制表）。

### L2：检索路由（routed）

tools/list 只返回常驻元工具：

| 元工具 | 签名 | annotations | 行为 |
|---|---|---|---|
| `oj_search_tools` | `{query: string, group?: string, limit?: number}` → `{matches: [...]}` | `readOnlyHint: true, idempotentHint: true` | 见下 |
| `oj_list_groups` | `{}` → `{groups: [{name, tool_count}]}` | `readOnlyHint: true, idempotentHint: true` | 与 grouped 模式同一 groups 面 |

`oj_search_tools` 行为：

- 在**展示裁剪后**的工具集内做大小写不敏感子串匹配（name/summary/description/
  group），按 name 命中优先、description 次之排序。
- **matches 每项透传 `annotations`**（readOnlyHint/destructiveHint 是客户端
  审批门控的正式载体，routed 下不能丢——比 flat 更不安全不可接受）。
- 每项形状：`{name, description, inputSchema, group, annotations}`——完整
  inputSchema 到场即可直接 call。
- `group` 精确过滤；`limit` 缺省 10、上限 50（超限钳 50）。
- 无命中 → `{matches: []}`，与「无权/不存在」同形状，防探测。
- **接线硬要求（评审 M3）**：search 在 call 面执行，endpoint user 目前只流到
  tools/list 分支——必须把 user 穿进 call_tool（act_mask 从 st.pipeline 取），
  search 内部复用 filter_tools_for_display 同款裁剪。

调用面：

- **成本口径**：tools_for 本来就是每请求现算（tools/list 与 tools/call 各自
  一次），search 同量级无新增渐近成本。spec 禁止引入跨请求缓存按 user 裁剪
  的结果（防探测）；若将来缓存，只能缓存 modules 后未按用户裁剪的集合。
- `oj_*` 短路插点：**call_tool 内在 tools_for 之前**按 name 前缀拦截（meta
  handler 内部自行 tools_for + filter_tools_for_display）。理由：openapi
  generate 报错不得把元工具一并打成 -32603；与「未知名 → -32601」顺序无交叠。
- 业务工具调用完全不变（modules 后全集、admit 单点拦）；未知名 → 现有
  -32601。
- **防探测威胁模型口径（评审裁决，需用户确认）**：search 面同形状，但
  tools/call 仍区分「被 scopes 裁掉 → admit 403」与「不存在 → -32601」——
  403 是**授权语义**，探测者绕过 search 直接 call 候选名仍可分出存在性。
  **接受成文**（方案 a）：routed 隐藏的是**上下文占用**，不是存在性；不为
  隐藏存在性破坏「调用面语义不变 + 三面同规则」。方案 b（call 面合成
  -32601）已否决。

### 线路 A：`oj mcp` stdio 快照

- **配置驱动、flag 覆盖**：`oj mcp` 读 `server.mcp.expose`（`--expose`
  显式给时覆盖，缺省跟随配置，配置缺省 flat）。
- **现状对齐（评审 M1，事实修正）**：线路 A codegen **从不调用**
  apply_modules_filter（它只存在于 serve/src/mcp_http.rs）——线路 A 产物
  today 内嵌**无 modules 过滤的全集**。本设计保持现状（检索/分组集合 = 产物
  内嵌集合），**不**新增 codegen 期 modules 过滤（那是独立的既有行为变更，
  要做得另立项）。
- **已知分歧（成文，不修复）**：线路 A 无 endpoint-admit user 概念，检索/
  分组集合 = codegen 期全集，**不做** scopes/act_mask 按用户裁剪；serve 侧
  （线路 B）是唯一按凭据展示裁剪的面。devkit 限制表写明。
- grouped：渲染期把 groups 元数据嵌进产物（`__GROUPS_JSON__` 占位替换，
  照搬 `__TOOLS_JSON__` 惯例）。
- routed（**评审 B1 修订，按此前描述实现必坏**）：模板 callTool 首句是
  `const d = DISPATCH[name]; if (!d) return isError("unknown tool: "+name)`——
  元工具不在 DISPATCH，必须先拦。模板改法：
  1. 新增占位 `__EXPOSE__`（flat/grouped/routed）与 `__GROUPS_JSON__`；
  2. 产物内嵌本地 `searchTools(query, group, limit)`（对嵌入式 TOOLS 的同步
     过滤，规则与 serve 侧逐条一致）；
  3. dispatch() 的 tools/call 分支入口先按 `oj_` 前缀拦截：`oj_search_tools`/
     `oj_list_groups` 走本地 `searchTools`/groups 数据直接回 result，**不发
     HTTP**；非 `oj_` 名照走既有 DISPATCH/HTTP 转发逻辑。
  4. tools/list 分支按 `__EXPOSE__` 渲染：routed → 只出两个元工具（带
     annotations）；grouped → 全量 + `result._meta` groups；flat → 现产物
     字节不变。
- **双实现漂移税（评审 M2，接受并记终点）**：本期 Rust + JS shim 两份子串
  匹配是既定成本；测试用**共享 JSON 一致性向量**（query→期望
  matches/排序/limit）钉语义等价——Rust 单测与 codegen 测试（node 跑同一份
  向量）共用。收敛终点写死：线路 A 未来变 stdio 薄代理（list/search/call
  全转发 serve，call 本就在线），向量检索落地即触发收敛。

### 错误与限制（成文口径）

- routed 的「tools/call 接受未在 tools/list 出现的 name」：**MCP spec 允许**
  （对 name 无「必须出自 list」条款），差异在客户端策略（个别 harness 把
  list 当 call 白名单）。渐进披露有成熟生态先例（mcp-tool-search、warden、
  Codex deferred loading），主流客户端实测可通。限制表措辞按此写，不写
  「违反严格客户端假设」。
- search 为子串匹配，非向量语义检索；描述写得不含关键词的工具仍可能找不到。
- 时序侧信道（接受口径）：search 耗时与语料字节数正相关，「可见工具数」在
  routed 下是被隐藏信息但可通过时序粗测——实现用固定扫描路径（不提前退出）
  压到噪声级；不为此加恒定延迟。
- `/mcp` 响应补 `Cache-Control: no-store`（防中介缓存按凭据裁剪的结果串号）。
- `groups`/`oj_*` 均不进 `/rpc` 线；但业务 operationId 净化出 `oj_` 名时
  /rpc 可调而 /mcp 被短路——由「净化后名扫描」统一兜住（装配 fail-fast 先行）。
- per-mount 模式决议：本期 v1 全局单态（`server.mcp` 是全局单段，modules
  白名单亦全局——预存缺口）；实现把 mode 决议收成单点函数 `f(config, mount,
  user)`，per-credential/per-mount 动态化未来只动它，不动管线。

## 改面清单（评审后修订）

- `src/config.rs`：McpCfg.expose（三值枚举）
- `oj/src/args.rs`：McpArgs --expose
- `oj/src/app.rs`：装配 expose 进 McpConfig
- `serve/src/lib.rs`：McpConfig.expose + 净化后名 `oj_` 扫描（所有 mount）
- `serve/src/mcp_http.rs`：groups 构建、_meta 渲染、routed list、search meta
  handler、call_tool 的 `oj_` 短路（tools_for 之前）+ user 穿透
- `serve/src/mcp.rs`：元工具定义/annotations（如需）
- `oj/src/mcp_cmd.rs`：--expose、__EXPOSE__/__GROUPS_JSON__ 占位、模板
  tools/call 的 oj_ 拦截 + 本地 searchTools、同标准 `oj_` 扫描
- 测试：`serve/src/mcp_http.rs` 单测（search 匹配/排序/limit 钳/group 过滤/
  空命中同形状/空组省略/groups 裁剪、annotations 透传）、`oj/src/mcp_cmd.rs`
  渲染断言（三模式 list 形态/元工具 call 本地闭环）、共享一致性向量 JSON、
  `oj test` L1 用例（flat 零变化回归 + grouped _meta + routed 列表/检索/
  调用闭环）、既有 modules fail-closed e2e 按模式分支（routed 下空业务集仍
  出元工具）
- devkit 四件 + CHANGELOG（版本发布时；`oj_` fail-fast 破坏性明写）

## 留给构建者的裁量

- `oj_search_tools` 排序 tie-break 细节（实现内注释说明即可）。
- JS shim 与 Rust search 的逐字段匹配函数形态（语义被一致性向量钉死）。
- 一致性向量 JSON 的落盘位置（建议 serve 测试 fixtures 旁，codegen 测试引用）。
