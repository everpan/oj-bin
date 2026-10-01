# PR-2 / PR-5 详细实施方案（v1 · 专家评审后修订）

> 状态：v1 —— 已吸收架构师 / 工程师 / 产品三方评审意见。
> v0 评审共识：**单次 `ABI 9→10` 是强制结果（无温和过渡）**；两 PR 各拆为「Phase A 无 ABI 速赢 + Phase B 并入 ABI 10」；跨 FFI 用**批量 pull + 信封**；取消语义 core 仅 best-effort、真取消 plugin-only。
> 代码锚点已在 v0 核对，本版仅据评审调整设计。

---

## 0. ABI 与分期策略（评审定稿）

- `ABI_VERSION = 9` 严格相等门禁（`plugin_loader.rs:385`）→ 任一 vtable 加字段，**三个插件必须同步 republish**，否则启动 fail-fast。**没有中间态**，故合并为单次 `9→10` 不是「选项」而是强制。
- 但**不代表现在就 bump**：每个 PR 拆两阶段，Phase A 不碰 vtable、不 bump ABI，立刻可交付；Phase B（vtable 扩展）并入同一次 ABI 10 发布。
  - **PR-2 Phase A**：`DataAccessor` trait + `SqlxAccessor`（core/sqlite/dev/test）流式，**不进 vtable**，对插件后端显式报 "backend does not support streaming"。
  - **PR-2 Phase B**：`DataAccessorVtable` 加 `stream_open/next/cancel` + `oj-db-mysql`/`oj-db-postgres` 实现 → ABI 10。
  - **PR-5 Phase A**：仅靠 **已有** `blob.uploadUrl` 客户端直传（ABI 9、零 vtable 变更）做"大文件上传"速赢（见场景 13）。
  - **PR-5 Phase B**：`BlobBackendVtable` 加 `put_stream_*` + `oj-blob-s3`/`LocalBlob` 实现 → ABI 10。
- **单一 ABI 10 发布**同时承载 PR-2 Phase B 与 PR-5 Phase B 两族槽位（避免两次破坏性发布）。插件 `oj-db-mysql`/`oj-db-postgres`/`oj-blob-s3` 锁步发 ABI 10。

---

## 1. PR-2：DB 流式查询 + 查询取消

### 1.1 目标
- `db.stream(sql, params?, opts?)` 逐批吐出结果行（不再 `fetch_all` 全量进内存），配合 v0.1.35 `json.stream` 写回客户端（大 CSV 导出 / 游标回传）。
- 取消：`db.stream(sql, params, { signal })` 接 `AbortSignal`（deno_web 已加载，`bootstrap.js:131`），abort 取消底层查询。

### 1.2 Phase A（无 ABI · 速赢）
- `DataAccessor` trait 新增 `async fn stream_query(&self, sql, params) -> BridgeResult<impl Stream<Item=Row>>`；`SqlxAccessor` 用 `sqlx::query(sql).fetch(pool)`（事务内 `tx.fetch`）。
- `InMemoryAccessor` 也实现（内存切片转流），保证测试与 fake 可用。
- `op_db_stream(name, sql, params, signal?)`：先同步跑 `guard::check_raw` / `check_tenant_raw`（**open 前必须校验**）→ 打开流 → spawn **脱离式 pump 任务**（`current_thread` 运行时内）循环拉批 → 经 `ReqState.stream_tx: UnboundedSender<Bytes>`（PR-1 通道）写回 → 结束 `end()`。op 立即返回，信号 `Arc<AbortSignal>` 移入 pump 任务。
- 插件后端（mysql/pg 经 vtable）：Phase A 不支持 → `op_db_stream` 显式 `json.fail("backend does not support streaming (ABI 10 required)")`。

### 1.3 Phase B（vtable · ABI 10）
- `DataAccessorVtable` 末尾追加（strict 版本号仍要求 10）：
  - `stream_open(handle, sql, params) -> FfiFuture<{stream_id:u64}>`（不支持的 backend 返回哨兵 `{"unsupported":true}` → host 回落 `fetch_all` 有界或报错）。
  - `stream_next(stream_id) -> FfiFuture<envelope>`：**批量**返回 ≤N 行 JSON 数组。
  - `stream_cancel(stream_id) -> FfiFuture`（方言级真取消）。
  - `stream_close(stream_id) -> FfiFuture`（显式 reclaim 游标；完成/出错都须调用，勿用 cancel 兼任清理）。
- 插件用 `Pin<Box<dyn Stream+Send>>` 以 `stream_id` 为键维护游标，按 `stream_next` 重查；`close/cancel` 释放 HashMap 条目（否则每 aborted 查询泄漏一项）。

### 1.4 跨 FFI 行传输契约（工程师定稿）
- **信封**，绝不裸 `null`（单列 `null` 行会与"结束"冲突）：`{"rows":[...]}` / `{"done":true}` / `{"error":"..."}`。
- **批量 pull**：每次 `stream_next` 返回 ≤100 行数组，砍掉逐行 N 次 FFI 往返的吞吐成本（计划 R2）。
- core（sqlite/Any）走进程内 `impl Stream`，无 vtable；仅 mysql/pg 插件走 pull 模型——两实现分别写，不混为一谈。

### 1.5 JS API 形态（产品定稿：callback 为主）
- **主形态（95% 场景）callback**，贴合 handler 已会的 `json.stream` 心智：
  ```ts
  export default {
    get() {
      const s = json.stream({ contentType: "text/csv" });
      db.stream("select * from big_table", [],
        { onRow: (row) => s.write(csv(row)), signal: aborter.signal });
      // handler 返回即框架接管泵；onRow 内可做逐行富化/变换
    },
  };
  ```
- **次形态（逃生口）** 异步迭代器：`for await (const row of db.stream(sql, params))` 仅用于消费侧特殊逻辑。
- 取消：core（`Any` 驱动）取消 = **best-effort drop**（文档明示"非真取消"）；真取消仅 plugin-only（`stream_cancel` 槽，PG CancelToken / MySQL `KILL QUERY`）。**不粉饰 drop = cancel**。

### 1.6 错误帧
- open 阶段同步错误 → `json.fail`。流中途异步错误 → 经通道写 `{"error":"..."}` 后 `end()`；帧顺序与 `stream_close` 时序在计划文档/契约中明确。

### 1.7 测试（必含回归）
- core sqlite：大表 `db.stream` 行集 == `db.query` 全量（顺序一致）。
- 取消：中途 abort → 不 panic、连接回池（sqlite `max_connections(1)` 泄漏即死锁，作哨兵）。
- `AbortSignal` 走**真实 op2 参数路径**（非手动 Notify）。
- 插件（env-gated PG/MySQL）：cancel 真杀服务端查询（无孤儿 `pg_stat_activity`）。
- **已知 `RefCell already borrowed` 回归**：stream-next 与另一 op 交错的泵路径不得持 OpState 借用跨 await。
- ABI 门禁：`cargo xtask plugin <name> --check` 对 ABI 9 旧插件失败、ABI 10 通过。

---

## 2. PR-5：流式 multipart / 更大上传

### 2.1 目标
- 上传请求体不再整体 `to_bytes` 缓冲；文件字段直接流式写入 blob，支持远超 `max_upload_bytes` 的单个大文件，服务端内存恒定。

### 2.2 Phase A（无 ABI · 速赢）
- **仅靠已有 `blob.uploadUrl` 客户端直传**（`BlobBackendVtable.upload_url`，ABI 9 已存在）：s3 返回预签名 PUT/multipart URL，前端直传，服务端零缓冲、零 vtable 变更。
- 文档/场景 13 强化：>+1 GiB 单文件走直传；local 后端无预签名 → 走 Phase B 直传路由。
- 这一步本周可发，**零 handler 破坏、零 ABI**。

### 2.3 Phase B（vtable · ABI 10）
- `BlobBackendVtable` 末尾追加：
  - `put_stream_open(handle, key, content_type) -> FfiFuture<{upload_id:u64}>`
  - `put_stream_chunk(handle, upload_id, bytes: RBytes) -> FfiFuture`
  - `put_stream_finish(handle, upload_id) -> FfiFuture`
  - `put_stream_abort(handle, upload_id) -> FfiFuture`（**必加**：失败清理，否则 s3 orphan parts 烧钱 / local 临时文件泄漏）
- `LocalBlob`：open=建临时文件、chunk=append、finish=commit + ct sidecar；临时文件 `Drop` 守卫（finish 未跑即删）。
- `S3Blob`（插件）：复用 `upload_url` 的 `multipart_initiate/part/complete`，每 chunk 对应一个 part；任意 chunk/finish 失败 → `put_stream_abort` 取消 multipart。
- **同步扩展进程内 `BlobBackend` trait（`bridge/blob.rs:29`）+ `LocalBlob` + `FfiBlobBackend` 适配器**（不仅 vtable）。

### 2.4 Server 侧（`server/src/lib.rs`）
- `body` 作为 `Stream`（`axum::body::Body` 即 `Stream<Bytes>`）：`multer::Multipart::new(body.into_data_stream(), boundary)`。
- 文本字段 → 累积 `fields`（**缓冲至首个文件字段出现**，保留"先文本后文件"语义；或文档约定文本字段前置）。
- 文件字段 → `field` 是 `Stream<Bytes>`，**直接** `put_stream_open/finish` 流式写，不再 `f.bytes().await` 攒 `Vec<u8>`。
- **大小闸**：multer `Constraints` 为**总**上限；**每字段**上限用 `field.set_size_limit(...)`（非一次调用表达"总+每字段"）。仍受 `blob_upload_max`（blob 直传腿）/ `max_upload*2`（handler 腿）约束，保留 DoS 防护。
- blob 直传 `PUT` 路由（`is_blob_put`，line 430）：由 `to_bytes` 整段改 `put_stream` 直接落盘 / multipart，解除内存上限。

### 2.5 JS API 形态（产品定稿：additive，不破坏）
- **保留** `http.file(i)`（`op_http_file` 全量 `Vec<u8>`，小文件仍可用）+ `http.files[i]` 元信息。
- **新增** `http.files[i].key` / `.url` 作为零拷贝大文件路径（handler 用 `blob.get(key)` / `blob.url(key)` 取）。
- **不移除 `bytes`/`http.file`**（原计划的 `f.read()` 兼容壳被否——"从已消费的流读"语义混乱）。大文件场景文档引导用 `key`/`.url`。
- `UploadedFile`：`{ field, filename, content_type, bytes? , key?, url? }` —— `bytes` 仅小文件回填，大文件走 `key`/`url`。

### 2.6 测试（必含回归）
- 200MB 上传：`max_upload_bytes` 小、`blob_upload_max` 大 → 成功；内存峰值恒定（对比整体缓冲基线）。
- local：落盘正确、ct sidecar 正确、chunk 失败临时文件清理。
- s3（env-gated）：multipart 直传；失败 abort 无 orphan parts。
- 大小超限 → `413` 仍触发；每字段超限也触发。
- 文本字段在文件字段前到达的语义不变。

---

## 3. 共同发布（ABI 10 单次）
- 版本号 bump（`oj/Cargo.toml` → 0.1.37？当前 HEAD 0.1.36 内部修复）；CHANGELOG 分段；devkit 四件 + 错误/限制表同步；`cargo xtask build` + `cargo test --release -p xtask` 契约校验。
- 插件锁步：`oj-db-mysql`/`oj-db-postgres`/`oj-blob-s3` 同发 ABI 10；旧插件启动 fail-fast（既有门禁）。
- 红线保持：SQL 注入（流式仍走 `db.table()`/参数化，绝不裸拼）、`panic="unwind"`、`bootstrap.js` 7-bit ASCII、失败 runtime 丢弃。
- 新增场景：**场景 23 `db.stream` 大表导出**（callback 主形态）+ 更新场景 13（大文件走 `key`/`.url` 直传）。

---

## 4. 评审采纳与留存分歧

**已采纳（三审一致）**
- 单次 ABI 9→10 强制；两 PR 各拆 Phase A（无 ABI）/ Phase B（ABI 10）。
- 跨 FFI **批量 pull + 信封**（`{"rows"}|{"done"}|{"error"}`），弃用裸 `null` 表结束。
- `db.stream` 主形态 = **callback 接 `json.stream`**，迭代器为逃生口。
- `UploadedFile`/`http.file` **additive**：保留 `bytes`，加 `key`/`.url`，**否掉 `f.read()`**。
- 取消 core best-effort drop、真取消 plugin-only（文档明示缺口）。
- 加 `put_stream_abort` + local 临时文件 `Drop` + s3 abort；multer 每字段 `set_size_limit`；进程内 `BlobBackend` trait 同步扩展；`AbortSignal` 已是全局无需新 op。

**分歧与处置**
- 产品担忧"为省一次 bump 提前合并 PR-5" → 处置：Phase A 各自独立速赢，**无提前合并压力**；ABI 10 由 PR-2 Phase B 触发，PR-5 Phase B 仅在"handler 内需就地处理 100MB+ 文件"被证实为真实需求时并入同次 bump（否则 PR-5 仅靠 Phase A 直传即可）。
- 待产品最终确认：in-JS 处理超大上传是否真实需求（决定 PR-5 Phase B 是否立项）。
- 待架构确认：`stream_open` 的 `{"unsupported":true}` 哨兵 → host 回落 `fetch_all`（有界）还是直接报错（推荐：回落 fetch_all 以保 dev/test 一致）。
