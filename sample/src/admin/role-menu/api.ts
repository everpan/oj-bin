async function get(): Promise<void> {
  const rows: any[] = await db.query("select id, parent_id, menu_type, name from menu order by id", []);
  json.ok(rows.map((m) => {
    const item: any = { id: m.id, menuType: m.menu_type, name: m.name };
    if (m.parent_id !== 0) item.parentId = m.parent_id;
    return item;
  }));
}
get.route = "/role-menu";
export default { get };
