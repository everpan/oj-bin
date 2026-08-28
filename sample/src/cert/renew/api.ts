import { isAdmin, parseDays } from "../_shared/guard";

// POST {id, days?=365} ：读库中私钥重签，公钥与 private_pem 不变。
async function post(): Promise<void> {
  if (!isAdmin()) { json.fail(403, "admin only"); return; }
  const b = http.body as { id?: number; days?: number };
  const id = Number(b.id ?? 0);
  if (!Number.isInteger(id) || id <= 0) { json.fail(400, "id required"); return; }
  const days = parseDays(b.days);
  if (!days) { json.fail(400, "days must be integer 1..=3650"); return; }
  try {
    const rows = await db.query("select private_pem from certs where id = ?", [id]);
    if (!rows.length) { json.fail(404, "cert not found"); return; }
    const now = Math.floor(Date.now() / 1000);
    const exp = now + days * 86400;
    const certJws = await cert.renew(String(rows[0].private_pem), now, exp);
    const n = await db.exec(
      "update certs set cert_jws = ?, nbf = ?, exp = ?, updated_at = ? where id = ?",
      [certJws, now, exp, now, id],
    );
    if (!n) { json.fail(404, "cert not found"); return; }
    json.ok({ renewed: true, nbf: now, exp });
  } catch (e) {
    json.fail(500, String(e));
  }
}

export default { post };
