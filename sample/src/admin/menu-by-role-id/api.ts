async function get(): Promise<void> {
  const id = Number(http.param("id", 0));
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  const rows: any[] = await db.table("role_menu")
    .select(["menu_id"])
    .where({ field: "role_id", op: "eq", value: id })
    .orderBy([{ field: "menu_id", dir: "asc" }])
    .all();
  json.ok(rows.map((r) => r.menu_id));
}
get.route = "/menu-by-role-id";
export default { get };
