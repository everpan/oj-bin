import { mapRole, paged, pageArgs } from "../_shared/map";

async function get(): Promise<void> {
  const name = String(http.param("name", ""));
  const status = String(http.param("status", ""));
  const code = String(http.param("code", ""));
  const { pageSize, current } = pageArgs();
  const conds: any[] = [];
  if (name) conds.push({ field: "name", op: "like", value: "%" + name + "%" });
  if (status !== "") conds.push({ field: "status", op: "eq", value: Number(status) });
  if (code) conds.push({ field: "code", op: "eq", value: code });
  let q = db.table("role")
    .select(["id", "name", "code", "status", "remark", "create_time", "update_time"])
    .orderBy([{ field: "id", dir: "asc" }]);
  if (conds.length) q = q.where({ and: conds });
  const rows: any[] = await q.all();
  const all = rows.map(mapRole);
  json.ok(paged(all, pageSize, current));
}
get.route = "/role-list";
export default { get };
