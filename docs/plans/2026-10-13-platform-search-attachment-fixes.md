# 平台侧修复（支持下游 oj-wiki 搜索 / 附件）

日期：2026-10-13 · 本仓：oj-bin（平台运行时）· 下游消费方：oj-wiki（P3 搜索 + 附件）
说明：oj-wiki 的 P3 实施报告（`task-p3-backend-report.md`）暴露了若干平台层偏差（D-2 / D-3）
与搜索引擎短板。这些**根因在本仓**，须由 oj-bin 修复；oj-wiki 仅消费，不应绕开。
本文是依据本仓代码逻辑复核后的修复设计。

## 上下文与职责划分
- **本仓（oj-bin）负责**：二进制流写出、handler `.schema` 关键字白名单、全文检索引擎样本（`sample/src/_platform/_shared/search.ts`，oj-wiki 仅 vendor）。
- **下游（oj-wiki）负责**：检索分页/可见性、附件上传双路径、引用检查、XSS 转义等**应用层**逻辑（不在本文范围）。
- 修复后 oj-wiki 需在 oj-bin 发版时**重新 vendor** `search.ts`。

## 优先级
- **P0（必修）**：D-2 二进制流写出、D-3 `schema.default` 白名单、search 短词跨方言回退、search FTS5 触发器陷阱根除
- **P1**：search 高亮 N 查询（mysql）、Content-Disposition 文件名支持

---

## 修复 1｜D-2 二进制流写出（P0）

### 根因（已定位）
- `src/bridge/stream.rs:77`：`op_stream_write(state, #[string] chunk: String)` —— 入参被 `#[string]` 收窄为 UTF-8 文本，内部 `Bytes::from(chunk)`，通道本身是 `UnboundedSender<Bytes>`（可承载任意字节）。
- `src/bridge/bootstrap.js:373`：`write: (c) => op_stream_write(String(c))` —— 任何入参被 `String(c)` 强转，二进制（Uint8Array/ArrayBuffer）被序列化成 `"[object ArrayBuffer]"`/`"97,98,…"`（见 oj-wiki 报告 D-2 实测三种失真）。
- 后果：handler 无法流式回源二进制（如附件下载），oj-wiki 只能改走「302 到免鉴权 blob 路由」的 capability URL 绕行，且浏览器另存名为安全化名（无 Content-Disposition）。

### 方案
1. **Rust**：新增字节写入 op（或扩展 `op_stream_write`），接受 `#[buffer]`（DenoTs `ArrayBufferView` / `Uint8Array`）。保留原文本 op 兼容 CSV/SSE 文本行。例如：
   ```rust
   #[op2]
   pub fn op_stream_write_bytes(state: &mut OpState, #[buffer] chunk: JsBuffer) {
       if let Some(tx) = state.borrow_mut::<ReqState>().stream_tx.borrow().clone() {
           let _ = tx.send(Bytes::from(chunk.to_vec()));
       }
   }
   ```
2. **bootstrap.js**：`json.stream(...).write` 按类型分发：
   ```js
   write: (c) => {
     if (c instanceof Uint8Array || c instanceof ArrayBuffer || ArrayBuffer.isView(c))
       return op_stream_write_bytes(c);
     return op_stream_write(String(c));
   }
   ```
3. **文件名（P1）**：`op_json_stream_open` 的 `opts` 增 `fileName?`（→ 响应头 `content-disposition: attachment; filename*=UTF-8''<编码>`），让下游附件下载可直接流式回源且浏览器另存为原名，彻底取代 302 绕行。
4. 同步更新 `stream.rs` 的测试（现有 `op_stream_write` 仅测文本），增二进制块测试。

### 验证
- 单元测试：二进制块经流写出后字节逐字一致（比对 `Bytes`）；非 UTF-8 字节不丢不替换。
- `serve` 集成：oj-wiki 附件下载改为 `json.stream` + `write(bytes)`（下游任务），L1 冒烟比对 302 前后字节一致。
- 契约 `response:"raw"` 的二进制端点不受影响（raw 走 `op_json_raw` 另一条路，需确认 raw 同样支持字节）。

---

## 修复 2｜D-3 `schema.default` 白名单（P0）

### 根因（已定位）
- `src/contract.rs:278` `audit_keywords` 递归审计，凡不在 `SUPPORTED_KEYWORDS`（contract.rs:25-40）的关键字一律 **FAIL FAST**（contract.rs:282-290，明确「静默跳过会让声明成谎话」）。
- `default` 不在该列表 ⇒ 任何 handler 在 `.schema` 里声明 `default` 都会在**装配期**整体加载失败（oj-wiki 因此把 offset/limit 缺省语义移入服务层钳制，契约层 zod 另保留 `.default()` 供生成 client）。

### 方案
- 在 `SUPPORTED_KEYWORDS`（contract.rs:25）加入 `"default"`。`default` 是标准 JSON Schema 的**注解关键字**，无校验语义，白名单即放行——声明不再成谎话，且 ojm 生成的 client 可继续用 zod `.default()` 提供客户端缺省。
- 可选增强（P1，非必须）：在 `InputContract::check` 的 `coerce_scalars` / `check_value` 阶段，对缺省缺失的字段按 `default` 注入，使服务端缺省也在契约层生效（与现有服务层钳制并存，不冲突）。
- **不**引入 `nullable` 之外的其它校验型关键字，保持白名单最小化（防误用）。

### 验证
- 单测（contract.rs 已有 `try_new` 负例）：声明含 `"default"` 的 `.schema` 不再 Err；未知关键字（如 `"format"`）仍 FAIL FAST（回归）。
- oj-wiki 侧：待本仓发版后，`search/api.ts` 的 `.schema` 可去掉手写钳制、改回声明式 `default`（下游任务，本文不覆盖）。

---

## 修复 3｜全文检索引擎（sample/src/_platform/_shared/search.ts）

> 该文件是下游 vendor 的源头；修复后 oj-wiki 重新 vendor。

### 3a 短词跨方言回退（P0）
- **根因**：`search.ts:142-151` 仅 `sqlite` 分支对 `<3` 字符查询做 LIKE 回退（trigram 对短词静默零命中）；`mysql`/`postgres` 分支（170-199）**无**此回退 ⇒ 同一短词在 sqlite 有结果、在 mysql/pg 静默零命中（方言行为不一致，下游 oj-wiki 报告记为此盲区）。
- **方案**：在 `mysql` 与 `postgres` 分支顶部复制同一 `<3` LIKE 回退分支（复用 `likeEscape` + 对基表 `LIKE ... ESCAPE '\'`，score=1）。判定 `const n = [...q].length; if (n>0 && n<3) { ... LIKE ... }` 与 sqlite 同构。
- **验证**：`sample/tests/search.test.ts` 增「<3 字符查询在三方言均非静默零命中」用例（sqlite 已有，mysql/pg 脚本验）。

### 3b FTS5 触发器陷阱根除（P0，防御性）
- **根因**：`search.ts:111-112` sqlite `index()` 用 `INSERT OR REPLACE`。在 `recursive_triggers=OFF`（sqlite 默认）下，`INSERT OR REPLACE` 的隐式 DELETE **不触发** AFTER DELETE 触发器 ⇒ FTS 虚表留旧词（oj-wiki D-5 实测复现；其被迫在调用侧用 `remove()+index()` 绕开）。
- **方案**：sqlite `index()` 改为「先 `remove()`（真 DELETE 必触发 fts_ad）再 `insert`」的内部等价实现，使 `index()` 三方言都安全，**移除 footgun**；`mysql`/`postgres` 维持 upsert（无触发器陷阱）。`remove()`/`query()` 不变。
- **验证**：`search.test.ts` 增「body 'alpha'→'beta' 后搜 'alpha' 必空（旧词不残留）」用例，钉死触发器正确性；保留 oj-wiki 调用侧 remove+index 不变（向后兼容）。

### 3c mysql 高亮 N 查询（P1）
- **根因**：`search.ts:178-185` mysql 高亮路径对**每命中**多一次 `SELECT` 取原文；下游若取大窗（如 1000）即千次查询塌方。
- **方案**：高亮策略与方言解耦——大窗（如 `limit>200`）时 `highlight:false`，由调用侧用本地 `mark()` 补片段（sqlite/pg/mysql 一致，免去 mysql N 查询）；小窗保持引擎高亮。此调整与下游 oj-wiki B1 的「大窗关高亮」协同。

---

## 落地点与文件索引
| 修复 | 文件 | 改动 |
|------|------|------|
| D-2 | `src/bridge/stream.rs` + `src/bridge/bootstrap.js` | 新增字节写入 op + `write` 类型分发 + `fileName` 头（P1） |
| D-3 | `src/contract.rs:25` `SUPPORTED_KEYWORDS` | 加入 `"default"`（+ 可选 check 注入） |
| 3a | `sample/src/_platform/_shared/search.ts:170-199` | mysql/pg 增 `<3` LIKE 回退 |
| 3b | `sample/src/_platform/_shared/search.ts:111-112` | sqlite index() 改 remove+insert |
| 3c | `sample/src/_platform/_shared/search.ts` | 大窗高亮改本地 mark |

## 验证门禁（P0 完成后）
- `cargo test -p oj`（`contract` / `stream` 相关用例全绿）；`cargo build` 通过。
- `sample/tests/search.test.ts` 三方言（sqlite 恒跑；mysql/pg 脚本可达才跑）覆盖 3a/3b。
- `pnpm typecheck` / `ojm api --check`（oj-wiki 侧，待本仓发版重新 vendor 后）0 error。
- L1 冒烟（oj-wiki）：附件下载字节一致；短词检索三方言非静默零命中。
