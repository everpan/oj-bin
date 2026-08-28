import { isAdmin, statusOf } from "../_shared/guard";

// GET ?id= ：详情（含 private_pem，管理员取用）。
async function get(): Promise<void> {
  if (!isAdmin()) { json.fail(403, "admin only"); return; }
  const id = Number(http.param("id", 0));
  if (!Number.isInteger(id) || id <= 0) { json.fail(400, "id required"); return; }
  try {
    const rows = await db.query(
      "select id, name, note, public_pem, private_pem, cert_jws, nbf, exp, created_at, updated_at from certs where id = ?",
      [id],
    );
    if (!rows.length) { json.fail(404, "cert not found"); return; }
    const r = rows[0];
    json.ok({ ...r, status: statusOf(r.nbf as number, r.exp as number, Math.floor(Date.now() / 1000)) });
  } catch (e) {
    json.fail(500, String(e));
  }
}

// PATCH {id, note} ：改备注。
async function patch(): Promise<void> {
  if (!isAdmin()) { json.fail(403, "admin only"); return; }
  const b = http.body as { id?: number; note?: string };
  const id = Number(b.id ?? 0);
  if (!Number.isInteger(id) || id <= 0) { json.fail(400, "id required"); return; }
  try {
    const n = await db.exec("update certs set note = ?, updated_at = ? where id = ?", [
      String(b.note ?? "").slice(0, 2000),
      Math.floor(Date.now() / 1000),
      id,
    ]);
    if (!n) { json.fail(404, "cert not found"); return; }
    json.ok({ updated: true });
  } catch (e) {
    json.fail(500, String(e));
  }
}

// DELETE ?id= ：删除。
async function del(): Promise<void> {
  if (!isAdmin()) { json.fail(403, "admin only"); return; }
  const id = Number(http.param("id", 0));
  if (!Number.isInteger(id) || id <= 0) { json.fail(400, "id required"); return; }
  try {
    const n = await db.exec("delete from certs where id = ?", [id]);
    if (!n) { json.fail(404, "cert not found"); return; }
    json.ok({ deleted: true });
  } catch (e) {
    json.fail(500, String(e));
  }
}

export default { get, patch, del };
