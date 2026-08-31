async function get(): Promise<void> {
  const id = Number(http.param("id", 0));
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  const rows: any[] = await db.query(
    "select menu_id from role_menu where role_id = ? order by menu_id", [id]);
  json.ok(rows.map((r) => r.menu_id));
}
get.route = "/menu-by-role-id";
export default { get };
