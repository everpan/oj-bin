// admin 模块共享映射：DB 行（snake_case）→ 前端契约（camelCase）。
// 数据形状以 react-antd-admin fake mock 实际数据为准（布尔/分页字段）。

export function mapRole(r: any): any {
  return {
    id: r.id,
    name: r.name,
    code: r.code,
    status: r.status,
    remark: r.remark ?? "",
    createTime: r.create_time,
    updateTime: r.update_time,
  };
}

export function mapMenu(m: any): any {
  return {
    id: m.id,
    parentId: m.parent_id === 0 ? "" : m.parent_id,
    menuType: m.menu_type,
    name: m.name,
    path: m.path ?? "",
    component: m.component ?? "",
    order: m.sort ?? undefined,
    icon: m.icon ?? "",
    currentActiveMenu: m.current_active_menu ?? "",
    iframeLink: m.iframe_link ?? "",
    keepAlive: !!m.keep_alive,
    externalLink: m.external_link ?? "",
    hideInMenu: !!m.hide_in_menu,
    ignoreAccess: !!m.ignore_access,
    status: m.status,
    createTime: m.create_time,
    updateTime: m.update_time,
  };
}

export function paged(all: any[], pageSize: number, current: number): any {
  return {
    list: all.slice((current - 1) * pageSize, current * pageSize),
    total: all.length,
    pageSize,
    current,
  };
}

export function pageArgs(): { pageSize: number; current: number } {
  return {
    pageSize: Number(http.param("pageSize", 10)) || 10,
    current: Number(http.param("current", 1)) || 1,
  };
}
