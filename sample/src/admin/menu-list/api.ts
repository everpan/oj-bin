import { mapMenu, paged, pageArgs } from "../_shared/map";

const COLS = "id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time";

async function get(): Promise<void> {
  const { pageSize, current } = pageArgs();
  const rows: any[] = await db.query("select " + COLS + " from menu order by id", []);
  json.ok(paged(rows.map(mapMenu), pageSize, current));
}
get.route = "/menu-list";
export default { get };
