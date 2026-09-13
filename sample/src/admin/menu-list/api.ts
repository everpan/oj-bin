import { mapMenu, paged, pageArgs, MENU_COLS } from "../_shared/map";

async function get(): Promise<void> {
  const { pageSize, current } = pageArgs();
  const rows: any[] = await db.table("menu")
    .select(MENU_COLS.split(",").map((c) => c.trim()))
    .orderBy([{ field: "id", dir: "asc" }])
    .all();
  json.ok(paged(rows.map(mapMenu), pageSize, current));
}
get.route = "/menu-list";
export default { get };
