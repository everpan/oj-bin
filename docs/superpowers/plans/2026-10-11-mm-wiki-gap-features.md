# mm-wiki 三缺口特性（v0.1.61）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec（`docs/superpowers/specs/2026-10-11-mm-wiki-gap-features-design.md`，已双专家评审）的三个特性：HTTP/CAS SSO、引擎原生 FTS（search）、内容版本化（versions），并完成 v0.1.61 发版。

**Architecture:** 三特性均落 **JS 业务模块层**（sample 共享库 + 普通业务路由），宿主仅两处小补缺：`db.dialect()` op 与 `http_sso:` config 段 + `sso` 全局注入。零 ABI、零新插件。

**Tech Stack:** Rust（deno_core ops、serde_yaml config）、TypeScript（sample 模块）、oj test L1（`*.test.ts`）、oj e2e（`oj/tests/*.rs` 独立目标）。

## Global Constraints（每个任务默认遵守）

- **只许 release**：测试/检查一律 `cargo test --release` / `cargo clippy --release --workspace --all-targets -- -D warnings`；禁裸 `cargo test`/`cargo clippy`/debug 构建。
- **`src/bridge/bootstrap.js` 必须 7-bit ASCII**——新增装配行只准 ASCII 字符。
- **`panic = "unwind"` 不许动**；不改任何插件 profile。
- SQL 红线：标识符白名单校验后拼接，值一律绑定参数；**动态标识符绝不来自用户字符串**。
- L1 测试跑法：`cargo xtask bin`（刷新 `bin/oj`）后 `./bin/oj test -c sample/config.yaml -d sample/src`。
- e2e 跑法：`cargo test --release -p oj --test <name>`。
- 提交信息结尾：`unix@vip.qq.com ai`；每个任务至少一个提交，测试绿了才提交。
- 版本分界（Task 9）遵循 `CHANGELOG.md` 顶部五步发版清单。
- sample 的 `dist/` 是构建产物，不要手工编辑。

---

### Task 1: `db.dialect()` op（Phase 0，search 前置）

**Files:**
- Modify: `src/bridge/db.rs`（紧邻 `op_db_next_seq`，约 :708）
- Modify: `src/bridge/mod.rs`（ops 列表 db 组 :354-369，插在 `query::op_db_query_sql` 之后）
- Modify: `src/bridge/bootstrap.js`（db 工厂 :730-752 与 tx 回调对象 :810-824）
- Modify: `sample/global.d.ts`（`DBInstance` :75-110）
- Test: `src/bridge/mod.rs` tests（镜像 `plugins_op.rs::plugins_reports_loaded_fields_and_host_abi`）

**Interfaces:**
- Produces: JS `db.dialect(): "sqlite" | "mysql" | "postgres"`（DBInstance 与 TxInstance 都有）；Rust `db::op_db_dialect`。Task 3 的 search 模块消费。

- [ ] **Step 1: 先读样板**

读 `src/bridge/plugins_op.rs` 的 `plugins_reports_loaded_fields_and_host_abi` 测试全文（Bridge::with_dbs_and_loader + run_with 的确切调用形态）与 `src/bridge/db.rs` 的 `op_db_next_seq`（:708 起）。新测试/新 op 完全按这两个样板机械仿写。

- [ ] **Step 2: 写失败测试**

在 `db.rs` 的 `#[cfg(test)]`（或 mod.rs tests，随样板位置）加：

```rust
/// 装配后 JS `db.dialect()` 报引擎名（sqlite/mysql/postgres；search 共享模块依赖此分派）。
#[tokio::test(flavor = "current_thread")]
async fn db_dialect_reports_engine_name() {
    let b = Bridge::with_dbs_and_loader(
        [("default".into(), "sqlite::memory:".into())].into_iter().collect(),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras::default(),
    );
    // run_with 的调用形态照抄 plugins_op.rs 的既有测试；求值 JS 换成：
    //   await db.dialect()  →  断言 == "sqlite"
}
```

（run_with 的求值入口若不接受裸表达式，则包一层最小 handler 模块，照该测试文件里已有做法。）

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test --release -p only-js db_dialect -- --nocapture`
Expected: FAIL（`db.dialect is not a function` 或编译错误）

- [ ] **Step 4: 实现 op**

`src/bridge/db.rs`（紧邻 op_db_next_seq）：

```rust
/// JS `db.dialect()`：报连接引擎名（"sqlite" | "mysql" | "postgres"）。方言是 accessor
/// 属性、与事务无关，不走 resolve_target（v0.1.61，search/_shared 模块按此分派方言 SQL；
/// 业务 handler 慎用——分支方言 = 放弃跨引擎可移植）。
#[deno_core::op2]
#[string]
pub fn op_db_dialect(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
) -> Result<String, JsErrorBox> {
    let da = super::query::lookup(&state.borrow(), &name)?;
    Ok(match da.dialect() {
        Dialect::MySql => "mysql",
        Dialect::Postgres => "postgres",
        Dialect::Sqlite => "sqlite",
    }
    .into())
}
```

（`query::lookup` / `JsErrorBox` / `Rc<RefCell<OpState>>` 的确切路径与用法照同文件既有 op；若 lookup 是 `&OpState` 则传 `&state.borrow()`。）

- [ ] **Step 5: 注册 + 装配**

1. `mod.rs` ops 列表 db 组末尾（`query::op_db_query_sql` 之后，约 :369）加 `db::op_db_dialect,`。
2. `bootstrap.js` 顶部大 import 块加 `op_db_dialect,`；db 工厂对象（`nextSeq` 行旁，:752）加：
   `dialect: () => op_db_dialect(name),`
3. tx 回调对象（:818 `nextSeq` 行旁）加同一行；同时给 tx 对象加 `isTx: true,`（Task 3 的
   ensureIndex 用它拒绝 DDL 进事务）。**只准 ASCII。**
4. `sample/global.d.ts`：`DBInstance` 接口加 `dialect(): "sqlite" | "mysql" | "postgres";`；
   `type TxInstance = Omit<DBInstance, "tx"> & { isTx?: true };`

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test --release -p only-js db_dialect -- --nocapture`
Expected: PASS

- [ ] **Step 7: 提交**

```bash
git add src/bridge/db.rs src/bridge/mod.rs src/bridge/bootstrap.js sample/global.d.ts
git commit -m "feat(db): JS db.dialect() 报引擎名（sqlite/mysql/postgres）+ tx 对象 isTx 标记

unix@vip.qq.com ai"
```

---

### Task 2: versions——迁移 + 共享模块 + L1（Phase 1）

**Files:**
- Create: `sample/src/_platform/migrations/0002__versions.sql`（通用=sqlite 语法）
- Create: `sample/src/_platform/migrations/0002__versions.mysql.sql`
- Create: `sample/src/_platform/migrations/0002__versions.pg.sql`
- Create: `sample/src/_platform/_shared/versions.ts`
- Test: `sample/tests/versions.test.ts`

**Interfaces:**
- Produces: `createVersions(dbi)` → `{ commit, get, history, diff, rollback }`；`commit(entityType, entityId, content, { author?, message?, expect? }) → { version, sha256 }`；冲突抛 `Error{status:409, message:"version conflict"}`。表 `oj_versions` / `oj_ver_content`（**不进 schema.yaml/manifest.tables**——未登记表裸 SQL 放行，同 `_oj_sequences`；MySQL TEXT 不可作 PK，故走手写迁移）。

- [ ] **Step 1: 写三个迁移文件**

`0002__versions.sql`（无后缀=通用，sqlite 语法）：

```sql
-- 内容版本化原语（v0.1.61）：修订表 + CAS 内容表。mysql/pg 由方言覆盖文件接管。
CREATE TABLE IF NOT EXISTS oj_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_type TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  author TEXT,
  message TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (entity_type, entity_id, version)
);
CREATE INDEX IF NOT EXISTS idx_oj_versions_entity ON oj_versions (entity_type, entity_id, version);
CREATE TABLE IF NOT EXISTS oj_ver_content (
  sha256 TEXT PRIMARY KEY,
  content TEXT NOT NULL
);
```

`0002__versions.mysql.sql`：

```sql
CREATE TABLE IF NOT EXISTS oj_versions (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  entity_type VARCHAR(191) NOT NULL,
  entity_id VARCHAR(191) NOT NULL,
  version BIGINT NOT NULL,
  sha256 CHAR(64) NOT NULL,
  author VARCHAR(191),
  message TEXT,
  created_at BIGINT NOT NULL,
  UNIQUE KEY uq_ver (entity_type, entity_id, version),
  KEY idx_ent (entity_type, entity_id, version)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE IF NOT EXISTS oj_ver_content (
  sha256 CHAR(64) PRIMARY KEY,
  content MEDIUMTEXT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
```

`0002__versions.pg.sql`：

```sql
CREATE TABLE IF NOT EXISTS oj_versions (
  id BIGSERIAL PRIMARY KEY,
  entity_type TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  version BIGINT NOT NULL,
  sha256 CHAR(64) NOT NULL,
  author TEXT,
  message TEXT,
  created_at BIGINT NOT NULL,
  UNIQUE (entity_type, entity_id, version)
);
CREATE INDEX IF NOT EXISTS idx_oj_versions_entity ON oj_versions (entity_type, entity_id, version);
CREATE TABLE IF NOT EXISTS oj_ver_content (
  sha256 CHAR(64) PRIMARY KEY,
  content TEXT NOT NULL
);
```

- [ ] **Step 2: 写失败测试** `sample/tests/versions.test.ts`

```ts
// L1 内容版本化：commit/get/history/diff/rollback/乐观锁/事务一致性。
// 运行：oj test -c sample/config.yaml -d sample/src
import { createVersions } from "../src/_platform/_shared/versions";

const versions = createVersions(db);

describe("versions (CAS + revisions)", () => {
  it("commit/history/get/rollback", async () => {
    for (let i = 1; i <= 5; i++) {
      await versions.commit("doc", "v1", `content v${i}`, { author: "tester", message: `m${i}` });
    }
    const log = await versions.history("doc", "v1");
    expect(log.length).toBe(5);
    expect(await versions.get("doc", "v1", 3)).toBe("content v3");
    expect(await versions.get("doc", "v1", "head")).toBe("content v5");
    const d = await versions.diff("doc", "v1", 4, 5);
    expect(d.some((l) => l.type === "del" && l.text === "content v4")).toBe(true);
    expect(d.some((l) => l.type === "add" && l.text === "content v5")).toBe(true);
    const r = await versions.rollback("doc", "v1", 2, { author: "tester", message: "回滚到 v2" });
    expect(r.version).toBe(6);
    expect(await versions.get("doc", "v1", "head")).toBe("content v2");
  });

  it("expect 乐观锁：撞版本 409 / 同内容去重", async () => {
    await versions.commit("doc", "v2", "A", {});
    const head = (await versions.history("doc", "v2"))[0].version;
    await expect(versions.commit("doc", "v2", "B", { expect: Number(head) - 1 })).rejects.toMatchObject({ status: 409 });
    await versions.commit("doc", "v2", "B", {});          // last-write-wins
    const before = (await db.query("SELECT COUNT(*) AS n FROM oj_ver_content WHERE sha256 = ?", [await crypto.sha256Hex("B")]))[0].n;
    await versions.commit("doc", "v3", "B", {});          // 同内容不同实体 → 不重复落内容
    const after = (await db.query("SELECT COUNT(*) AS n FROM oj_ver_content WHERE sha256 = ?", [await crypto.sha256Hex("B")]))[0].n;
    expect(Number(after)).toBe(Number(before));
  });

  it("db.tx 回滚连带修订回滚（同一事务）", async () => {
    await db.tx(async () => {
      await versions.commit("doc", "tx1", "X", { author: "t" });
      throw new Error("boom");
    }).catch(() => {});
    expect(await versions.history("doc", "tx1")).toHaveLength(0);
  });

  it("diff 大文档：格子预算闸", async () => {
    const big1 = Array.from({ length: 2500 }, (_, i) => `l${i}`).join("\n");
    const big2 = Array.from({ length: 2500 }, (_, i) => `x${i}`).join("\n");
    await versions.commit("doc", "big", big1, {});
    await versions.commit("doc", "big", big2, {});
    await expect(versions.diff("doc", "big", 1, 2)).rejects.toThrow(/diff too large/);
  });
});
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo xtask bin && ./bin/oj test -c sample/config.yaml -d sample/src -t tests`
Expected: FAIL（Cannot find module `../src/_platform/_shared/versions`）

- [ ] **Step 4: 实现** `sample/src/_platform/_shared/versions.ts`

```ts
// 内容版本化原语（v0.1.61，spec §三）：CAS 内容表 + 修订表 + 行级 diff。
// 事务：语句经 op 层 resolve_target 自动路由进调用方在同名库上开启的 db.tx——在事务内
// 调用即同事务（blob 轴不参与事务，故内容存本库 oj_ver_content）。
// 并发：UNIQUE(entity_type, entity_id, version) 兜底；不带 expect = last-write-wins。
// 注意：sha256 是 CHAR(64) 主键（MySQL TEXT 不可作 PK），内容去重按方言 INSERT IGNORE。

const DUP = /unique|duplicate/i;

function conflict(msg: string): Error & { status: number } {
  const e = new Error(msg) as Error & { status: number };
  e.status = 409;
  return e;
}

export interface CommitMeta { author?: string; message?: string; expect?: number; }
export interface CommitResult { version: number; sha256: string; }
export interface HistoryEntry { version: number; author: string | null; message: string | null; created_at: number; }
export interface DiffLine { type: "ctx" | "add" | "del"; text: string; }

/** 行级 LCS；修剪公共前后缀后任一侧超格子预算（4M 格 ≈ 16MB DP 矩阵）即拒绝。 */
export function lineDiff(a: string, b: string): DiffLine[] {
  const A0 = a.split("\n"), B0 = b.split("\n");
  let s = 0;
  while (s < A0.length && s < B0.length && A0[s] === B0[s]) s++;
  let ea = A0.length, eb = B0.length;
  while (ea > s && eb > s && A0[ea - 1] === B0[eb - 1]) { ea--; eb--; }
  const head = A0.slice(0, s).map((text) => ({ type: "ctx" as const, text }));
  const tail = A0.slice(ea).map((text) => ({ type: "ctx" as const, text }));
  const A = A0.slice(s, ea), B = B0.slice(s, eb);
  if (A.length * B.length > 4_000_000) throw new Error("diff too large (line budget exceeded)");
  const w = B.length + 1;
  const dp = new Int32Array((A.length + 1) * w);
  for (let i = A.length - 1; i >= 0; i--) {
    for (let j = B.length - 1; j >= 0; j--) {
      dp[i * w + j] = A[i] === B[j] ? dp[(i + 1) * w + j + 1] + 1 : Math.max(dp[(i + 1) * w + j], dp[i * w + j + 1]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0, j = 0;
  while (i < A.length && j < B.length) {
    if (A[i] === B[j]) { out.push({ type: "ctx", text: A[i] }); i++; j++; }
    else if (dp[(i + 1) * w + j] >= dp[i * w + j + 1]) { out.push({ type: "del", text: A[i] }); i++; }
    else { out.push({ type: "add", text: B[j] }); j++; }
  }
  while (i < A.length) { out.push({ type: "del", text: A[i] }); i++; }
  while (j < B.length) { out.push({ type: "add", text: B[j] }); j++; }
  return [...head, ...out, ...tail];
}

export function createVersions(dbi: DBInstance) {
  const d = dbi.dialect();
  const P = (n: number) => (d === "postgres" ? `$${n + 1}` : "?");
  const insIgnore = d === "sqlite" ? "INSERT OR IGNORE INTO"
    : d === "mysql" ? "INSERT IGNORE INTO"
    : "INSERT INTO";
  const pgTail = d === "postgres" ? " ON CONFLICT (sha256) DO NOTHING" : "";

  async function commit(entityType: string, entityId: string, content: string,
                        meta: CommitMeta = {}): Promise<CommitResult> {
    const sha = crypto.sha256Hex(content);
    await dbi.exec(`${insIgnore} oj_ver_content (sha256, content) VALUES (${P(0)}, ${P(1)})${pgTail}`,
      [sha, content]);
    const rows = await dbi.query(
      `SELECT COALESCE(MAX(version), 0) AS v FROM oj_versions WHERE entity_type = ${P(0)} AND entity_id = ${P(1)}`,
      [entityType, entityId]);
    const head = Number(rows[0].v);
    if (meta.expect !== undefined && Number(meta.expect) !== head) {
      throw conflict(`version conflict (expect ${meta.expect}, head ${head})`);
    }
    try {
      await dbi.exec(
        `INSERT INTO oj_versions (entity_type, entity_id, version, sha256, author, message, created_at)
         VALUES (${P(0)}, ${P(1)}, ${P(2)}, ${P(3)}, ${P(4)}, ${P(5)}, ${P(6)})`,
        [entityType, entityId, head + 1, sha, meta.author ?? null, meta.message ?? null,
         Math.floor(Date.now() / 1000)]);
    } catch (e) {
      if (DUP.test(String(e))) throw conflict("version conflict");   // 并发双 commit：后插者撞 UNIQUE
      throw e;
    }
    return { version: head + 1, sha256: sha };
  }

  async function get(entityType: string, entityId: string, version: number | "head"): Promise<string> {
    const vSql = version === "head"
      ? `(SELECT COALESCE(MAX(version), 0) FROM oj_versions WHERE entity_type = ${P(0)} AND entity_id = ${P(1)})`
      : P(2);
    const params = version === "head" ? [entityType, entityId] : [entityType, entityId, version];
    const rows = await dbi.query(
      `SELECT c.content AS content FROM oj_versions v JOIN oj_ver_content c ON c.sha256 = v.sha256
       WHERE v.entity_type = ${P(0)} AND v.entity_id = ${P(1)} AND v.version = ${vSql}`, params);
    if (!rows.length) throw new Error(`version not found: ${entityType}/${entityId}@${version}`);
    return String(rows[0].content);
  }

  async function history(entityType: string, entityId: string,
                         opts: { limit?: number; offset?: number } = {}): Promise<HistoryEntry[]> {
    const limit = Math.max(1, Number(opts.limit ?? 20));
    const offset = Math.max(0, Number(opts.offset ?? 0));
    return await dbi.query(
      `SELECT version, author, message, created_at FROM oj_versions
       WHERE entity_type = ${P(0)} AND entity_id = ${P(1)} ORDER BY version DESC LIMIT ${limit} OFFSET ${offset}`,
      [entityType, entityId]);
  }

  async function diff(entityType: string, entityId: string, va: number, vb: number): Promise<DiffLine[]> {
    return lineDiff(await get(entityType, entityId, va), await get(entityType, entityId, vb));
  }

  async function rollback(entityType: string, entityId: string, version: number,
                          meta: CommitMeta = {}): Promise<CommitResult> {
    return commit(entityType, entityId, await get(entityType, entityId, version), meta);
  }

  return { commit, get, history, diff, rollback };
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo xtask bin && ./bin/oj test -c sample/config.yaml -d sample/src -t tests`
Expected: versions 4 个用例全绿（`oj_versions`/`oj_ver_content` 由 migrate 建出——oj test 会跑 migrate；若测试库未跑迁移，先 `./bin/oj migrate -c sample/config.yaml -d sample/src`）

- [ ] **Step 6: 提交**

```bash
git add sample/src/_platform/migrations/0002__versions*.sql sample/src/_platform/_shared/versions.ts sample/tests/versions.test.ts
git commit -m "feat(versions): 内容版本化原语——CAS 内容表 + 修订表 + 行级 diff（L1）

unix@vip.qq.com ai"
```

---

### Task 3: search——引擎原生 FTS 共享模块 + L1（Phase 2）

**Files:**
- Create: `sample/src/_platform/_shared/search.ts`
- Test: `sample/tests/search.test.ts`

**Interfaces:**
- Consumes: `db.dialect()`（Task 1）、tx 对象 `isTx` 标记。
- Produces: `createSearch(dbi)` → `{ ensureIndex(table, {fields, pk?, tokenizer?}), index(table, row), remove(table, pkValue), query(table, q, {limit?, fields?, highlight?}) }`。`hits = [{id, score, fragments?}]`，score 为引擎原始分（不跨引擎比较）。

- [ ] **Step 1: 写失败测试** `sample/tests/search.test.ts`

```ts
// L1 引擎原生 FTS（sqlite 腿 = bundled FTS5 trigram）。
import { createSearch } from "../src/_platform/_shared/search";

const search = createSearch(db);

describe("search (sqlite FTS5)", () => {
  it("index/query/highlight/remove", async () => {
    await db.exec("CREATE TABLE IF NOT EXISTS search_docs (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT)");
    await db.exec("DELETE FROM search_docs");
    await search.ensureIndex("search_docs", { fields: ["title", "body"] });
    await search.index("search_docs", { id: 1, title: "部署手册", body: "在生产环境部署 oj 服务" });
    await search.index("search_docs", { id: 2, title: "公告", body: "本周例会取消" });

    const hits = await search.query("search_docs", "环境部署", { limit: 10 });   // trigram ≥3 字
    expect(hits.length).toBe(1);
    expect(Number(hits[0].id)).toBe(1);

    const hl = await search.query("search_docs", "部署 oj", { limit: 10, highlight: true });
    expect(String(hl[0].fragments.body)).toContain("<mark>");

    const short = await search.query("search_docs", "部署", { limit: 10 });      // <3 字 → LIKE 回退
    expect(short.some((h) => Number(h.id) === 1)).toBe(true);

    await search.remove("search_docs", 1);
    expect(await search.query("search_docs", "环境部署", { limit: 10 })).toHaveLength(0);
  });

  it("ensureIndex 拒绝在 db.tx 内调用（DDL 隐式提交）", async () => {
    await db.tx(async (tx) => {
      await expect(createSearch(tx as any).ensureIndex("nope", { fields: ["x"] })).rejects.toThrow(/inside db\.tx/);
    });
  });

  it("标识符白名单", () => {
    expect(() => createSearch(db).query("docs; DROP TABLE users", "x")).toThrow(/invalid/);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo xtask bin && ./bin/oj test -c sample/config.yaml -d sample/src -t tests`
Expected: FAIL（Cannot find module `../src/_platform/_shared/search`）

- [ ] **Step 3: 实现** `sample/src/_platform/_shared/search.ts`

```ts
// 引擎原生 FTS 统一面（v0.1.61，spec §二）：sqlite FTS5(trigram) / mysql FULLTEXT(ngram) /
// pg tsvector+GIN。ensureIndex = DDL（进程内每表一次，禁止在 db.tx 内调用——mysql DDL
// 隐式提交调用方事务）；index/remove = 业务表 upsert/delete（索引随业务行自动维护，
// 绝不直写 FTS 对象）；query = MATCH 裸 SQL + 绑定参数。score 为引擎原始分，不可跨引擎比。

export type Dialect = "sqlite" | "mysql" | "postgres";
const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;
const ensured = new Set<string>();

function id(name: string, what: string): string {
  if (!IDENT.test(name)) throw new Error(`search: invalid ${what} ${JSON.stringify(name)}`);
  return name;
}
function likeEscape(s: string): string { return s.replace(/[\\%_]/g, (c) => "\\" + c); }
function mark(text: string, q: string): string {
  const i = text.indexOf(q);
  if (i < 0) return text.slice(0, 60);
  const s = Math.max(0, i - 30);
  return (s > 0 ? "…" : "") + text.slice(s, i) + "<mark>" + q + "</mark>"
    + text.slice(i + q.length, i + q.length + 30) + (i + q.length + 30 < text.length ? "…" : "");
}

export interface EnsureOpts { fields: string[]; pk?: string; tokenizer?: string; }
export interface QueryOpts { limit?: number; fields?: string[]; highlight?: boolean; }
export interface Hit { id: unknown; score: number; fragments?: Record<string, string>; }

export function createSearch(dbi: DBInstance) {
  const d: Dialect = dbi.dialect();
  const P = (i: number) => (d === "postgres" ? `$${i + 1}` : "?");
  const pks = new Map<string, string>();
  const fieldsOf = new Map<string, string[]>();

  function registered(t: string, what: string): void {
    if (!pks.has(t)) throw new Error(`search.${what}: call ensureIndex('${t}') first`);
  }

  async function ensureIndex(table: string, opts: EnsureOpts): Promise<void> {
    const t = id(table, "table");
    const pk = id(opts.pk ?? "id", "pk");
    const fs = opts.fields.map((f) => id(f, "field"));
    if (!fs.length) throw new Error("search.ensureIndex: fields must not be empty");
    const key = `${d}:${t}`;
    if (ensured.has(key)) return;
    if ((dbi as { isTx?: true }).isTx) {
      throw new Error("search.ensureIndex: DDL inside db.tx is not allowed (mysql implicit commit)");
    }
    if (d === "sqlite") {
      const tk = (opts.tokenizer ?? "trigram").replace(/[^a-z0-9_]/g, "");
      await dbi.exec(`CREATE VIRTUAL TABLE IF NOT EXISTS "${t}_fts" USING fts5(${
        fs.join(", ")}, tokenize='${tk}', content='${t}', content_rowid='${pk}')`, []);
      const cols = fs.join(", ");
      await dbi.exec(`CREATE TRIGGER IF NOT EXISTS "${t}_fts_ai" AFTER INSERT ON "${t}" BEGIN
        INSERT INTO "${t}_fts"(rowid, ${cols}) VALUES (new.${pk}, ${
        fs.map((f) => `new.${f}`).join(", ")}); END`, []);
      await dbi.exec(`CREATE TRIGGER IF NOT EXISTS "${t}_fts_ad" AFTER DELETE ON "${t}" BEGIN
        INSERT INTO "${t}_fts"("${t}_fts", rowid, ${cols}) VALUES ('delete', old.${pk}, ${
        fs.map((f) => `old.${f}`).join(", ")}); END`, []);
      await dbi.exec(`CREATE TRIGGER IF NOT EXISTS "${t}_fts_au" AFTER UPDATE ON "${t}" BEGIN
        INSERT INTO "${t}_fts"("${t}_fts", rowid, ${cols}) VALUES ('delete', old.${pk}, ${
        fs.map((f) => `old.${f}`).join(", ")});
        INSERT INTO "${t}_fts"(rowid, ${cols}) VALUES (new.${pk}, ${
        fs.map((f) => `new.${f}`).join(", ")}); END`, []);
    } else if (d === "mysql") {
      const idxName = `ft_${t}`;
      const hit = await dbi.query(`SELECT COUNT(*) AS n FROM information_schema.statistics
        WHERE table_schema = DATABASE() AND table_name = ${P(0)} AND index_name = ${P(1)}`, [t, idxName]);
      if (Number(hit[0].n) === 0) {
        await dbi.exec(`ALTER TABLE \`${t}\` ADD FULLTEXT INDEX \`${idxName}\` (${
          fs.join(", ")}) WITH PARSER ngram`, []);
      }
    } else {
      const hit = await dbi.query(`SELECT COUNT(*) AS n FROM information_schema.columns
        WHERE table_schema = current_schema() AND table_name::text = ${P(0)} AND column_name::text = 'tsv'`, [t]);
      if (Number(hit[0].n) === 0) {
        const expr = `to_tsvector('simple', ${fs.map((f) => `coalesce(${f}, '')`).join(" || ' ' || ")})`;
        await dbi.exec(`ALTER TABLE "${t}" ADD COLUMN IF NOT EXISTS tsv tsvector GENERATED ALWAYS AS (${expr}) STORED`, []);
        await dbi.exec(`CREATE INDEX IF NOT EXISTS "idx_${t}_tsv" ON "${t}" USING gin (tsv)`, []);
      }
    }
    pks.set(t, pk);
    fieldsOf.set(t, fs);
    ensured.add(key);
  }

  async function index(table: string, row: Record<string, unknown>): Promise<void> {
    const t = id(table, "table");
    registered(t, "index");
    const cols = Object.keys(row);
    if (!cols.length) throw new Error("search.index: empty row");
    cols.forEach((c) => id(c, "column"));
    const q = (c: string) => (d === "mysql" ? `\`${c}\`` : `"${c}"`);
    const colSql = cols.map(q).join(", ");
    const marks = cols.map((_, i) => P(i)).join(", ");
    const vals = [...Object.values(row)];
    if (d === "sqlite") {
      await dbi.exec(`INSERT OR REPLACE INTO ${q(t)} (${colSql}) VALUES (${marks})`, vals);
    } else if (d === "mysql") {
      const set = cols.map((c) => `${q(c)} = VALUES(${q(c)})`).join(", ");
      await dbi.exec(`INSERT INTO ${q(t)} (${colSql}) VALUES (${marks}) ON DUPLICATE KEY UPDATE ${set}`, vals);
    } else {
      const set = cols.map((c) => `${c} = EXCLUDED.${c}`).join(", ");
      await dbi.exec(`INSERT INTO ${q(t)} (${colSql}) VALUES (${marks}) ON CONFLICT ("${
        pks.get(t)}") DO UPDATE SET ${set}`, vals);
    }
  }

  async function remove(table: string, pkValue: unknown): Promise<void> {
    const t = id(table, "table");
    registered(t, "remove");
    const q = (c: string) => (d === "mysql" ? `\`${c}\`` : `"${c}"`);
    await dbi.exec(`DELETE FROM ${q(t)} WHERE ${q(pks.get(t)!)} = ${P(0)}`, [pkValue]);
  }

  async function query(table: string, q: string, opts: QueryOpts = {}): Promise<Hit[]> {
    const t = id(table, "table");
    registered(t, "query");
    const pk = pks.get(t)!;
    const all = fieldsOf.get(t)!;
    const fs = opts.fields?.length ? opts.fields.map((f) => id(f, "field")) : all;
    const limit = Math.max(1, Number(opts.limit ?? 20));
    const qw = (c: string) => (d === "mysql" ? `\`${c}\`` : `"${c}"`);
    const num = (rows: Array<Record<string, unknown>>) =>
      rows.map((r) => ({ id: r.id, score: Number(r.score) }));

    if (d === "sqlite") {
      if ([...q].length > 0 && [...q].length < 3) {
        // trigram 对 <3 字符查询静默零命中 → LIKE 回退（score=1）
        const where = fs.map((f) => `${qw(f)} LIKE ${P(0)} ESCAPE '\\'`).join(" OR ");
        const like = `%${likeEscape(q)}%`;
        return await dbi.query(
          `SELECT ${qw(pk)} AS id, 1 AS score FROM ${qw(t)} WHERE ${where} LIMIT ${limit}`,
          fs.map(() => like));
      }
      const phrase = `"${q.replace(/"/g, '""')}"`;
      const snip = opts.highlight
        ? ", " + fs.map((f, i) => `snippet("${t}_fts", ${i}, '<mark>', '</mark>', '…', 12) AS "s_${f}"`).join(", ")
        : "";
      const rows = await dbi.query(
        `SELECT rowid AS id, bm25("${t}_fts") AS score${snip} FROM "${t}_fts"
         WHERE "${t}_fts" MATCH ${P(0)} ORDER BY rank LIMIT ${limit}`, [phrase]);
      return rows.map((r: Record<string, unknown>) => {
        const h: Hit = { id: r.id, score: Number(r.score) };
        if (opts.highlight) {
          h.fragments = {};
          for (const f of fs) h.fragments[f] = String(r[`s_${f}`] ?? "");
        }
        return h;
      });
    }
    if (d === "mysql") {
      const cols = fs.join(", ");
      const rows = await dbi.query(
        `SELECT ${qw(pk)} AS id, MATCH(${cols}) AGAINST(${P(0)} IN NATURAL LANGUAGE MODE) AS score
         FROM ${qw(t)} WHERE MATCH(${cols}) AGAINST(${P(1)} IN NATURAL LANGUAGE MODE) LIMIT ${limit}`,
        [q, q]);
      if (!opts.highlight) return num(rows);
      const out: Hit[] = [];
      for (const r of rows) {
        const fragments: Record<string, string> = {};
        for (const f of fs) {
          const v = await dbi.query(`SELECT ${qw(f)} AS v FROM ${qw(t)} WHERE ${qw(pk)} = ${P(0)}`, [r.id]);
          fragments[f] = mark(String((v[0] as Record<string, unknown>)?.v ?? ""), q);
        }
        out.push({ id: (r as Record<string, unknown>).id, score: Number((r as Record<string, unknown>).score), fragments });
      }
      return out;
    }
    // postgres：ts_headline 是文档级 → 单一 fragments._doc
    const hl = opts.highlight ? `, ts_headline('simple', tsv, plainto_tsquery('simple', ${P(1)}), '<mark>', '</mark>', '…', 12) AS headline` : "";
    const rows = await dbi.query(
      `SELECT ${qw(pk)} AS id, ts_rank(tsv, plainto_tsquery('simple', ${P(0)})) AS score${hl}
       FROM ${qw(t)} WHERE tsv @@ plainto_tsquery('simple', ${P(0)}) LIMIT ${limit}`, [q, q]);
    return rows.map((r: Record<string, unknown>) => {
      const h: Hit = { id: r.id, score: Number(r.score) };
      if (opts.highlight) h.fragments = { _doc: String(r.headline ?? "") };
      return h;
    });
  }

  return { ensureIndex, index, remove, query };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo xtask bin && ./bin/oj test -c sample/config.yaml -d sample/src -t tests`
Expected: search 3 个用例 + versions 4 个用例全绿

- [ ] **Step 5: 提交**

```bash
git add sample/src/_platform/_shared/search.ts sample/tests/search.test.ts
git commit -m "feat(search): 引擎原生 FTS 统一面——sqlite FTS5 / mysql FULLTEXT(ngram) / pg tsvector（L1 sqlite 腿）

unix@vip.qq.com ai"
```

---

### Task 4: `http_sso:` config 段（Phase 3a）

**Files:**
- Modify: `src/config.rs`（`Config` :1120 起的字段区，`oidc` :1137-1138 之后；`OidcSection` :945-976 附近加结构体；`validate_anon_paths` :552-568 附近加校验函数）
- Test: `src/config.rs` tests 模块

**Interfaces:**
- Produces: `Config::http_sso: Option<HashMap<String, HttpSsoProvider>>`；`HttpSsoProvider`（Deserialize+Clone+Default+Serialize）；`validate_http_sso(&Config) -> Result<(), String>`。Task 5 消费。

- [ ] **Step 1: 写失败测试**（config.rs tests 模块，镜像既有 validate 测试）

```rust
/// http_sso 校验矩阵（v0.1.61）：kind 白名单 / form 必 success_when / cas2 必两 URL /
/// name 禁 `:` / 缺省拒 http，allow_http 放行。
#[test]
fn validate_http_sso_matrix() {
    let mut cfg = Config::default();
    assert!(validate_http_sso(&cfg).is_ok(), "段缺省 = 不校验");
    let mut p = HttpSsoProvider::default();
    p.kind = "form".into();
    p.login_url = "https://sso.corp.com/login".into();
    let mut m = std::collections::HashMap::new();
    m.insert("corp".to_string(), p.clone());
    cfg.http_sso = Some(m.clone());
    assert!(validate_http_sso(&cfg).is_err(), "form 缺 success_when → err");
    p.success_when.insert("message".to_string(), serde_json::json!(""));
    let mut m2 = std::collections::HashMap::new();
    m2.insert("corp".to_string(), p.clone());
    cfg.http_sso = Some(m2);
    assert!(validate_http_sso(&cfg).is_ok());
    let mut m3 = std::collections::HashMap::new();
    m3.insert("ba:d".to_string(), p.clone());
    cfg.http_sso = Some(m3);
    assert!(validate_http_sso(&cfg).is_err(), "name 含 `:` → err");
    let mut m4 = std::collections::HashMap::new();
    let mut p2 = HttpSsoProvider::default();
    p2.kind = "cas2".into();
    p2.login_url = "http://sso.corp.com/login".into();   // http 且未 allow_http
    m4.insert("corp".to_string(), p2.clone());
    cfg.http_sso = Some(m4);
    assert!(validate_http_sso(&cfg).is_err(), "非 https 缺省拒绝");
    p2.allow_http = true;
    let mut m5 = std::collections::HashMap::new();
    m5.insert("corp".to_string(), p2);
    cfg.http_sso = Some(m5);
    assert!(validate_http_sso(&cfg).is_err(), "cas2 缺 cas_validate_url/callback_url → err（在 https 检查之后仍要报）");
}
```

（若 config.rs 测试里 `serde_json` 未直接可用，用 `serde_json::json!` 已有引用或补 `use`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js validate_http_sso`
Expected: 编译错误（HttpSsoProvider/validate_http_sso 不存在）

- [ ] **Step 3: 实现结构体 + 校验**

config.rs（`OidcSection` 后面）：

```rust
/// HTTP 表单 / CAS SSO 提供者（v0.1.61，spec 2026-10-11 §一）。`http_sso:` 段存在即启用；
/// 键 = 提供者名（禁 `:`，JIT 键按 `:` 切分）。段内敏感值写 ENC[...]（装载期整树自动解封）。
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct HttpSsoProvider {
    /// form | cas2
    pub kind: String,
    /// form: 凭据校验端点；cas2: CAS /login
    #[serde(default)]
    pub login_url: String,
    /// cas2 必填：ticket 校验端点（/serviceValidate）
    #[serde(default)]
    pub cas_validate_url: String,
    /// cas2 必填：本站回跳绝对 URL（装配期固定，禁从请求 Host 派生——防 ticket 校验被引偏）
    #[serde(default)]
    pub callback_url: String,
    /// 缺省 username
    #[serde(default = "default_sso_user_field")]
    pub user_field: String,
    /// 缺省 password
    #[serde(default = "default_sso_pass_field")]
    pub pass_field: String,
    /// form 出站编码：form(urlencoded，缺省，对齐上游) | json
    #[serde(default = "default_sso_body_encoding")]
    pub body_encoding: String,
    /// 附加固定字段（app_id 等；secret 类值写 ENC[...]）
    #[serde(default)]
    pub extra_fields: HashMap<String, serde_json::Value>,
    /// 成功判定：点路径 → 期望值，全部命中即成功；form 必填
    #[serde(default)]
    pub success_when: HashMap<String, serde_json::Value>,
    /// 用户信息映射：本地 users 列 → 上游响应点路径
    #[serde(default)]
    pub profile: HashMap<String, String>,
    /// 显式允许 http:// 上游（内网联调）。缺省拒绝（fail-closed，同 smtp allow_none_tls 口径）
    #[serde(default)]
    pub allow_http: bool,
}

fn default_sso_user_field() -> String { "username".into() }
fn default_sso_pass_field() -> String { "password".into() }
fn default_sso_body_encoding() -> String { "form".into() }
```

`Config` 结构体 `oidc` 字段（:1137-1138）之后加：

```rust
    /// HTTP 表单 / CAS SSO（v0.1.61）。None = 不启用；段存在即启用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_sso: Option<HashMap<String, HttpSsoProvider>>,
```

（`Serialize` 若 Config 系列未统一 derive，仅给 HttpSsoProvider 加即可——Task 5 的 `serde_json::to_value` 需要。）

`validate_anon_paths`（:552）附近加：

```rust
/// http_sso 段装配期校验（v0.1.61）：kind 白名单 / form 必 success_when / cas2 必两 URL /
/// name 禁 `:` / HTTPS 门禁（allow_http 显式放行）。
pub fn validate_http_sso(cfg: &Config) -> Result<(), String> {
    let Some(provs) = &cfg.http_sso else { return Ok(()); };
    for (name, p) in provs {
        if name.contains(':') {
            return Err(format!("http_sso.{name}: name must not contain ':'"));
        }
        match p.kind.as_str() {
            "form" => {
                if p.success_when.is_empty() {
                    return Err(format!("http_sso.{name}: form requires success_when"));
                }
            }
            "cas2" => {
                if p.cas_validate_url.is_empty() || p.callback_url.is_empty() {
                    return Err(format!(
                        "http_sso.{name}: cas2 requires cas_validate_url and callback_url"));
                }
            }
            other => return Err(format!("http_sso.{name}: unknown kind {other:?} (form | cas2)")),
        }
        for (what, url) in [
            ("login_url", &p.login_url),
            ("cas_validate_url", &p.cas_validate_url),
            ("callback_url", &p.callback_url),
        ] {
            if !url.is_empty() && !url.starts_with("https://") && !p.allow_http {
                return Err(format!(
                    "http_sso.{name}: {what} must be https (set allow_http: true to override)"));
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js validate_http_sso`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add src/config.rs
git commit -m "feat(config): http_sso 段（HTTP 表单/CAS SSO 提供者）+ 装配期校验

unix@vip.qq.com ai"
```

---

### Task 5: `sso` 全局装配（Phase 3b）

**Files:**
- Create: `src/bridge/sso.rs`
- Modify: `src/bridge/mod.rs`（`pub mod sso;`、ops 列表 oidc 组 :397-399 后加 `sso::op_sso_providers`、`StableState` :189 oidc 之后加 `sso` 字段、`Extras` :244 之后加 `sso` 字段、:717 extras→state 搬运处加 `sso: extras.sso,`、**其它 StableState 字面量构造点按编译器提示补字段**）
- Modify: `oj/src/app.rs`（`build_jwt_and_oidc` :462-486 附近加 build_sso；make_bridge 的 Extras 字面量 :1119 附近与 StableState 构造点 :1164 附近各加 `sso`）
- Modify: `src/bridge/bootstrap.js`（import 块 + `globalThis.oidc` 段 :943-953 之后）
- Modify: `sample/global.d.ts`（`OidcApi` :340-355 附近 + declare global 挂载 :494 附近）
- Test: `src/bridge/sso.rs` tests

**Interfaces:**
- Consumes: `Config::http_sso`（Task 4）。
- Produces: JS `sso.providers: Record<string, SsoProviderCfg>`（未配置抛 `http sso not configured (config http_sso: section missing)`）。

- [ ] **Step 1: 写失败测试**（`src/bridge/sso.rs` 内，镜像 `oidc.rs` :443 的 not-configured 测试形态）

```rust
/// 段缺省 → `http sso not configured`（与 oidc not configured 同语义）。
#[tokio::test(flavor = "current_thread")]
async fn sso_providers_errors_when_unconfigured() {
    // Bridge::with_dbs_and_loader + run_with，照 oidc.rs 既有同名测试机械仿写；
    // 求值 JS：sso.providers → 断言错误文案含 "http sso not configured"。
}
```

- [ ] **Step 2: 跑测试确认失败**，然后实现 `src/bridge/sso.rs`：

```rust
//! HTTP 表单 / CAS SSO 提供者注册表（v0.1.61）：装配期注入 config `http_sso:` 段，
//! JS 登录端点（sample/src/sso/）经 `sso.providers` 读取。段不存在 = not configured
//! （同 oidc 语义）。暴露面声明：providers 对 handler 全量可读，与 oidc.info（含
//! client_secret）同级；ENC[...] 密封值已在 config 装载期整树解封（secret.rs decrypt_tree）。

use std::cell::RefCell;
use std::rc::Rc;

use deno_core::op2;

use crate::bridge::JsErrorBox;

/// 取 SSO 配置态（未配置 → 报错，同 oidc/jwt/es 的 not configured 语义）。
fn sso_of(state: &deno_core::OpState) -> Result<std::sync::Arc<serde_json::Value>, JsErrorBox> {
    state
        .borrow::<std::sync::Arc<crate::bridge::StableState>>()
        .sso
        .clone()
        .ok_or_else(|| JsErrorBox::generic("http sso not configured (config http_sso: section missing)"))
}

#[op2]
pub fn op_sso_providers(
    state: Rc<RefCell<deno_core::OpState>>,
) -> Result<serde_json::Value, JsErrorBox> {
    Ok((*sso_of(&state.borrow())?).clone())
}
```

（`op2` 属性宏的写法（`#[op2]` vs `#[deno_core::op2]`）、`JsErrorBox` 导入路径照 `oidc.rs` :158-187 原样。）

- [ ] **Step 3: 接线**

1. `mod.rs`：`pub mod sso;`（oidc mod 声明旁）；ops 列表 `oidc::op_oidc_info`（:399）后加
   `sso::op_sso_providers,`；`StableState`（:189 后）加
   ```rust
   /// HTTP 表单 / CAS SSO 提供者（v0.1.61）。None = `sso.providers` 报 not configured。
   pub sso: Option<Arc<serde_json::Value>>,
   ```
   `Extras`（:244 后）加同类型 `pub sso: Option<Arc<serde_json::Value>>`（`#[derive(Default)]`
   已覆盖）；:717 `oidc: extras.oidc,` 后加 `sso: extras.sso,`。**编译器若报其它
   StableState 构造点缺字段，逐个补 `sso: <对应来源>（测试夹具补 None）。**
2. `oj/src/app.rs`：
   ```rust
   /// http_sso 段 → bridge Extras.sso（v0.1.61）。段不存在 = None（JS 报 not configured）。
   fn build_sso(cfg: &Config) -> Option<Arc<serde_json::Value>> {
       cfg.http_sso.as_ref().map(|m| Arc::new(serde_json::to_value(m).expect("http_sso serialize")))
   }
   ```
   make_bridge 的 Extras 字面量（:1119 附近）与 StableState 构造点（:1164 附近）各加
   `sso: build_sso(cfg),`。
3. `bootstrap.js`：import 块加 `op_sso_providers,`；`globalThis.oidc` 段（:953）后加：
   ```js
   // ----- sso: HTTP form / CAS SSO provider registry (assembly-time http_sso: config) -----
   globalThis.sso = {
     get providers() { return op_sso_providers(); },
   };
   ```
   **只准 ASCII。**
4. `sample/global.d.ts`（`OidcApi` 后）：
   ```ts
   // sso.*：HTTP 表单 / CAS SSO 提供者注册表（config http_sso: 段注入；未配置调用报
   // http sso not configured）。登录端点（业务模块）读它分派上游；详见 api-manual §sso。
   interface SsoProviderCfg {
     kind: "form" | "cas2";
     login_url: string;
     cas_validate_url?: string;
     callback_url?: string;
     user_field?: string;
     pass_field?: string;
     body_encoding?: "form" | "json";
     extra_fields?: Record<string, unknown>;
     success_when?: Record<string, unknown>;
     profile?: Record<string, string>;
     allow_http?: boolean;
   }
   interface SsoApi { readonly providers: Record<string, SsoProviderCfg>; }
   ```
   declare global 内 `const oidc: OidcApi;`（:494）后加 `const sso: SsoApi;`

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --release -p only-js sso_ && cargo clippy --release -p only-js --all-targets -- -D warnings && cargo clippy --release -p oj --all-targets -- -D warnings`
Expected: PASS（含既有测试回归）

- [ ] **Step 5: 提交**

```bash
git add src/bridge/sso.rs src/bridge/mod.rs oj/src/app.rs src/bridge/bootstrap.js sample/global.d.ts
git commit -m "feat(sso): JS 全局 sso.providers——http_sso 段装配期注入（not configured 同 oidc 语义）

unix@vip.qq.com ai"
```

---

### Task 6: sample sso 模块 + sample 配置（Phase 3c）

**Files:**
- Create: `sample/src/sso/manifest.yaml`
- Create: `sample/src/sso/_shared/core.ts`
- Create: `sample/src/sso/login/api.ts`
- Create: `sample/src/sso/cas/login/api.ts`
- Create: `sample/src/sso/cas/callback/api.ts`
- Modify: `sample/config.yaml`（`oidc:` 段 :172-195 附近加 `http_sso:` 演示段；`tenant.anonymous_paths` :135-141 与 `auth.anonymous_paths` :150-165 各加两条）

**Interfaces:**
- Consumes: `sso.providers`（Task 5）、`#/auth/_shared/session` 的 `issueTokens`、`#/auth/_shared/util` 的 `redirect`。
- Produces: `POST /v1/api/sso/login/`（form）、`GET /v1/api/sso/cas/login/`（302）、`GET /v1/api/sso/cas/callback/`（ticket → 302 交接）。JIT 键 `sso:<provider>:<tenant>:<username>`，占位 hash `"!sso"`。

- [ ] **Step 1: manifest** `sample/src/sso/manifest.yaml`

```yaml
name: "sso"
desc: "HTTP 表单 / CAS SSO 登录来源（参考实现）：form 转发凭据 / cas2 302+ticket；JIT 建号与会话复用 auth/_shared（场景 28）"
version: "0.1.0"
deps:
  _platform: "^0.1.0"
  auth: "^0.1.0"
```

- [ ] **Step 2: 共享核心** `sample/src/sso/_shared/core.ts`

```ts
// sso 共享核心：provider 解析 / success_when 判定 / JIT 建号。
// JIT 键 sso:<provider>:<tenant>:<username>（tenant 段防 users 表跨租户混同）；
// password_hash = "!sso" 非法占位（bcrypt.verify 恒 false → 该号永不可密码登录）。

export type Prov = {
  kind: "form" | "cas2";
  login_url: string;
  cas_validate_url?: string;
  callback_url?: string;
  user_field?: string;
  pass_field?: string;
  body_encoding?: "form" | "json";
  extra_fields?: Record<string, unknown>;
  success_when?: Record<string, unknown>;
  profile?: Record<string, string>;
  allow_http?: boolean;
};

export function providerOf(name: string): Prov | null {
  return (sso.providers as Record<string, Prov>)[name] ?? null;
}

export function dotGet(obj: unknown, path: string): unknown {
  let cur: unknown = obj;
  for (const seg of String(path).split(".")) {
    if (cur == null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[seg];
  }
  return cur;
}

/** success_when：缺失/null 仅当期望为 null 命中；其余 String(actual)===String(want)。 */
export function hitWhen(body: unknown, when: Record<string, unknown>): boolean {
  for (const [path, want] of Object.entries(when)) {
    const got = dotGet(body, path);
    if (want === null) {
      if (got !== null && got !== undefined) return false;
      continue;
    }
    if (got === null || got === undefined || String(got) !== String(want)) return false;
  }
  return true;
}

export async function jit(provider: string, username: string, upstream: unknown,
                          p: Prov): Promise<{ id: string; roles: string[] }> {
  const localName = `sso:${provider}:${http.tenantId ?? ""}:${username}`;
  let rows = await db.table("users").select(["id", "roles"])
    .where({ field: "username", op: "eq", value: localName }).all();
  if (!rows.length) {
    const extra: Record<string, unknown> = {};
    for (const [col, path] of Object.entries(p.profile ?? {})) {
      extra[col] = String(dotGet(upstream, path) ?? "");
    }
    await db.table("users")
      .insert({ username: localName, password_hash: "!sso", roles: "[]", ...extra }).run();
    rows = await db.table("users").select(["id", "roles"])
      .where({ field: "username", op: "eq", value: localName }).all();
  }
  let roles: string[] = [];
  try { roles = JSON.parse(<string>rows[0].roles || "[]"); } catch { roles = []; }
  return { id: String(rows[0].id), roles };
}

/** 302 目标白名单：配置固定值或 `/` 开头且非 `//` 的相对路径（防 open-redirect）。 */
export function safeNext(raw: unknown): string {
  const s = String(raw ?? "/");
  return s.startsWith("/") && !s.startsWith("//") ? s : "/";
}
```

- [ ] **Step 3: form 登录** `sample/src/sso/login/api.ts`

```ts
// HTTP 表单 SSO：转发凭据到企业统一登录端点，success_when 判定 → JIT → 签发（场景 14 双 cookie）。
// 本端点是普通业务路由，必须匿名（config auth.anonymous_paths / tenant.anonymous_paths）。
import { issueTokens } from "#/auth/_shared/session";
import { hitWhen, providerOf } from "../_shared/core";

export default {
  async post() {
    const name = String(http.body?.provider ?? "");
    const p = providerOf(name);
    if (!p) { json.fail(400, `unknown sso provider: ${name}`); return; }
    const fields: Record<string, unknown> = { ...(p.extra_fields ?? {}) };
    fields[p.user_field ?? "username"] = String(http.body?.username ?? "");
    fields[p.pass_field ?? "password"] = String(http.body?.password ?? "");
    let r: Response;
    try {
      r = await fetch(p.login_url, {
        method: "POST",
        headers: {
          "content-type": p.body_encoding === "json"
            ? "application/json" : "application/x-www-form-urlencoded",
        },
        body: p.body_encoding === "json"
          ? JSON.stringify(fields)
          : new URLSearchParams(fields as Record<string, string>).toString(),
      });
    } catch {
      json.fail(502, "sso upstream failed");   // 网络错/上游挂 ≠ 密码错（两态拆分）
      return;
    }
    const body: unknown = await r.json().catch(() => null);
    if (!r.ok || !body || !hitWhen(body, p.success_when ?? {})) {
      json.fail(401, "invalid credentials");   // 不回吐外部响应体
      return;
    }
    const u = await jit(name, String(http.body?.username ?? ""), body, p);
    json.ok(await issueTokens(u.id, u.roles));
  },
};
```

- [ ] **Step 4: CAS 两腿** `sample/src/sso/cas/login/api.ts`

```ts
// CAS 2.0 跳转腿：302 到 {login_url}?service={callback_url}（service 装配期固定，两腿同串）。
import { redirect } from "#/auth/_shared/util";
import { providerOf } from "../../_shared/core";

export default {
  get() {
    const name = String(http.query.provider ?? "");
    const p = providerOf(name);
    if (!p || p.kind !== "cas2") { json.fail(400, `unknown cas provider: ${name}`); return; }
    redirect(`${p.login_url}?service=${encodeURIComponent(p.callback_url ?? "")}`);
  },
};
```

`sample/src/sso/cas/callback/api.ts`（**先读 `sample/src/auth/_shared/session.ts` 全文**，按其
实际返回形状取 access token 字段名；cookie 写法照 `sample/src/auth/login/api.ts` /
场景 14 的 `json.header("set-cookie", …)` 双发）：

```ts
// CAS 2.0 回跳腿：ticket → serviceValidate XML 判定 → JIT → 双 cookie 交接 → 302。
// 红线：token 不进 URL；302 目标走 safeNext 白名单；service 与跳转腿同串（装配期固定）。
import { redirect } from "#/auth/_shared/util";
import { providerOf, jit, safeNext } from "../../_shared/core";

export default {
  async get() {
    const name = String(http.query.provider ?? "");
    const p = providerOf(name);
    if (!p || p.kind !== "cas2") { json.fail(400, `unknown cas provider: ${name}`); return; }
    const ticket = String(http.query.ticket ?? "");
    let r: Response;
    try {
      r = await fetch(`${p.cas_validate_url}?ticket=${encodeURIComponent(ticket)
        }&service=${encodeURIComponent(p.callback_url ?? "")}`);
    } catch {
      json.fail(502, "sso upstream failed");
      return;
    }
    const xml = await r.text();
    // 命名空间前缀可变（<cas:…> 或默认命名空间）、元素跨行：[\s\S] 而非 .
    const ok = /<[\w.-]*:?authenticationSuccess[\s/>]/.test(xml);
    const m = xml.match(/<(?:[\w.-]+:)?user(?:\s[^>]*)?>([\s\S]*?)<\/(?:[\w.-]+:)?user>/);
    if (!r.ok || !ok || !m || !m[1].trim()) { json.fail(401, "invalid credentials"); return; }
    const u = await jit(name, m[1].trim(), xml, p);
    const tokens = await issueTokens(u.id, u.roles);
    // cookie 双发（照 auth/login 的写法取 tokens 的 access/csrf 字段——先读 session.ts 定名）
    json.header("set-cookie", `oj_sess=${/* access token */ ""}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400`);
    redirect(safeNext(http.query.next));
  },
};
```

（cookie 行的占位以 session.ts 实际字段为准填实——这一步允许按真实 API 微调，其余照抄。）

- [ ] **Step 5: sample 配置**

`sample/config.yaml` `oidc:` 段附近加（演示段，占位上游）：

```yaml
http_sso:                          # HTTP 表单 / CAS SSO（v0.1.61）；段存在即启用
  corp:
    kind: form
    login_url: "https://sso.example.com/login"   # 占位示例，对接企业 SSO 时替换
    success_when: { message: "" }  # 上游成功时 message 为空串（mm-wiki 同款判据）
    profile: { name: data.name, email: data.email }
```

`tenant.anonymous_paths`（:135-141）与 `auth.anonymous_paths`（:150-165）各追加：

```yaml
      - "/sso/login"                              # form 登录端点（无 token 可言，必须匿名）
      - { path: "/sso/cas/*", one_layer: true }   # CAS 302 腿（带不了自定义头）
```

- [ ] **Step 6: 构建与冒烟**

Run: `cargo xtask bin && ./bin/oj build -d sample/src -o sample/dist && cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings && ./bin/oj test -c sample/config.yaml -d sample/src -t tests`
Expected: build 产出含 sso 模块；fmt/clippy/全部 L1 绿（未配上游，登录走不通属预期——e2e 在 Task 7 用 mock 上游验证）

- [ ] **Step 7: 提交**

```bash
git add sample/src/sso sample/config.yaml sample/dist
git commit -m "feat(sample): sso 模块——HTTP 表单 / CAS SSO 参考实现（JIT 键含 tenant 段，双匿名列表）

unix@vip.qq.com ai"
```

---

### Task 7: sso e2e（独立测试目标，Phase 3d）

**Files:**
- Create: `oj/tests/sso_e2e.rs`（无 `[[test]]` 声明，文件即目标）

**Interfaces:**
- Consumes: Task 4-6 全部产物；`oj/tests/oidc_e2e.rs` 的 helper 形态。
- 为什么要独立目标：oj-auth 插件进程级 `OnceLock` 首次 init 钉死 anonymous_paths，与既有目标同进程会互相污染（`oidc_e2e.rs:2-8` 头注释原文）。

- [ ] **Step 1: 先读** `oj/tests/oidc_e2e.rs` 全文（lock/sample/tmp_project/base_cfg/plugins_dir/copy_dir、起服务、HTTP 请求与 cookie 断言的确切写法、auth 段/jwt_secret 的配置方式、anonymous_paths 的 push 方式）。

- [ ] **Step 2: 写测试**（结构照抄 oidc_e2e.rs，两个用例）：

```rust
//! sso e2e：独立测试目标——oj-auth 插件 OnceLock 首次 init 钉死 anonymous_paths，
//! 与其它目标同进程会互相污染（oidc_e2e.rs 同款隔离）。mock 上游 = 项目内业务模块。

// helper（lock/sample/tmp_project/base_cfg/plugins_dir/copy_dir）照 oidc_e2e.rs 原样复制。

async fn start_with(tmp_files: &[(&str, &str)], sso_prov: HttpSsoProvider) -> (String, tempfile-ish guard) {
    // 1) 固定端口：TcpListener::bind("127.0.0.1:0") 取 port 后 drop（login_url 要进配置）
    // 2) tmp_project：拷 ["auth", "sso", "_platform"] 三模块 + 写 mocksso 夹具（见下）
    // 3) base_cfg + cfg.auth（jwt_secret 等，照 oidc_e2e）+ 插件严格清单 auth
    // 4) anonymous_paths（auth 与 tenant 两列表）push "/mocksso/*"
    // 5) cfg.http_sso = Some([("corp", sso_prov)].into())
    // 6) serve_cmd::start(cfg, &t, src, "/v1/api".into())
}

#[tokio::test(flavor = "current_thread")]
async fn sso_form_login_jits_and_issues_session() {
    // prov: kind=form, login_url=http://127.0.0.1:{port}/v1/api/mocksso/, allow_http=true,
    //       success_when={"message": ""}, profile={"name": "data.name"}
    // ① POST {base}/sso/login/ {"provider":"corp","username":"alice","password":"x"}
    //    → 200 {code:0,...} + set-cookie oj_sess
    // ② GET {base}/me/（探针模块，回 http.user）带 cookie → data.u.id 非空
    // ③ POST username=bob → 401 invalid credentials
    // ④ POST provider=nope → 400
}

#[tokio::test(flavor = "current_thread")]
async fn sso_cas2_callback_validates_ticket() {
    // prov: kind=cas2, cas_validate_url=http://127.0.0.1:{port}/v1/api/mocksso/,
    //       callback_url=http://127.0.0.1:{port}/v1/api/sso/cas/callback/?provider=corp, allow_http=true
    // ① GET {base}/sso/cas/callback/?provider=corp&ticket=ST-ok → 302 + set-cookie oj_sess
    // ② cookie 访问 /me/ → u.id 非空
    // ③ ticket=ST-bad → 401
}
```

mocksso 夹具（tmp_project 内 `src/mocksso/api.ts` + `manifest.yaml{name:mocksso,version:0.1.0}`）：

```ts
export default {
  post() {
    const ok = String(http.body?.username ?? "") === "alice";
    json.raw(JSON.stringify(ok
      ? { message: "", data: { name: "Alice", email: "a@x" } }
      : { message: "bad credentials" }));
  },
  get() {
    json.raw(String(http.query.ticket ?? "") === "ST-ok"
      ? "<cas:serviceResponse>\n  <cas:authenticationSuccess>\n    <cas:user>alice</cas:user>\n  </cas:authenticationSuccess>\n</cas:serviceResponse>"
      : "<cas:serviceResponse><cas:authenticationFailure code=\"INVALID_TICKET\">no</cas:authenticationFailure></cas:serviceResponse>");
  },
};
```

探针 `src/me/api.ts`（**不匿名**，验 cookie 会话）：`export default { get() { json.ok({ u: http.user }); } };` + manifest。

- [ ] **Step 3: 跑测试**

Run: `cargo test --release -p oj --test sso_e2e`
Expected: 2 用例 PASS（首次跑先确认因缺 Task 5/6 产物而失败——若此时它们已落地则直接绿）

- [ ] **Step 4: 全量 e2e 回归**

Run: `cargo test --release -p oj`
Expected: 全部目标绿（确认新目标没有污染既有目标）

- [ ] **Step 5: 提交**

```bash
git add oj/tests/sso_e2e.rs
git commit -m "test(sso): 独立 e2e 目标——form JIT 登录 + cas2 ticket 回校（mock 上游）

unix@vip.qq.com ai"
```

---

### Task 8: devkit 四件同步（Phase 4a）

**Files:**
- Modify: `docs/devkit/api-manual.md`
- Modify: `docs/devkit/scenarios.md`
- Modify: `docs/devkit/SKILL.md`
- Modify: `docs/devkit/README.md`

**Interfaces:**
- 消费 Task 1-7 的最终形态；产物经 `cargo xtask build` 归置 `bin/devkit/`，`cargo test --release -p xtask` 的 devkit 契约用例校验一致。

- [ ] **Step 1: api-manual.md**

1. 第 6 章全局表加两行：`db.dialect()`（口径注明：供 `_shared` 模块与诊断用，业务
   handler 分支方言 = 放弃跨引擎可移植承诺，慎用）与 `sso.providers`。
2. `oidc` 配置段小节（:2651 形态）后加「`sso` —— HTTP 表单/CAS SSO 提供者注册表
   （段存在即启用）」：`http_sso:` 字段表（kind/login_url/cas_validate_url/callback_url/
   user_field/pass_field/body_encoding/extra_fields/success_when/profile/allow_http）、
   判等语义（缺失/null 仅当期望 null 命中；其余 String 相等）、暴露面声明、JIT 键格式。
3. 错误/限制表新增行：`http sso not configured`；`unknown sso provider` 400；非 https
   装配期拒绝（allow_http 放行）；trigram <3 字符（search LIKE 回退）；sqlite FTS 腿仅
   整型自增 PK；`sql_guard: deny` 与引擎原生 FTS 互斥；mysql 同事务 FULLTEXT 可见性
   需实测；`search.ensureIndex` 禁入 db.tx；versions `version conflict` 409；`diff too
   large`；`oj schema diff` 对 `*_fts*`/`oj_ver*` 未登记表的 D002 噪音。

- [ ] **Step 2: scenarios.md**

场景 29「引擎原生全文检索（search 共享模块）」、场景 30「内容版本化（versions 共享模块）」，
四段式（何时抄我/配置+代码/验证/常见坑），代码取 Task 2/3 的 L1 测试形态；坑表覆盖：
ensureIndex 禁 tx、score 不跨引擎、mysql 幂等需查 information_schema、expect 撞 UNIQUE 409、
rollback 不删历史、purge 留给业务鉴权。

- [ ] **Step 3: SKILL.md**

路由行加「全文检索 → 场景 29」「内容版本化/回滚/diff → 场景 30」；陷阱速查加：
trigram 短词 LIKE 回退、ensureIndex 禁 tx、`version conflict` 409 的 UNIQUE 语义、
`sql_guard: deny` × FTS、CAS 交接红线（若场景 28 行未含全）。

- [ ] **Step 4: README.md**

scenarios 索引行 append 场景 29/30。

- [ ] **Step 5: 归置 + 契约测试**

Run: `cargo xtask build && cargo test --release -p xtask`
Expected: devkit 产物一致，契约用例绿

- [ ] **Step 6: 提交**

```bash
git add docs/devkit
git commit -m "docs(devkit): v0.1.61 四件同步——db.dialect/sso/search/versions 与限制表

unix@vip.qq.com ai"
```

---

### Task 9: CHANGELOG + 版本分界 + 全量门禁 + 标签（Phase 4b）

**Files:**
- Modify: `CHANGELOG.md`（顶部插入 v0.1.61 节）
- Modify: `oj/Cargo.toml`（version 0.1.60 → 0.1.61）

- [ ] **Step 1: bump**

`oj/Cargo.toml` version → `0.1.61`；`cargo xtask build`（同步 Cargo.lock + 归置产物）。

- [ ] **Step 2: CHANGELOG**

顶部加 `## v0.1.61（未打标签）`（打标签后改）：
feat(db) db.dialect + isTx；feat(versions)；feat(search)；feat(config/sso) http_sso 段 +
sso 全局；feat(sample) sso 模块；docs(devkit) 四件；fix 归组（如有）。

- [ ] **Step 3: 全量门禁**

Run: `cargo fmt && cargo fmt --check && cargo clippy --release --workspace --all-targets -- -D warnings && cargo test --release --workspace`
Expected: 全绿（这一步最长，V8 链接慢属正常）

- [ ] **Step 4: 版本分界提交 + 标签**

```bash
git add CHANGELOG.md oj/Cargo.toml Cargo.lock
git commit -m "chore(release): v0.1.61——HTTP/CAS SSO、引擎原生 FTS、内容版本化

unix@vip.qq.com ai"
git tag -a v0.1.61 -m "v0.1.61: HTTP/CAS SSO、引擎原生 FTS（search）、内容版本化（versions）、db.dialect()"
```

**不 push**：`git push origin main v0.1.61` 由用户确认后执行（外发动作）。

---

## 最终审计（执行完成后，用户要求的收尾）

对照 spec「验收」逐项核：
1. mock SSO form 登录 + JIT 建号（键含 tenant 段）+ 占位 hash 不可密码登录 + provider 未配置 400 + http:// 缺省拒 → sso_e2e 两用例 + validate 测试；
2. cas2 mock `/serviceValidate` 解析 + 302 交接红线 → sso_e2e cas2 用例；
3. search 三方言增删改检索一致性 → L1（sqlite）+ mysql/pg 腿标注 env-gated（`OJ_TEST_MYSQL_DSN`/`OJ_TEST_PG_DSN` 为后续基建，本期不阻塞）；
4. versions commit×5/get/diff/rollback/expect 409/CAS 去重/tx 回滚 → L1 四用例；
5. devkit 四件与实现一致 + xtask 契约绿；
6. `cargo fmt --check` / `clippy -D warnings` / `cargo test --release --workspace` 全绿；
7. CHANGELOG/版本/标签三处一致（未打标签状态在 CHANGELOG 标明）。
审计产出：简报（验收对照表 + 遗留项），必要时 `/code-review` 补一轮。
