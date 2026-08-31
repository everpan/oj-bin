import { mapMenu, paged, pageArgs, MENU_COLS } from "../_shared/map";

async function get(): Promise<void> {
  const { pageSize, current } = pageArgs();
  const rows: any[] = await db.query("select " + MENU_COLS + " from menu order by id", []);
  json.ok(paged(rows.map(mapMenu), pageSize, current));
}
get.route = "/menu-list";
export default { get };
