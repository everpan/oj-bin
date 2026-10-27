// 内容版本化原语（v0.1.61，spec 2026-10-11 §三）：CAS 内容表 + 修订表 + 行级 diff。
// 事务：语句经 op 层 resolve_target 自动路由进调用方在同名库上开启的 db.tx——在事务内
// 调用即同事务（blob 轴不参与事务，故内容存本库 oj_ver_content）。
// 并发：oj_versions 的 UNIQUE(entity_type, entity_id, version) 兜底；不带 expect =
// last-write-wins（UNIQUE 仍保证版本号不重）。
// 注意：sha256 是 CHAR(64) 主键（MySQL TEXT 不可作 PK，建表走 _platform/migrations），
// 内容去重按方言 INSERT IGNORE / OR IGNORE / ON CONFLICT DO NOTHING。

const DUP = /unique|duplicate/i;

function conflict(msg: string): Error & { status: number } {
  const e = new Error(msg) as Error & { status: number };
  e.status = 409;
  return e;
}

export interface CommitMeta { author?: string; message?: string; expect?: number; }
export interface CommitResult { version: number; sha256: string; }
export interface HistoryEntry {
  version: number;
  author: string | null;
  message: string | null;
  created_at: number;
}
export interface DiffLine { type: "ctx" | "add" | "del"; text: string; }

/** 行级 LCS；先修剪公共前后缀，剩余格子超预算（4M 格 ≈ 16MB DP 矩阵）即拒绝。 */
export function lineDiff(a: string, b: string): DiffLine[] {
  const A0 = a.split("\n"), B0 = b.split("\n");
  let s = 0;
  while (s < A0.length && s < B0.length && A0[s] === B0[s]) s++;
  let ea = A0.length, eb = B0.length;
  while (ea > s && eb > s && A0[ea - 1] === B0[eb - 1]) { ea--; eb--; }
  const head = A0.slice(0, s).map((text) => ({ type: "ctx" as const, text }));
  const tail = A0.slice(ea).map((text) => ({ type: "ctx" as const, text }));
  const A = A0.slice(s, ea), B = B0.slice(s, eb);
  if (A.length * B.length > 4_000_000) {
    throw new Error("diff too large (line budget exceeded)");
  }
  const w = B.length + 1;
  const dp = new Int32Array((A.length + 1) * w);
  for (let i = A.length - 1; i >= 0; i--) {
    for (let j = B.length - 1; j >= 0; j--) {
      dp[i * w + j] = A[i] === B[j]
        ? dp[(i + 1) * w + j + 1] + 1
        : Math.max(dp[(i + 1) * w + j], dp[i * w + j + 1]);
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
  // T9：未知方言（不在 builder-known 子集）一期 raw-only，内容版本化原语显式拒绝，
  // 结构化为 OJ_DB_UNSUPPORTED（与宿主门控同形），禁止静默落 pg 分支生成错误 SQL。
  // 注意：`dbi.dialect()` 在宿主侧**已不再**对未知方言 fail-loud（开放契约下它原样返回
  // 插件声明的方言名），所以本段是这一层唯一的防线，不能省。
  const SUPPORTED = ["sqlite", "mysql", "postgres"];
  if (!SUPPORTED.includes(d)) {
    const e = new Error(
      `versions is not supported on dialect "${d}" yet. See docs/db-guide.md 方言能力矩阵.`,
    ) as Error & { code: string; data: unknown };
    e.code = "OJ_DB_UNSUPPORTED";
    e.data = { capability: "versions", dialect: d };
    throw e;
  }
  const P = (n: number) => (d === "postgres" ? `$${n + 1}` : "?");
  const insIgnore = d === "sqlite" ? "INSERT OR IGNORE INTO"
    : d === "mysql" ? "INSERT IGNORE INTO"
    : "INSERT INTO";
  const pgTail = d === "postgres" ? " ON CONFLICT (sha256) DO NOTHING" : "";

  /** 提交一次修订：CAS 落内容（命中即略）→ 读 head →（可选 expect 比对）→ 插修订行。
   *  并发双 commit 时后插者在 INSERT 处撞 UNIQUE → 统一映射 409 version conflict
   *  （三引擎重复键文案不同，按 /unique|duplicate/i 识别；不做二次 max 复核）。 */
  async function commit(entityType: string, entityId: string, content: string,
                        meta: CommitMeta = {}): Promise<CommitResult> {
    const sha = crypto.sha256Hex(content);
    await dbi.exec(
      `${insIgnore} oj_ver_content (sha256, content) VALUES (${P(0)}, ${P(1)})${pgTail}`,
      [sha, content]);
    const rows = await dbi.query(
      `SELECT COALESCE(MAX(version), 0) AS v FROM oj_versions
       WHERE entity_type = ${P(0)} AND entity_id = ${P(1)}`,
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
      if (DUP.test(String(e))) throw conflict("version conflict");
      throw e;
    }
    return { version: head + 1, sha256: sha };
  }

  /** 取正文；"head" = max(version)（先单独取号，避免同一 SQL 里占位符与参数错位）。 */
  async function get(entityType: string, entityId: string,
                     version: number | "head"): Promise<string> {
    let v = version;
    if (version === "head") {
      const head = await dbi.query(
        `SELECT COALESCE(MAX(version), 0) AS v FROM oj_versions
         WHERE entity_type = ${P(0)} AND entity_id = ${P(1)}`, [entityType, entityId]);
      v = Number(head[0].v);
    }
    const rows = await dbi.query(
      `SELECT c.content AS content FROM oj_versions v JOIN oj_ver_content c ON c.sha256 = v.sha256
       WHERE v.entity_type = ${P(0)} AND v.entity_id = ${P(1)} AND v.version = ${P(2)}`,
      [entityType, entityId, v]);
    if (!rows.length) throw new Error(`version not found: ${entityType}/${entityId}@${version}`);
    return String(rows[0].content);
  }

  async function history(entityType: string, entityId: string,
                         opts: { limit?: number; offset?: number } = {}): Promise<HistoryEntry[]> {
    const limit = Math.max(1, Number(opts.limit ?? 20));
    const offset = Math.max(0, Number(opts.offset ?? 0));
    // SELECT 列与 HistoryEntry 一一对应（Row 是索引签名类型，接口须经 unknown 断言）。
    return await dbi.query(
      `SELECT version, author, message, created_at FROM oj_versions
       WHERE entity_type = ${P(0)} AND entity_id = ${P(1)}
       ORDER BY version DESC LIMIT ${limit} OFFSET ${offset}`,
      [entityType, entityId]) as unknown as HistoryEntry[];
  }

  async function diff(entityType: string, entityId: string, va: number, vb: number): Promise<DiffLine[]> {
    return lineDiff(await get(entityType, entityId, va), await get(entityType, entityId, vb));
  }

  /** 回滚 = 旧内容重放为新版本（不删历史，审计友好；清理留给业务侧 purge）。 */
  async function rollback(entityType: string, entityId: string, version: number,
                          meta: CommitMeta = {}): Promise<CommitResult> {
    return commit(entityType, entityId, await get(entityType, entityId, version), meta);
  }

  return { commit, get, history, diff, rollback };
}
