import { mapRole, paged, pageArgs } from "../_shared/map";

async function get(): Promise<void> {
  const name = String(http.param("name", ""));
  const status = String(http.param("status", ""));
  const code = String(http.param("code", ""));
  const { pageSize, current } = pageArgs();
  let sql = "select id, name, code, status, remark, create_time, update_time from role";
  const conds: string[] = [];
  const params: unknown[] = [];
  if (name) { conds.push("name like ?"); params.push("%" + name + "%"); }
  if (status !== "") { conds.push("status = ?"); params.push(Number(status)); }
  if (code) { conds.push("code = ?"); params.push(code); }
  if (conds.length) sql += " where " + conds.join(" and ");
  sql += " order by id";
  const rows: any[] = await db.query(sql, params);
  const all = rows.map(mapRole);
  json.ok(paged(all, pageSize, current));
}
get.route = "/role-list";
export default { get };
