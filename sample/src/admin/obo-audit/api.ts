// 换签审计查询端点（spec 2026-11-11 P2）：分页列表 + CSV 导出。
// 审计行由 auth/obo 端点写入（每次换签调用一行，成败均记）。
import { isAdmin, denyNonAdmin } from "../_shared/guard";

async function get(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const svc = String(http.param("svc", ""));
  const ok = String(http.param("ok", ""));
  const conds: any[] = [];
  if (svc) conds.push({ field: "svc", op: "like", value: "%" + svc + "%" });
  if (ok === "0" || ok === "1") conds.push({ field: "ok", op: "eq", value: Number(ok) });
  let q = db.table("oj_obo_audit")
    .select(["id", "svc", "provider", "external_sub", "user_id", "ok", "reason", "ts"])
    .orderBy([{ field: "id", dir: "desc" }]);
  if (conds.length) q = q.where({ and: conds });
  const rows: any[] = await q.all();
  const pageSize = Number(http.param("pageSize", 50)) || 50;
  const current = Number(http.param("current", 1)) || 1;
  json.ok({
    list: rows.slice((current - 1) * pageSize, current * pageSize),
    total: rows.length,
    pageSize,
    current,
  });
}

const exportCsv = {
  async get(): Promise<void> {
    if (!isAdmin()) return denyNonAdmin();
    const rows: any[] = await db.table("oj_obo_audit")
      .select(["id", "svc", "provider", "external_sub", "user_id", "ok", "reason", "ts"])
      .orderBy([{ field: "id", dir: "desc" }])
      .all();
    const s = json.stream({ contentType: "text/csv" });
    s.write("id,svc,provider,external_sub,user_id,ok,reason,ts\n");
    for (const r of rows) {
      s.write(`${r.id},${r.svc},${r.provider},${r.external_sub},${r.user_id},${r.ok},"${String(r.reason ?? "").replace(/"/g, '""')}",${r.ts}\n`);
    }
    s.end();
  },
};
exportCsv.route = "export";

export default { get, exportCsv };
