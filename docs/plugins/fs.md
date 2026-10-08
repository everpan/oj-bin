# fs —— 本地文件系统（核心内置，v0.1.53）

> 本文档归入 docs/plugins/ 仅为索引方便；fs **不是 cdylib 插件**，而是核心运行时
> 内建轴（与 `json`/`db`/`kv` 同类），不占用 AXES 插件槽位、不涉及 ABI_VERSION。

## 是什么

为 JS/TS handler 提供一次性（one-shot）本地文件读写能力，移植自 Deno 官方
`deno_fs` 扩展（版本锁对齐 deno_core 0.411）。handler 侧暴露 `fs` 全局对象。

与 `blob` 的分工：`blob` 走对象存储（S3 等插件后端），适合大文件与共享存储；
`fs` 走**服务进程所在机器的本地磁盘**，适合配置文件、模板、落盘导出等小对象。

## 配置

```yaml
fs:
  root: ./data     # jail 根目录；相对路径按 config.yaml 所在目录解析（缺省 "data"）
  readonly: false  # true = 写类 API 全部拒绝（read 轴仍放行）
```

- **段缺省 = 不启用**：`fs.*` 调用抛 `NotCapable`（fail-closed，不静默降级）。
- 装配期 canonicalize `root`；不存在或非目录 → **启动 fail-fast**。
- `readonly: true` 用于「只需要读模板/资源」的场景，写路径双保险。

## API（9 个，全部 async）

| 函数 | 签名 | 说明 |
|---|---|---|
| `readFile` | `(path) => Promise<Uint8Array>` | 读二进制 |
| `readTextFile` | `(path) => Promise<string>` | 读 UTF-8 文本 |
| `writeFile` | `(path, data: Uint8Array) => Promise<void>` | 写二进制（覆盖） |
| `writeTextFile` | `(path, data: string) => Promise<void>` | 写文本（覆盖） |
| `mkdir` | `(path, opts?) => Promise<void>` | 建目录（`{recursive: true}` 可嵌套） |
| `remove` | `(path, opts?) => Promise<void>` | 删除（`{recursive: true}` 删目录树） |
| `rename` | `(oldPath, newPath) => Promise<void>` | 移动/重命名 |
| `stat` | `(path) => Promise<FileInfo>` | 元数据（size/mtime/isFile/isDirectory…） |
| `readDir` | `(path) => Promise<AsyncIterable<DirEntry>>` | 列目录，迭代项 `{name, isFile, isDirectory, isSymlink}` |

路径规则：**相对路径解析到 jail 根之下**（不是进程 cwd）；绝对路径必须在 root 内。

## 安全模型（jail）

1. 门面层 `op_fs_resolve`：对目标路径做 best-effort canonicalize（写新文件时逐级
   上溯到最近存在的祖先），落在 root 外 → `NotCapable`。
2. deno_permissions 容器：read/write 两轴收窄到 root（`..`、root 外绝对路径、
   root 内指向外部的 symlink 一律拒绝）；net/env/sys 等轴不受影响（fetch/WS
   出站行为不变）。

已知边界：检查与使用之间存在 TOCTOU 窗口（symbolic link 交换），对
「防误触/默认拒绝越界」的威胁模型可接受；恶意 handler 本就有其他外发通道。

## 错误

| 场景 | JS 错误类 | 消息关键词 |
|---|---|---|
| 未配置 `fs:` 段 | `NotCapable` | `fs not configured` |
| 越出 root（含 symlink 逃逸） | `NotCapable` | `path escapes fs.root` |
| readonly 下调用写 API | `NotCapable` | `Requires write access` |
| 路径不存在 | `NotFound` | `No such file or directory` |

## 限制

- **无 fd/流式 API**：不暴露 `open/read/write/close`——op 内自开自关，池化
  runtime 无跨请求 fd 泄漏面。代价是大文件整体进内存；GB 级场景请走 `blob`。
- `readDir` 返回异步迭代器（不是数组），用完自动关闭目录句柄。
- 一次性 API 均为 Promise；handler 可直接 `await`（异步测试用 async IIFE 包裹）。

## 案例

### 导出 JSON 落盘 + 读回

```ts
// src/report/export/api.ts
async function post() {
  const rows = db.table("account").select(["id", "name"]).limit(100).all();
  const payload = JSON.stringify({ at: Date.now(), rows });
  await fs.mkdir("exports", { recursive: true });
  await fs.writeTextFile(`exports/accounts-${Date.now()}.json`, payload);
  json.ok({ bytes: payload.length });
}
export default { post };
```

### multipart 上传 → 落盘 → 任务异步解析

```ts
// src/upload/api.ts —— 小文件直接落盘（大文件请走 blob 上传）
async function post() {
  const f = http.files[0];                 // 第一个上传文件（按字段过滤用 m.field）
  if (!f) { json.fail(400, "need a file (multipart)"); return; }
  await fs.writeFile(`inbox/${f.filename}`, await http.file(0));
  json.ok({ saved: f.filename, size: f.size });
}
export default { post };
```

### multipart 多文件批量上传落盘

```ts
// src/upload/batch/api.ts —— 一次请求带多个文件（同名字段重复或多个字段名均可）
async function post() {
  if (!http.files.length) { json.fail(400, "need files (multipart)"); return; }
  await fs.mkdir("inbox", { recursive: true });
  const saved = [];
  for (let i = 0; i < http.files.length; i++) {
    const meta = http.files[i];            // {field, filename, content_type, size, key, url}
    const bytes = await http.file(i);      // 第 i 个文件字节
    // 文件名是客户端可控输入：白名单化后再拼路径（jail 也会拦 ../，但 sanitized 名字更友好）
    const safe = meta.filename.replace(/[^\w.-]+/g, "_");
    const key = `inbox/${Date.now()}-${i}-${safe}`;
    await fs.writeFile(key, bytes);
    saved.push({ name: meta.filename, size: bytes.length });
  }
  json.ok({ count: saved.length, saved });
}
export default { post };
```

限制：只适用于 ≤ `server.max_upload_bytes` 的文件——超限字段由服务端流式直落
blob（`http.file(i)` 报错，走 `http.files[i].key` / `.url`，见 api-manual blob 章节）。
多文件总和受 axum 请求体上限约束，批量传大文件请逐文件直传 blob。

```bash
# 同名字段重复（-F 多次）与多字段名混合均可
curl -F "doc=@a.pdf" -F "doc=@b.pdf" -F "img=@c.png" \
  http://localhost:9778/v1/api/upload/batch/
# → {"code":0,"data":{"count":3,"saved":[{"name":"a.pdf","size":…}, …]}}
ls data/inbox/
```

```yaml
# config.yaml
fs:
  root: ./data
  readonly: false
```

```bash
curl -F "doc=@a.pdf" http://localhost:9778/v1/api/upload/
cat sample/data/inbox/a.pdf   # 已落盘到 jail 根下
```

### readonly：只读模板目录

```yaml
fs:
  root: ./templates
  readonly: true
```

```ts
const tpl = await fs.readTextFile("notice.md");      // OK
await fs.writeTextFile("notice.md", "x");            // NotCapable: Requires write access
```
