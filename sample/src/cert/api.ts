import { isAdmin, parseDays, statusOf } from "./_shared/guard";

// GET /v1/api/cert/ ：列表（不含 private_pem），附 status。
async function get(): Promise<void> {
  if (!isAdmin()) { json.fail(403, "admin only"); return; }
  try {
    const rows = await db.table("certs")
      .select(["id", "name", "note", "public_pem", "cert_jws", "nbf", "exp", "created_at", "updated_at"])
      .orderBy([{ field: "id", dir: "asc" }])
      .all();
    const now = Math.floor(Date.now() / 1000);
    json.ok(rows.map((r) => ({ ...r, status: statusOf(r.nbf as number, r.exp as number, now) })));
  } catch (e) {
    json.fail(500, String(e));
  }
}

// POST /v1/api/cert/ ：{name, note?, days?=365, bits?=2048} → 生成并入库。
async function post(): Promise<void> {
  if (!isAdmin()) { json.fail(403, "admin only"); return; }
  const b = http.body as { name?: string; note?: string; days?: number; bits?: number };
  const name = (b.name ?? "").trim();
  if (!name || name.length > 128) { json.fail(400, "name required (<=128 chars)"); return; }
  const days = parseDays(b.days);
  if (!days) { json.fail(400, "days must be integer 1..=3650"); return; }
  const bits = Number(b.bits ?? 2048);
  if (bits !== 2048 && bits !== 3072 && bits !== 4096) {
    json.fail(400, "bits must be 2048|3072|4096"); return;
  }
  try {
    const now = Math.floor(Date.now() / 1000);
    const exp = now + days * 86400;
    const m = await cert.generate(bits, now, exp);
    // insert + returning：单条 RETURNING 原子取 id（pg/sqlite 一次往返，
    // 规避 exec 后再查 last_insert_rowid() 的并发串号）。
    const rows = await db.table("certs").insert({
      name,
      note: (b.note ?? "").slice(0, 2000),
      public_pem: m.public_pem,
      private_pem: m.private_pem,
      cert_jws: m.cert_jws,
      nbf: now,
      exp,
      created_at: now,
      updated_at: now,
    }).returning(["id"]).run() as unknown as { id: number }[];
    json.ok({ id: rows[0]?.id, name });
  } catch (e) {
    json.fail(500, String(e));
  }
}

export default { get, post };
