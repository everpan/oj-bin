# MCP server + JSON-RPC 一键生成设计（双线路）— v2 评审修订版

日期：2026-10-30（v2：经 builder-check + 架构/工程/产品/AI 应用五路评审修订）
状态：待批准
输入：`oj openapi` 已产出完整 OpenAPI 3.1（operationId / parameters / requestBody / 信封包裹的 responses / tags 模块归属）。

## 意图与动机

oj 的 API 以 openapi.json 为唯一公开契约描述。把它一键变为 MCP tool 面，让
LLM agent（Claude Desktop / Claude Code / 任意 MCP client）直接调用 oj 业务路由；
同时以同一套 JSON-RPC 2.0 机制顺带暴露裸 JSON-RPC 端点（**用户点名要求**），服务
非 MCP 的 RPC 客户端。

两条互补线路，**不冲突、可独立交付**：

- **A. 离线 codegen**（`oj mcp` 子命令）：生成一个**零依赖 Node.js stdio MCP
  server** 单文件脚本。 persona：开发者把本机 agent 指向远端 oj 部署。
- **B. serve 内置端点**（`{base}/mcp` + `{base}/rpc`）：oj 进程内 streamable
  HTTP + 裸 JSON-RPC 2.0，进程内分发。 persona：部署owner向团队共享 agent /
  server-to-server 暴露。

**选型表**（写给用户）：

| 场景 | 选 |
|---|---|
| 本机 Claude Desktop/Code 接远端 oj | A（agent 机器需 node ≥18） |
| 团队共享、多 agent、不愿维护生成物 | B（部署侧开配置门） |
| API 经常变、讨厌重新生成 | B（tool 列表随活路由表） |
| 部署在 agent 够不到的内网 / 需离线拷贝 | A |

## 目标 / 非目标 / 反目标

目标：
- tool 列表与 openapi.json **同源**：共享一条 `RouteTable → spec → tools` 链。
- A：生成的脚本 `node mcp-server.js` 直接可跑（plain JS，node ≥18 全局 fetch，
  无 npm install、无构建步）。**快照语义**：tool 数组在 codegen 期内嵌，重新部署
  oj 后须重新运行 `oj mcp` 并重启 MCP 进程。
- B：`POST {base}/mcp`（MCP streamable HTTP，stateless）+ `POST {base}/rpc`
  （裸 JSON-RPC 2.0，method = operationId）。tool 列表从**运行期活
  `Arc<RouteTable>`** 每次现算——dev 热重载（reload_api 换表）后下一次
  `tools/list` 自然反映新路由，无需任何重新执行。
- 动态模块：见上，B 自愈，A 重新生成。

非目标：
- 不引入 rmcp / tower-sse 等新 crate；MCP 协议子集手写。
- 不做 prompts / resources / sampling，只做 tools。
- 不做 OAuth；鉴权复用既有 Bearer 守卫。
- A 不做进程内分发，只做 HTTP 回源代理。
- **无 `.schema` 声明的路由不保证可被 agent 正确调用**（见反目标 4 的诚实契约）。

反目标（做了就算失败）：
1. tool 列表与真实路由漂移（不同源）。
2. 任何绕过 AuthGuard / 摊平 per-route 鉴权粒度的旁路。
3. 生成物含本机绝对路径或密钥。
4. **隐瞒 tool 可用性与 `.schema` 覆盖度的关系**——未声明契约的端点 tool
   描述必须如实标注，不得让 agent 凭空猜参数。

## v1 架构修订（评审结论，重点）

### R1. `generate` 从 `oj` crate 下移到 `serve`（阻塞项，三方评审一致）

现状：`generate()` + 助手（`operation_id`/`to_oa_path`/`contract_params`/
`path_params`/`envelope_schema`/`pattern_param_names`）及其全部测试住在
`oj/src/openapi_cmd.rs`；依赖箭头 oj → serve，serve 调不到。

修订：整体迁移到 **`serve/src/openapi.rs`**（分层上本就属于 serve：spec 生成是
路由元数据，与 CLI 无关）。`oj openapi` 保留 `discover_routes` + emit/drift-check，
改为薄封装调用 `serve::openapi::generate`。
**硬约束：迁移必须保持 openapi.json 字节级一致**，否则 `oj openapi --check` 的
CI 漂移门禁破裂（迁移本身机械：只依赖 `RouteTable` + `only_js::contract`，均在
serve 依赖树内）。`spec_to_tools` 随之落在 `serve/src/mcp.rs`，紧挨 `generate`。

### R2. 分发不接 `run_route`，抽取共享 dispatch 助手（阻塞项）

`run_route`（serve/src/lib.rs:745）是 axum 缝合层：吃 `Body`+`HeaderMap`，
吐出已序列化的 axum `Response`——MCP handler 套它要做序列化→解析回环，还把
transport 状态码与业务信封搅在一起。且直接抄 `dispatch_meta_handler` 先例会踩
`validate=false` 的坑（那是框架内部合成请求；MCP 是外部输入，**必须
validate=true**，否则 `.schema` 契约校验被静默关闭）。

修订：从 `run_route` 抽共享助手
`dispatch(st, headers, verb, path_no_base, query, body) -> Capture`（admit +
RequestInfo 合成 + per-route timeout 选择（`pipeline.route_timeouts` 按完整路径，
不能丢）+ `run_module(validate=true)`）。MCP tools/call 与 `/rpc` 共用。
**admit 传 tool 自身的 `path_no_base`**（不是字面 `/mcp`）——per-route
`anonymous_paths` 粒度原样保留，杜绝「把 `/mcp` 加进 anonymous_paths 就匿名开全
API」的陷阱。

### R3. `tools/call` 结果与错误映射修正（阻塞项，四路评审一致）

- `CallToolResult.content` **必填**：恒返回 `content:[{type:"text",
  text:JSON.stringify(data)}]` + `structuredContent:data`（未声明 outputSchema
  时 structuredContent 原样合法）。
- **业务错误（信封 `code!==0`）→ `{isError:true, content:[{text:msg}]}`**，
  HTTP 200。JSON-RPC error 只留给协议级失败：未知 tool（-32601）、参数不合法
  （-32602）。handler 抛错/超时同理 isError。
- 理由：JSON-RPC error 被客户端当传输故障藏起，模型读不到、无法自我纠正。

### R4. tool 名净化 + 显式映射表（阻塞项，三路评审一致）

MCP tool 名限定 `^[a-zA-Z0-9_-]{1,64}$`，而 `operation_id()` 产
`^[a-zA-Z0-9._~-]+$` 且无长度上限。`spec_to_tools` 负责：
- `[^a-zA-Z0-9_-]` → `_`；超 64 字符截断 + 短 hash 后缀；
- **在净化后的名字上重跑唯一性门禁**（碰撞确定性加后缀）；
- 输出侧表：`toolName → {operationId, method, oa_path, inputSchema, ...}`。
  B 的分发与 A 的内嵌路由表都按这张表走，**永不从 operationId 反推**。
- `/rpc` 的 method 仍用原始 operationId（不受 MCP 命名约束）。

### R5. 协议合规清单（streamable HTTP，2025-06-18）

核验结论：stateless 省略 `Mcp-Session-Id` **合法**（缺席即声明）；纯 JSON 响应
**合法**（client 给了两种 Accept，选 JSON 合规）；stdio 用换行分隔 JSON **正确**
（Content-Length 是 LSP，不是 MCP）。但需补齐：
- `initialize` 返回必须含 `protocolVersion`（目标 2025-06-18，支持集至少
  2024-11-05 / 2025-03-26 / 2025-06-18；协商 = client 版本在支持集则回显，否则回
  己方最新）+ `capabilities:{tools:{}}` + `serverInfo`（version 与 `oj --version`
  一致）。**无 capabilities.tools 声明，严格客户端不会发 tools/list。**
- 通知（任何无 `id` 的消息）**不回 JSON-RPC 响应**：HTTP 侧回 **202 空 body**；
  stdio 侧不回写。结构性规则，不止 `notifications/initialized`。
- **拒绝 JSON-RPC batch 数组**（-32600）：MCP 2025-06-18 已移除 batch，两侧一致拒绝。
- `GET/DELETE {base}/mcp` → 405（axum `post()` 路由天然给）；响应
  `Content-Type: application/json` 必须显式设置。
- 校验请求 `Accept` 含 `application/json` 或 `text/event-stream`，否则 406（便宜）。
- `/rpc` 同样拒绝 batch（它是自有协议，v1 明确不批处理）。

### R6. 取舍 6 修订：stream/SSE 端点不挂起、不裸奔

原取舍「stream/sse 端点照常进 tool 列表、调用侧分流」被三方评审击穿：
SSE 流不终止，B 缓冲响应体、A 的 `fetch().text()` 都**永久挂起**；且 spec 无
静态标记可预先识别。修订：
- **json.raw**：保留（有限 body），命中不了信封时原样 text 返回。
- **json.stream / json.sse**：**调用期检测**——B 侧 `cap.stream.is_some()` →
  返回 isError「streaming endpoint, not supported via MCP」；A 侧响应
  `Content-Type: text/event-stream` 或读取超时 → 同文案 isError。不挂起、不加
  新声明面。shortcut: 调用期报错而非预先排除，若将来 `.schema` 增加流标记可
  升级为生成期排除。

### R7. 鉴权防线加固

- 配置门打开但**未装配 auth 插件**（`Pipeline.auth: None`）→ 启动期 **fail-fast**，
  除非显式 `server.mcp.allow_anonymous: true`。否则 `/mcp` 匿名暴露全路由的
  机器可读目录 + 调用面，比业务路由自身的暴露面更糟。
- `/mcp`、`/rpc` 加入 reserved-path 集合与 `{base}/plugins` 同款的启动 WARN
  机制（业务模块命名 mcp/api.ts 被遮蔽要可见）。

### R8. 配置面（两个独立门）

```yaml
server:
  mcp:
    enabled: false        # 缺省关；挂 {base}/mcp
    modules: []           # 可选：模块/tag 白名单，过滤 RouteTable 后再 generate
    allow_anonymous: false
  rpc:
    enabled: false        # 独立门；挂 {base}/rpc（用户点名要 json-rpc，故保留，
                          # 但与 mcp 解耦，只开 mcp 不强行带 rpc）
```
mcp disabled 且路由表非空时，启动日志打一行提示（`N routes could be exposed
via MCP: set server.mcp.enabled=true`）——默认关是对的，但要给用户发现 affordance。

## 共享核心：`serve/src/mcp.rs`

纯函数链，无 I/O：
1. `serve::openapi::generate(table, base)`（自 oj 迁入，字节级不变）→ spec Value。
2. `spec_to_tools(spec) -> Vec<Tool>`，每个 tool：
   - `name`：R4 净化后的名字（唯一性门禁在净化后重跑）；
   - `description`：summary；**未声明契约的端点追加 "（no declared input
     contract — inspect source）"**（反目标 4 的落地）；stream 端点（若可检测）
     追加提示；
   - `inputSchema`：path/query parameters → properties（path 恒 required）+
     requestBody → `body` 键（required 随契约）；**含 `$ref` 直接 fail-fast**
     （oj 生成物目前全内联，契约里出现 $ref 说明用户写了工具解析不了的东西，
     宁可报错不可静默出残 schema）；
   - `annotations`：method 映射——GET → `readOnlyHint:true`，DELETE →
     `destructiveHint:true`（约 5 行，实质改善 agent 行为与授权 UX）；
   - 私有映射项 `{method, oa_path}` 供分发（不进 MCP  payload 的部分由
     调用方剥离）。
3. tools/list 侧与 tools/call 侧共用同一次 generate + spec_to_tools 产物，
   B 每次请求现算（表在内存，代价可忽略，消灭失效问题）。

**参数约定（两侧逐字实现同一规则，防 Rust/JS 双实现漂移）**：
- path 参数：单段值 `encodeURIComponent`；catch-all `{path}` 值**不编码**
  （其中的 `/` 是真实分隔符）；值含 `/` 的单段参数将（正确地）不匹配。
- query 值：一律字符串化（`RequestInfo.query` 是 `HashMap<String,String>`，
  数组/多值 query 塌缩——文档明示）。
- 信封识别：body 解析为对象且含 `code`(number)/`msg`/`data` 三键才算信封；
- 业务 code 非 0 或 HTTP 非 2xx **都**可能携带错误信封——**解析不 gate 在
  HTTP status 上**（A 的 fetch 不抛错，但解析逻辑不得看 `res.ok`）。

## 线路 A：`oj mcp`

```
oj mcp -d dist -o mcp/                # release
oj mcp -d src --base /v1/api -o mcp/  # dev（V8 内省）
```
- 输入只有 RouteTable 一条路径（与 `oj openapi` 同构）；**不接 openapi.json 文件
  输入**——那是第二条解析/校验路径，无人要求。
- 生成物 `mcp/mcp-server.js`：NDJSON over stdio；方法集 `initialize` / `ping` /
  `tools/list`（内嵌 tool 数组原样返回）/ `tools/call`；通知不回写。
- 初始化返回含 protocolVersion 协商 + capabilities + serverInfo + 内嵌 spec
  hash（重部署后可人工比对漂移）。
- `tools/call`：内嵌 `toolName → {method, path模板}` 表 → 按参数约定填 path、
  拼 query → `fetch(OJ_BASE, method, JSON body, Authorization: Bearer OJ_TOKEN)`。
- 模板纪律：**stdout 纯净**——一切诊断走 stderr，一行 stray log 就杀死握手；
  编码按参数约定；解析不 gate 在 res.ok。
- 配置全走环境变量（`OJ_BASE` / `OJ_TOKEN`），生成物零硬编码密钥；Claude
  Desktop 把 env token 存明文配置，文档提一句。

## 线路 B：serve 内置

- 挂载仿 `{p}/health` / `{p}/plugins`（serve/src/lib.rs:233 固定路由区），
  配置门按 R8。`/mcp` handler 每请求读**当前** `Arc<RouteTable>`（与
  `dispatch_meta_handler` 同款取表方式；AppState 需补挂表句柄，reload 换表
  自然生效——N2 要求钉死）。无 `reload_api` 的 dev 裸新增目录路由不进表，
  该限制写进文档。
- `POST {base}/mcp`：R5 协议清单 + R3 结果映射 + R2 dispatch + R6 流检测。
- `POST {base}/rpc`：裸 JSON-RPC 2.0，`method = 原始 operationId`，同一
  dispatch；错误映射：信封 code!=0 → JSON-RPC error（裸 RPC 无 isError 概念，
  这里 error 对象是正确载体）。

## R9. 安全加固（安全评审 v3 增补）

- **B1（阻塞）endpoint 级 admit**：`/mcp` 与 `/rpc` 的**每一个请求**先经
  `admit(st, headers, verb, Some("/mcp"))`（字面端点路径）——拒绝 → 401 响应。
  tools/call 的 per-tool admit 在其内层叠加（R2）。分层语义：把 `/mcp` 加进
  `anonymous_paths` 只白名单目录/initialize 腿，每个 tool 调用仍被独立准入。
  若无此层，装没装 auth 插件匿名者都能枚举全路由目录（R7  rationale 自我落空）。
- **Content-Type 门禁**：请求体必须 `application/json`，否则 415/406——Accept
  检查单独扛不住浏览器 form-CSRF 的全部变体（admit 会转发 cookie 给守卫，
  cookie 会话是该路径上的环境凭据）。
- **body 上限**：/mcp、/rpc 读体同样受 `pipeline.max_upload` 约束。
- **modules 白名单 fail-closed**：配置的 tag 匹配不到任何模块 → 空 tool 列表
  （不得回落全集）；配置白名单时无 tag 的路由排除。打错一个字母不得静默扩面。
- **线路 A 租户头**：codegen 读 `-c config.yaml`，把配置的租户头**名**（非密钥）
  内嵌进脚本，值取 `OJ_TENANT` env；header-only 租户模式不内嵌则全线 400。
- **A 启动警告**：`OJ_BASE` 为非 localhost 的 `http://` 时 stderr 警告一行
  （Bearer 明文过线）。
- **sample 纪律**：`allow_anonymous: true` 与关鉴权的 config 不得出现在
  sample/ 及任何入库 config。
- 采纳的便宜项：B 侧实现 `ping`（严格客户端健康检查会发）；spec 按表身份缓存
  （reload 换表自然失效）；A 模板 `redirect:"manual"`；JSON 解析失败 →
  `-32700`（id:null）。
- 维持 deferred 的裁定：结果大小护栏（HTTP 直连本就能拉同样的字节，无新增
  DoS 面）、Origin/DNS-rebinding 检查（S1 Content-Type 门 + 406 后门后无浏览器
  CSRF 可达；cookie 会话部署需知悉残余风险）、A 自动漂移检测。

## 对外接入与分发（其他 agent / 工具导入）


**B = URL 型**（对外首选，分发成本为零）：开启 `server.mcp.enabled` 后，任意
MCP client 按 `POST {base}/mcp` 接入，Bearer header 带 token——

```bash
# Claude Code
claude mcp add --transport http oj https://host/v1/api/mcp \
  --header "Authorization: Bearer $OJ_TOKEN"
```
```jsonc
// Claude Desktop 等 client 的 mcpServers 段
{"oj": {"url": "https://host/v1/api/mcp",
        "headers": {"Authorization": "Bearer ..."}}}
```

LangChain / LlamaIndex / OpenAI Agents SDK 的 MCP adapter（stdio 与 streamable
HTTP 均可消费）、Cursor / Windsurf / Cline / Continue、以及 n8n / Dify 的 HTTP
节点，同 URL 接入。非 MCP 调用方走 `{base}/rpc`（裸 JSON-RPC 2.0）。

**A = stdio 文件型**（本机 agent / 网络隔离环境）：

```jsonc
{"oj": {"command": "node", "args": ["/abs/mcp/mcp-server.js"],
        "env": {"OJ_BASE": "https://host", "OJ_TOKEN": "..."}}}
```

分发三选一：拷贝单文件 / `oj mcp -o dist/mcp/` 与 release 产物同行随 `.tgz`
发布 / 包 npm bin 壳 `npx` 跑（脚本零依赖，npm 只是分发壳）。

**兼容性备注**：部分 client（Claude Desktop 早版）对远端 URL 型 server 鉴权
支持参差（偏向 OAuth）；Bearer header 型 remote server 在 Claude Code 上一等
公民。stdio 形态兼容度最高——这是双线路并存的价值，不是我们侧可修的。

devkit 场景文档须提供「三步接入」完整可复制示例（含上述 client 配置片段）。

## 关键取舍汇总（含评审后修订）

1. 手写 MCP 子集，不加 rmcp（不变，评审认可）。
2. B 的 tools/list 不缓存（不变）。
3. A 生成物 plain JS、快照语义（修订：放弃「启动时读 spec」说法，评审指出其
   与内嵌矛盾且引入第二条解析路径）。
4. stateless transport（不变；评审核实合法，补齐 R5 清单）。
5. 配置门缺省关 + 无 auth fail-fast + 启动提示日志（加固）。
6. ~~stream/SSE 端点照常进列表、调用侧分流~~ → **R6：调用期检测报错**。
7. 新增：`generate` 下移 serve（R1）；tool 名净化（R4）；业务错误 isError（R3）；
   `$ref` fail-fast；annotations；模块白名单可选。

## 测试

- 纯函数：`spec_to_tools`（拼装/必填/body/`$ref` 拒绝/净化+去重/annotations/
  未契约标注）；generate 迁移前后**字节级一致**（快照对拍）。
- 协议组帧：通知 202 空 body；batch 拒绝 -32600；Accept 406；GET 405；
  initialize 协商（client 2024-11-05 → 回显）。
- B 的 e2e（oj/tests/e2e.rs）：initialize → tools/list 含已知 tool →
  tools/call 命中 sample 路由返回 content+structuredContent；`code!=0` →
  isError:true 且 msg 可读；未开配置门 404；**无 auth 插件 + enabled →
  启动失败**；带 anonymous_paths 的 admit 粒度用例；契约违反参数经 MCP →
  400（钉死 validate=true）；catch-all path 填充；未知 tool → -32601；`/rpc`
  同 dispatch parity。
- A：env-gated（`OJ_E2E_NODE`，诚实登记）生成 + 起 node 进程跑
  initialize/tools/list + tools/call 回源。
- 版本门禁：CHANGELOG.md + devkit 四件套 + `cargo xtask build` + devkit 契约
  测试；场景文档给「三步接入 Claude Code」完整可复制示例（client JSON 片段）。

## 交付顺序

1. R1：`generate` 迁移 serve（含字节级对拍测试）——一切的前置。
2. 共享 `spec_to_tools` + 单测。
3. B（serve 端点 + e2e）。
4. A（`oj mcp` codegen + env-gated smoke）。

## 风险与留给实现者

- R1 迁移动 `oj openapi --check` 的已测表面：字节级不变是硬约束，CI 漂移门禁
  是验收标准。
- AppState 补挂 `Arc<RouteTable>` 的具体字段与 reload 换表的交互（照
  `dispatch_meta_handler` 取表方式）。
- `dispatch` 助手从 `run_route` 抽取时的签名取舍（route_timeouts 查找逻辑原样
  搬运）。
- `server.mcp.*` / `server.rpc.*` 的 YAML 解析与 AppConfig 字段（照
  `server.app_path` 形态）。
- MCP `initialize` serverInfo.name 取 "oj"。
- 评审记录（原始五路意见）见会话；本文档已合并全部 blocker 与 should-fix，
  nice-to-have 中采纳 annotations / 模块白名单 / 启动提示，未采纳项：结果大小
  护栏（记为后续）、Origin 校验（记为后续）、A 的启动期漂移自动检测（spec hash
  已内嵌可人工比对，自动检测记为后续）。
