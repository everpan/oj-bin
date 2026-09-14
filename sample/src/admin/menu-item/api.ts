import { mapMenu, MENU_COLS } from "../_shared/map";

function bool(v: unknown): number { return v ? 1 : 0; }

async function post(): Promise<void> {
  const b = http.body as any;
  if (!b || !b.name) { json.fail(400, "name required"); return; }
  const now = Date.now();
  // insert + returning：单条 RETURNING 原子取回主键（pg/sqlite），列经白名单、
  // 值经绑定参数，裸 SQL 不再需要。
  const rows = await db.table("menu").insert({
    parent_id: Number(b.parentId) || 0,
    menu_type: b.menuType ?? 0,
    name: b.name,
    path: b.path ?? "",
    component: b.component ?? "",
    sort: b.order ?? null,
    icon: b.icon ?? "",
    current_active_menu: b.currentActiveMenu ?? "",
    iframe_link: b.iframeLink ?? "",
    keep_alive: bool(b.keepAlive),
    external_link: b.externalLink ?? "",
    hide_in_menu: bool(b.hideInMenu),
    ignore_access: bool(b.ignoreAccess),
    status: b.status ?? 1,
    create_time: now,
    update_time: now,
  }).returning(["id"]).run() as unknown as { id: number }[];
  json.ok({ id: rows[0].id, created: true });
}

async function put(): Promise<void> {
  const b = http.body as any;
  const id = Number(b?.id ?? 0);
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  if (!b || !b.name?.trim()) { json.fail(400, "name required"); return; }
  const now = Date.now();
  const n = await db.table("menu").update({
    parent_id: Number(b.parentId) || 0,
    menu_type: b.menuType ?? 0,
    name: b.name ?? "",
    path: b.path ?? "",
    component: b.component ?? "",
    sort: b.order ?? null,
    icon: b.icon ?? "",
    current_active_menu: b.currentActiveMenu ?? "",
    iframe_link: b.iframeLink ?? "",
    keep_alive: bool(b.keepAlive),
    external_link: b.externalLink ?? "",
    hide_in_menu: bool(b.hideInMenu),
    ignore_access: bool(b.ignoreAccess),
    status: b.status ?? 1,
    update_time: now,
  }).where({ field: "id", op: "eq", value: id }).run();
  if (n === 0) { json.fail(404, "no such menu"); return; }
  const rows: any[] = await db.table("menu")
    .select(MENU_COLS.split(",").map((c) => c.trim()))
    .where({ field: "id", op: "eq", value: id })
    .all();
  json.ok(mapMenu(rows[0]));
}

async function del(): Promise<void> {
  const id = Number(http.body);   // 裸 JSON 数字
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  let found = true;
  try {
    await db.tx(async (tx: any) => {
      await tx.table("role_menu").delete().where({ field: "menu_id", op: "eq", value: id }).run();
      const n = await tx.table("menu").delete().where({ field: "id", op: "eq", value: id }).run();
      if (n === 0) { found = false; throw new Error("no such menu"); }
    });
  } catch (_e) {
    if (!found) { json.fail(404, "no such menu"); return; }
    throw _e;
  }
  json.ok({ deleted: true });
}

post.route = "/menu-item";
put.route = "/menu-item";
del.route = "/menu-item";
export default { post, put, del };
