// 引擎原生 FTS 统一面（v0.1.61，spec 2026-10-11 §二）：sqlite FTS5(trigram) /
// mysql FULLTEXT(ngram，服务端 ≥5.7.6) / pg tsvector+GIN(PG12+)。
// ensureIndex = DDL（进程内每表一次；禁止在 db.tx 内调用——MySQL 的 DDL 会隐式提交
// 调用方事务，isTx 句柄直接拒绝）；index/remove = 业务表 upsert/delete（三引擎索引
// 随业务行自动维护，绝不直写 FTS 对象）；query = MATCH 裸 SQL + 绑定参数（标识符
// 白名单校验后拼接）。score 为引擎原始分，**不可跨引擎比较**。
// 占位符：pg $n，sqlite/mysql ?（sqlx Any 不转换）。

export type Dialect = "sqlite" | "mysql" | "postgres";
const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;
const ensured = new Set<string>();

function id(name: string, what: string): string {
  if (!IDENT.test(name)) throw new Error(`search: invalid ${what} ${JSON.stringify(name)}`);
  return name;
}
function likeEscape(s: string): string {
  return s.replace(/[\\%_]/g, (c) => "\\" + c);
}
/** mysql 应用层高亮：indexOf 截窗口（ngram 分词在索引侧，与原文无关）。 */
function mark(text: string, q: string): string {
  const i = text.indexOf(q);
  if (i < 0) return text.slice(0, 60);
  const s = Math.max(0, i - 30);
  return (s > 0 ? "…" : "") + text.slice(s, i) + "<mark>" + q + "</mark>"
    + text.slice(i + q.length, i + q.length + 30)
    + (i + q.length + 30 < text.length ? "…" : "");
}

/** 短词（<3 字符）跨方言统一 LIKE 回退：trigram/ngram/tsvector 对短词静默零命中，
 *  直接对基表 LIKE（复用 likeEscape 转义 %/_/\）。三方言共用，避免分支重复。 */
async function likeFallback(
  dbi: DBInstance,
  t: string,
  pk: string,
  fs: string[],
  q: string,
  limit: number,
  P: (i: number) => string,
  qw: (c: string) => string,
): Promise<Hit[]> {
  const n = [...q].length;
  if (!(n > 0 && n < 3)) return [];
  const where = fs.map((f) => `${qw(f)} LIKE ${P(0)} ESCAPE '\\'`).join(" OR ");
  const like = `%${likeEscape(q)}%`;
  return (await dbi.query(
    `SELECT ${qw(pk)} AS id, 1 AS score FROM ${qw(t)} WHERE ${where} LIMIT ${limit}`,
    fs.map(() => like),
  )).map((r: Record<string, unknown>) => ({ id: r.id, score: Number(r.score) }));
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
      await dbi.exec(
        `CREATE VIRTUAL TABLE IF NOT EXISTS "${t}_fts" USING fts5(${
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
      const hit = await dbi.query(
        `SELECT COUNT(*) AS n FROM information_schema.statistics
         WHERE table_schema = DATABASE() AND table_name = ${P(0)} AND index_name = ${P(1)}`,
        [t, idxName]);
      if (Number(hit[0].n) === 0) {   // ADD FULLTEXT 无 IF NOT EXISTS：先查再建
        await dbi.exec(`ALTER TABLE \`${t}\` ADD FULLTEXT INDEX \`${idxName}\` (${
          fs.join(", ")}) WITH PARSER ngram`, []);
      }
    } else {
      const hit = await dbi.query(
        `SELECT COUNT(*) AS n FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name::text = ${P(0)
        } AND column_name::text = 'tsv'`, [t]);
      if (Number(hit[0].n) === 0) {
        const expr = `to_tsvector('simple', ${
          fs.map((f) => `coalesce(${f}, '')`).join(" || ' ' || ")})`;
        await dbi.exec(
          `ALTER TABLE "${t}" ADD COLUMN IF NOT EXISTS tsv tsvector GENERATED ALWAYS AS (${expr}) STORED`, []);
        await dbi.exec(`CREATE INDEX IF NOT EXISTS "idx_${t}_tsv" ON "${t}" USING gin (tsv)`, []);
      }
    }
    pks.set(t, pk);
    fieldsOf.set(t, fs);
    ensured.add(key);
  }

  /** 索引一条 = 业务表 upsert（sqlite INSERT OR REPLACE / mysql ON DUPLICATE KEY /
   *  pg ON CONFLICT(pk)）。直写 FTS 对象会双份索引——禁止。 */
  async function index(table: string, row: Record<string, unknown>): Promise<void> {
    const t = id(table, "table");
    registered(t, "index");
    const pk = pks.get(t)!;
    const cols = Object.keys(row);
    if (!cols.length) throw new Error("search.index: empty row");
    cols.forEach((c) => id(c, "column"));
    const q = (c: string) => (d === "mysql" ? `\`${c}\`` : `"${c}"`);
    const colSql = cols.map(q).join(", ");
    const marks = cols.map((_, i) => P(i)).join(", ");
    const vals = [...Object.values(row)];
    if (d === "sqlite") {
      // INSERT OR REPLACE 在 recursive_triggers=OFF（默认）下隐式 DELETE 不触发 AFTER DELETE
      // 触发器 → FTS 虚表留旧词。改用 真 DELETE（触发 fts_ad）+ INSERT（触发 fts_ai），三方言安全。
      // 两语句非原子：sqlite 调用方须在 db.tx 内调用（oj-wiki searchIndexInTx 已满足）；
      // 回填路径（无事务）失败可重跑，幂等。
      await dbi.exec(`DELETE FROM ${q(t)} WHERE ${q(pk)} = ${P(0)}`, [row[pk]]);
      await dbi.exec(`INSERT INTO ${q(t)} (${colSql}) VALUES (${marks})`, vals);
    } else if (d === "mysql") {
      const set = cols.map((c) => `${q(c)} = VALUES(${q(c)})`).join(", ");
      await dbi.exec(`INSERT INTO ${q(t)} (${colSql}) VALUES (${marks}) ON DUPLICATE KEY UPDATE ${set}`, vals);
    } else {
      const set = cols.map((c) => `${c} = EXCLUDED.${c}`).join(", ");
      await dbi.exec(
        `INSERT INTO ${q(t)} (${colSql}) VALUES (${marks}) ON CONFLICT ("${pks.get(t)}") DO UPDATE SET ${set}`,
        vals);
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
    const num = (rows: Array<Record<string, unknown>>): Hit[] =>
      rows.map((r) => ({ id: r.id, score: Number(r.score) }));

    // 短词跨方言统一回退（基表 LIKE）——trigram/ngram/tsvector 对 <3 字符静默零命中。
    const fb = await likeFallback(dbi, t, pk, fs, q, limit, P, qw);
    if (fb.length) return fb;

    if (d === "sqlite") {
      // trigram 短语查询：整词加引号防 FTS 语法注入（双写引号转义）
      const phrase = `"${q.replace(/"/g, '""')}"`;
      const snip = opts.highlight
        ? ", " + fs.map((f, i) => `snippet("${t}_fts", ${i}, '<mark>', '</mark>', '…', 12) AS "s_${f}"`)
          .join(", ")
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
      // 高亮：一次批量取回全部命中行的原文（避免每命中一次 SELECT 的 N 查询塌方），
      // 再在应用层用 mark() 截取片段——与 sqlite/pg 一致，mysql 无内置 snippet。
      if (!rows.length) return num(rows);
      const ids = rows.map((r) => r.id);
      const placeholders = ids.map((_, i) => P(i)).join(", ");
      const src = await dbi.query(
        `SELECT ${qw(pk)} AS id, ${fs.map(qw).join(", ")} FROM ${qw(t)} WHERE ${qw(pk)} IN (${placeholders})`,
        ids,
      );
      const byId = new Map<unknown, Record<string, unknown>>(src.map((r) => [r.id, r]));
      return rows.map((r) => {
        const rec = byId.get(r.id) ?? {};
        const fragments: Record<string, string> = {};
        for (const f of fs) fragments[f] = mark(String(rec[f] ?? ""), q);
        return { id: r.id, score: Number(r.score), fragments };
      });
    }
    // postgres：ts_headline 是文档级 → 单一 fragments._doc
    const hl = opts.highlight
      ? `, ts_headline('simple', tsv, plainto_tsquery('simple', ${P(1)}), '<mark>', '</mark>', '…', 12) AS headline`
      : "";
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
