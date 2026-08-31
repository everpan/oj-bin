import { mapMenu } from "../_shared/map";

function bool(v: unknown): number { return v ? 1 : 0; }

async function post(): Promise<void> {
  const b = http.body as any;
  if (!b || !b.name) { json.fail(400, "name required"); return; }
  const now = Date.now();
  const rows: any[] = await db.query(
    "insert into menu (parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) returning id",
    [Number(b.parentId) || 0, b.menuType ?? 0, b.name, b.path ?? "", b.component ?? "",
     b.order ?? null, b.icon ?? "", b.currentActiveMenu ?? "", b.iframeLink ?? "",
     bool(b.keepAlive), b.externalLink ?? "", bool(b.hideInMenu), bool(b.ignoreAccess),
     b.status ?? 1, now, now],
  );
  json.ok({ id: rows[0].id, created: true });
}

async function put(): Promise<void> {
  const b = http.body as any;
  const id = Number(b?.id ?? 0);
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  const now = Date.now();
  const n = await db.exec(
    "update menu set parent_id = ?, menu_type = ?, name = ?, path = ?, component = ?, sort = ?, icon = ?, current_active_menu = ?, iframe_link = ?, keep_alive = ?, external_link = ?, hide_in_menu = ?, ignore_access = ?, status = ?, update_time = ? where id = ?",
    [Number(b.parentId) || 0, b.menuType ?? 0, b.name ?? "", b.path ?? "", b.component ?? "",
     b.order ?? null, b.icon ?? "", b.currentActiveMenu ?? "", b.iframeLink ?? "",
     bool(b.keepAlive), b.externalLink ?? "", bool(b.hideInMenu), bool(b.ignoreAccess),
     b.status ?? 1, now, id],
  );
  if (n === 0) { json.fail(404, "no such menu"); return; }
  const rows: any[] = await db.query(
    "select id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time from menu where id = ?",
    [id]);
  json.ok(mapMenu(rows[0]));
}

async function del(): Promise<void> {
  const id = Number(http.body);   // 裸 JSON 数字
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  await db.exec("delete from role_menu where menu_id = ?", [id]);
  const n = await db.exec("delete from menu where id = ?", [id]);
  if (n === 0) { json.fail(404, "no such menu"); return; }
  json.ok({ deleted: true });
}

post.route = "/menu-item";
put.route = "/menu-item";
del.route = "/menu-item";
export default { post, put, del };
