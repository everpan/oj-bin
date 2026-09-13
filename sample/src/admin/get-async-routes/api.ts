// 动态路由下发：http.user.roles → role.code → role_menu → menu（menu_type 0/1/2）组树。
// 响应形状对齐 react-antd-admin：{ path, component?, handle: {title, icon?, order?, ...}, children? }

async function get(): Promise<void> {
  const u = http.user;
  if (!u) { json.fail(401, "unauthorized"); return; }
  const roles: string[] = u.roles ?? [];
  let menuIds: number[] = [];
  if (roles.length) {
    const roleRows: any[] = await db.table("role")
      .select(["id"])
      .where({ field: "code", op: "in", value: roles })
      .all();
    const rids = roleRows.map((r) => r.id);
    if (rids.length) {
      const binds: any[] = await db.table("role_menu")
        .select(["menu_id"])
        .where({ field: "role_id", op: "in", value: rids })
        .all();
      menuIds = binds.map((b) => b.menu_id);
    }
  }
  if (!menuIds.length) { json.ok([]); return; }
  const rows: any[] = await db.table("menu")
    .select(["id", "parent_id", "name", "path", "component", "sort", "icon", "keep_alive", "iframe_link", "external_link"])
    .where({ and: [
      { field: "menu_type", op: "in", value: [0, 1, 2] },
      { field: "id", op: "in", value: menuIds },
    ] })
    .orderBy([{ field: "id", dir: "asc" }])
    .all();

  const nodes = new Map<number, any>();
  for (const m of rows) {
    const handle: any = { title: m.name };
    if (m.icon) handle.icon = m.icon;
    if (m.sort != null) handle.order = m.sort;
    if (m.keep_alive) handle.keepAlive = true;
    if (m.iframe_link) handle.iframeLink = m.iframe_link;
    if (m.external_link) handle.externalLink = m.external_link;
    const node: any = { path: m.path ?? "", handle };
    if (m.component) node.component = m.component;
    nodes.set(m.id, node);
  }
  const roots: any[] = [];
  for (const m of rows) {
    const node = nodes.get(m.id);
    const parent = nodes.get(m.parent_id);
    if (parent) {
      if (!parent.children) parent.children = [];
      parent.children.push(node);
    } else {
      roots.push(node);
    }
  }
  json.ok(roots);
}
get.route = "/get-async-routes";
export default { get };
