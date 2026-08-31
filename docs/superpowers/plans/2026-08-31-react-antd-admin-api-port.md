# react-antd-admin API 移植 + api-manual 数据层补全 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `sample/` 下以新模块 `admin` 实现 react-antd-admin 前端的 12 个自建端点（登录三件套复用内置 `/auth/*`），并在 `docs/devkit/api-manual.md` 补入模块数据层章节。

**Architecture:** 单模块 `sample/src/admin/` 拥有 4 张表（role/menu/role_menu/notification），每个 handler 挂 `.route = "/<前端路径>"` 根挂载使 URL 与前端逐字一致；DB 列 snake_case、响应 camelCase，映射集中在 `_shared/map.ts`；数据层走 schema.yaml + migrations + seed.sql 既有规范。

**Tech Stack:** oj TS handler（deno_core/V8）、sqlite（默认库）、L1 `oj test` + L2 vitest。

**Spec:** `docs/superpowers/specs/2026-08-31-react-antd-admin-api-port-design.md`（已批准）。

## Global Constraints

- oj 信封 `{code,msg,data}`（`code=0` 成功）——不改前端信封，不模拟 `{result,message,success}`。
- SQL 红线：标识符只来自静态 SQL 字面量或 `db.table()` 构造器；值一律绑定参数（sqlite 占位符 `?`）。
- DELETE 方法名是 `del`；`.route` 用顶层标准赋值写法 `fn.route = "…"`（build 剥离只认这种）。
- sample 开了 auth + tenant：所有新端点非匿名，测试必须带 `Authorization: Bearer` 和 `X-TENANT-ID` 头。
- sample `ownership_guard: deny`：读 `users` 表必须 manifest 声明 `deps: { _platform: "^0.1.0" }`。
- seed.sql 语句按 `;` 切分——语句内不得含分号字面量；一律 `INSERT OR IGNORE`。
- 取 id 用 `insert … returning id`（不用 `last_insert_rowid`，跨请求竞争——见 9dc9d77）。
- 数据形状以 fake mock 实际数据为准（`home/line` 返回 number[]、`menu-by-role-id` 返回 number[]、分页带 `pageSize`、menu 布尔字段输出 boolean）。
- 登录三件套**不写**：复用内置 `POST {base}/auth/{login,refresh,logout}`，前端适配字段名。

---

### Task 1: admin 模块骨架与数据层

**Files:**
- Create: `sample/src/admin/manifest.yaml`
- Create: `sample/src/admin/schema.yaml`
- Create: `sample/src/admin/migrations/0001__init.sql`
- Create: `sample/src/admin/seed.sql`

**Interfaces:**
- Produces: 表 `role(id,name,code,status,remark,create_time,update_time)`、
  `menu(id,parent_id,menu_type,name,path,component,sort,icon,current_active_menu,iframe_link,keep_alive,external_link,hide_in_menu,ignore_access,status,create_time,update_time)`、
  `role_menu(id,role_id,menu_id)`、`notification(id,avatar,date,is_read,message,title)`；
  种子：角色 1=admin/2=common，菜单 id 100–107，role_menu 仅角色 1 绑定 100–107，通知 4 条。

- [ ] **Step 1: 写 manifest.yaml**

```yaml
name: "admin"
desc: "react-antd-admin 前端接口的后端移植：角色/菜单/通知/首页图表/动态路由"
version: "0.1.0"
tables:            # 与 schema.yaml 双向一致（S005）
  - role
  - menu
  - role_menu
  - notification
deps:              # user-info 读 _platform.users（ownership_guard: deny 下必须声明）
  _platform: "^0.1.0"
```

- [ ] **Step 2: 写 schema.yaml**

```yaml
# 声明式表结构：归属图 + SchemaRegistry 列白名单的源（规范见 docs/migration.md）。
# 列类型最小集：integer | bigint | text | boolean | double | blob
tables:
  role:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      name: { type: text, null: false }
      code: { type: text, null: false }
      status: { type: integer, null: false }   # 1 启用 0 停用
      remark: { type: text }
      create_time: { type: bigint, null: false }   # ms 时间戳
      update_time: { type: bigint, null: false }
  menu:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      parent_id: { type: integer, null: false }    # 根为 0
      menu_type: { type: integer, null: false }    # 0 菜单 1 iframe 2 外链 3 按钮
      name: { type: text, null: false }
      path: { type: text }
      component: { type: text }
      sort: { type: integer }                      # 前端字段名 order（SQL 关键字避让）
      icon: { type: text }
      current_active_menu: { type: text }
      iframe_link: { type: text }
      keep_alive: { type: integer, null: false }   # 0/1，输出映射 boolean
      external_link: { type: text }
      hide_in_menu: { type: integer, null: false }
      ignore_access: { type: integer, null: false }
      status: { type: integer, null: false }
      create_time: { type: bigint, null: false }
      update_time: { type: bigint, null: false }
  role_menu:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      role_id: { type: integer, null: false }
      menu_id: { type: integer, null: false }
  notification:
    pk: id
    columns:
      id: { type: integer, autoincrement: true }
      avatar: { type: text }
      date: { type: text, null: false }
      is_read: { type: integer, null: false }      # 0/1，输出映射 boolean
      message: { type: text }
      title: { type: text, null: false }
```

- [ ] **Step 3: 写 migrations/0001__init.sql**（与 schema.yaml 对齐；每条语句一行、无内嵌分号）

```sql
CREATE TABLE IF NOT EXISTS role (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, code TEXT NOT NULL, status INTEGER NOT NULL, remark TEXT, create_time INTEGER NOT NULL, update_time INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS menu (id INTEGER PRIMARY KEY AUTOINCREMENT, parent_id INTEGER NOT NULL, menu_type INTEGER NOT NULL, name TEXT NOT NULL, path TEXT, component TEXT, sort INTEGER, icon TEXT, current_active_menu TEXT, iframe_link TEXT, keep_alive INTEGER NOT NULL, external_link TEXT, hide_in_menu INTEGER NOT NULL, ignore_access INTEGER NOT NULL, status INTEGER NOT NULL, create_time INTEGER NOT NULL, update_time INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS role_menu (id INTEGER PRIMARY KEY AUTOINCREMENT, role_id INTEGER NOT NULL, menu_id INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS notification (id INTEGER PRIMARY KEY AUTOINCREMENT, avatar TEXT, date TEXT NOT NULL, is_read INTEGER NOT NULL, message TEXT, title TEXT NOT NULL);
```

- [ ] **Step 4: 写 seed.sql**（数据沿用 react-antd-admin `fake/system.fake.ts` / `fake/notification.fake.ts`）

```sql
INSERT OR IGNORE INTO role (id, name, code, status, remark, create_time, update_time) VALUES (1, '超级管理员', 'admin', 1, '超级管理员拥有最高权限', 1729752330782, 1729752330782);
INSERT OR IGNORE INTO role (id, name, code, status, remark, create_time, update_time) VALUES (2, '普通角色', 'common', 1, '普通角色拥有部分权限', 1729752330782, 1729752330782);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (100, 0, 0, 'system:menu.system', '/system', '/system', 100, 'SettingOutlined', '', '', 1, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (101, 100, 0, 'system:menu.user', '/system/user', '/system/user', NULL, 'UserOutlined', '', '', 1, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (102, 100, 0, 'system:menu.role', '/system/role', '/system/role', NULL, 'TeamOutlined', '', '', 1, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (103, 100, 0, 'system:menu.menu', '/system/menu', '/system/menu', NULL, 'MenuOutlined', '', '', 1, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (104, 100, 0, 'system:menu.dept', '/system/dept', '/system/dept', NULL, 'ApartmentOutlined', '', '', 1, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (105, 104, 3, 'common.add', '', '', NULL, '', '', '', 0, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (106, 104, 3, 'common.edit', '', '', NULL, '', '', 0, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO menu (id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time) VALUES (107, 104, 3, 'common.delete', '', '', NULL, '', '', 0, '', 0, 0, 1, 1737023155965, 1737023164653);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (1, 1, 100);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (2, 1, 101);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (3, 1, 102);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (4, 1, 103);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (5, 1, 104);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (6, 1, 105);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (7, 1, 106);
INSERT OR IGNORE INTO role_menu (id, role_id, menu_id) VALUES (8, 1, 107);
INSERT OR IGNORE INTO notification (id, avatar, date, is_read, message, title) VALUES (1, 'https://avatar.vercel.sh/vercel.svg?text=VC', '3 小时前', 1, '描述信息描述信息描述信息', '收到了 14 份新周报');
INSERT OR IGNORE INTO notification (id, avatar, date, is_read, message, title) VALUES (2, 'https://avatar.vercel.sh/1', '刚刚', 0, '描述信息描述信息描述信息', 'Tom 回复了你');
INSERT OR IGNORE INTO notification (id, avatar, date, is_read, message, title) VALUES (3, 'https://avatar.vercel.sh/2', '2024-10-10', 0, '描述信息描述信息描述信息', 'Jack 评论了你');
INSERT OR IGNORE INTO notification (id, avatar, date, is_read, message, title) VALUES (4, 'https://avatar.vercel.sh/Jack', '1 天前', 0, '描述信息描述信息描述信息', '代办提醒');
```

- [ ] **Step 5: 验证结构检查全绿**

Run: `cargo run -p oj -- build --check -d sample/src -o sample/dist`
（若旗标不符先 `cargo run -p oj -- build --help` 核对；`-o` 在 --check 下不落盘）
Expected: admin 模块 S001–S007 检查通过，无表归属/双向一致报错。

- [ ] **Step 6: dev 启动冒烟（migrate_on_start=auto 建表 + 重放 seed）**

Run: `cargo run -p oj -- server -c sample/config.yaml --api-path sample/src`（后台起，5 秒后 `curl -s http://localhost:9778/v1/api/health` 看到证书状态 JSON 即杀掉）
Expected: 启动打印模块清单含 `admin`；`sqlite3 sample/db.sqlite "select count(*) from role"` 为 2。

- [ ] **Step 7: Commit**

```bash
git add sample/src/admin/
git commit -m "feat(sample): admin 模块骨架——4 表 schema/migrations/seed（react-antd-admin 移植）"
```

---

### Task 2: role 域 4 端点 + 共享映射

**Files:**
- Create: `sample/src/admin/_shared/map.ts`
- Create: `sample/src/admin/role-list/api.ts`
- Create: `sample/src/admin/role-item/api.ts`
- Create: `sample/src/admin/role-menu/api.ts`
- Create: `sample/src/admin/menu-by-role-id/api.ts`
- Test: `sample/tests/admin.test.ts`

**Interfaces:**
- Produces: `mapRole(row): {id,name,code,status,remark,createTime,updateTime}`、
  `mapMenu(row): MenuItem 形状（camelCase，boolean 映射，parentId 根为 ""，order←sort）`、
  `paged(all, pageSize, current): {list,total,pageSize,current}`（all 为全量数组，内部切片）、
  `pageArgs(): {pageSize, current}`——Task 3 复用以上全部。
- 路由：`GET /role-list`、`POST|PUT|DELETE /role-item`、`GET /role-menu`、`GET /menu-by-role-id`。

- [ ] **Step 1: 先写 L1 失败测试 `sample/tests/admin.test.ts`（role 域部分）**

```ts
// L1 admin 模块集成测试：react-antd-admin 接口移植的端到端契约（auth + tenant 全开）。

describe("admin role", () => {
  it("GET /role-list → 分页包装 + 种子角色（camelCase）", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/role-list", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    expect(r.status).toBe(200);
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.total).toBe(2);
    expect(body.data.pageSize).toBe(10);
    expect(body.data.current).toBe(1);
    expect(body.data.list[0].code).toBe("admin");
    expect(body.data.list[0].createTime).toBe(1729752330782);
  });

  it("GET /role-list 过滤 name → 只剩匹配项", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/role-list?name=" + encodeURIComponent("普通"), {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.data.total).toBe(1);
    expect(body.data.list[0].code).toBe("common");
  });

  it("role-item POST→PUT→DELETE 闭环（DELETE body 为裸数字）", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const created = JSON.parse((await client.post("/role-item", {
      headers: h, body: JSON.stringify({ name: "运营", code: "ops", status: 1, remark: "r" }),
    })).body);
    expect(created.code).toBe(0);
    const id = created.data.id;
    expect(id > 0).toBeTruthy();

    const updated = JSON.parse((await client.put("/role-item", {
      headers: h, body: JSON.stringify({ id, name: "运营2", code: "ops", status: 0, remark: "r2" }),
    })).body);
    expect(updated.data.name).toBe("运营2");
    expect(updated.data.status).toBe(0);

    const deleted = JSON.parse((await client.del("/role-item", {
      headers: h, body: String(id),
    })).body);
    expect(deleted.code).toBe(0);

    const gone = JSON.parse((await client.del("/role-item", {
      headers: h, body: String(id),
    })).body);
    expect(gone.code).toBe(404);
  });

  it("GET /role-menu → 精简菜单树（根无 parentId）", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/role-menu", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.data.length).toBe(8);
    expect(body.data[0].id).toBe(100);
    expect(body.data[0].parentId).toBe(undefined);
    expect(body.data[1].parentId).toBe(100);
  });

  it("GET /menu-by-role-id?id=1 → 8 个 id；id=2 → []", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const all = JSON.parse((await client.get("/menu-by-role-id?id=1", { headers: h })).body);
    expect(all.data.length).toBe(8);
    expect(all.data[0]).toBe(100);
    const none = JSON.parse((await client.get("/menu-by-role-id?id=2", { headers: h })).body);
    expect(none.data.length).toBe(0);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src`
Expected: 新用例 FAIL（404 no route matched），已有用例保持绿。

- [ ] **Step 3: 写 `_shared/map.ts`**

```ts
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
```

- [ ] **Step 4: 写 `role-list/api.ts`**（过滤语义对齐 mock：name 包含、status 精确、code 精确）

```ts
import { mapRole, paged, pageArgs } from "../_shared/map";

async function get(): Promise<void> {
  const name = String(http.param("name", ""));
  const status = String(http.param("status", ""));
  const code = String(http.param("code", ""));
  const { pageSize, current } = pageArgs();
  let sql = "select id, name, code, status, remark, create_time, update_time from role";
  const conds: string[] = [];
  const params: unknown[] = [];
  if (name) { conds.push("name like ?"); params.push("%" + name + "%"); }
  if (status !== "") { conds.push("status = ?"); params.push(Number(status)); }
  if (code) { conds.push("code = ?"); params.push(code); }
  if (conds.length) sql += " where " + conds.join(" and ");
  sql += " order by id";
  const rows: any[] = await db.query(sql, params);
  const all = rows.map(mapRole);
  json.ok(paged(all, pageSize, current));
}
get.route = "/role-list";
export default { get };
```

- [ ] **Step 5: 写 `role-item/api.ts`**

```ts
import { mapRole } from "../_shared/map";

async function post(): Promise<void> {
  const b = http.body as { name?: string; code?: string; status?: number; remark?: string } | null;
  if (!b || !b.name || !b.code) { json.fail(400, "name and code required"); return; }
  const now = Date.now();
  const rows: any[] = await db.query(
    "insert into role (name, code, status, remark, create_time, update_time) values (?, ?, ?, ?, ?, ?) returning id",
    [b.name, b.code, b.status ?? 1, b.remark ?? "", now, now],
  );
  json.ok({ ...mapRole({ ...b, create_time: now, update_time: now }), id: rows[0].id });
}

async function put(): Promise<void> {
  const b = http.body as { id?: number; name?: string; code?: string; status?: number; remark?: string } | null;
  const id = Number(b?.id ?? 0);
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  const now = Date.now();
  const n = await db.exec(
    "update role set name = ?, code = ?, status = ?, remark = ?, update_time = ? where id = ?",
    [b?.name ?? "", b?.code ?? "", b?.status ?? 1, b?.remark ?? "", now, id],
  );
  if (n === 0) { json.fail(404, "no such role"); return; }
  const rows: any[] = await db.query(
    "select id, name, code, status, remark, create_time, update_time from role where id = ?", [id]);
  json.ok(mapRole(rows[0]));
}

async function del(): Promise<void> {
  const id = Number(http.body);   // 前端 DELETE 的 body 是裸 JSON 数字
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  await db.exec("delete from role_menu where role_id = ?", [id]);
  const n = await db.exec("delete from role where id = ?", [id]);
  if (n === 0) { json.fail(404, "no such role"); return; }
  json.ok({ deleted: true });
}

post.route = "/role-item";
put.route = "/role-item";
del.route = "/role-item";
export default { post, put, del };
```

- [ ] **Step 6: 写 `role-menu/api.ts`**（精简投影，根节点省略 parentId——对齐 mock）

```ts
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
```

- [ ] **Step 7: 写 `menu-by-role-id/api.ts`**

```ts
async function get(): Promise<void> {
  const id = Number(http.param("id", 0));
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  const rows: any[] = await db.query(
    "select menu_id from role_menu where role_id = ? order by menu_id", [id]);
  json.ok(rows.map((r) => r.menu_id));
}
get.route = "/menu-by-role-id";
export default { get };
```

- [ ] **Step 8: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src`
Expected: admin role 5 个用例全绿，其余用例不回归。

- [ ] **Step 9: Commit**

```bash
git add sample/src/admin/ sample/tests/admin.test.ts
git commit -m "feat(sample): admin role 域——role-list/role-item/role-menu/menu-by-role-id + L1"
```

---

### Task 3: menu 域 2 端点

**Files:**
- Create: `sample/src/admin/menu-list/api.ts`
- Create: `sample/src/admin/menu-item/api.ts`
- Test: `sample/tests/admin.test.ts`（追加 describe）

**Interfaces:**
- Consumes: `mapMenu / paged / pageArgs`（Task 2 的 `_shared/map.ts`）。
- 路由：`GET /menu-list`、`POST|PUT|DELETE /menu-item`。

- [ ] **Step 1: 追加 L1 失败测试**

```ts
describe("admin menu", () => {
  it("GET /menu-list → 8 条种子菜单，keepAlive 为 boolean，order 可缺省", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/menu-list", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.total).toBe(8);
    const root = body.data.list[0];
    expect(root.id).toBe(100);
    expect(root.parentId).toBe("");
    expect(root.keepAlive).toBe(true);
    expect(root.order).toBe(100);
    const leaf = body.data.list[1];
    expect(leaf.parentId).toBe(100);
    expect(leaf.order).toBe(undefined);   // sort 为 NULL → order 缺省（对齐 mock）
  });

  it("menu-item POST→PUT→DELETE 闭环（DELETE body 为裸数字）", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const created = JSON.parse((await client.post("/menu-item", {
      headers: h,
      body: JSON.stringify({ parentId: 100, menuType: 0, name: "system:menu.audit", path: "/system/audit", component: "/system/audit", icon: "AuditOutlined", keepAlive: true, status: 1 }),
    })).body);
    expect(created.code).toBe(0);
    const id = created.data.id;
    expect(id > 0).toBeTruthy();

    const updated = JSON.parse((await client.put("/menu-item", {
      headers: h,
      body: JSON.stringify({ id, parentId: 100, menuType: 0, name: "system:menu.audit2", path: "/system/audit2", component: "/system/audit2", icon: "AuditOutlined", keepAlive: false, status: 1 }),
    })).body);
    expect(updated.data.name).toBe("system:menu.audit2");
    expect(updated.data.keepAlive).toBe(false);

    const deleted = JSON.parse((await client.del("/menu-item", {
      headers: h, body: String(id),
    })).body);
    expect(deleted.code).toBe(0);
  });
});
```

- [ ] **Step 2: 跑测试确认失败（404）**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src`
Expected: admin menu 2 用例 FAIL，其余绿。

- [ ] **Step 3: 写 `menu-list/api.ts`**

```ts
import { mapMenu, paged, pageArgs } from "../_shared/map";

const COLS = "id, parent_id, menu_type, name, path, component, sort, icon, current_active_menu, iframe_link, keep_alive, external_link, hide_in_menu, ignore_access, status, create_time, update_time";

async function get(): Promise<void> {
  const { pageSize, current } = pageArgs();
  const rows: any[] = await db.query("select " + COLS + " from menu order by id", []);
  json.ok(paged(rows.map(mapMenu), pageSize, current));
}
get.route = "/menu-list";
export default { get };
```

- [ ] **Step 4: 写 `menu-item/api.ts`**

```ts
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
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src`
Expected: admin menu 2 用例绿，无回归。

- [ ] **Step 6: Commit**

```bash
git add sample/src/admin/menu-list sample/src/admin/menu-item sample/tests/admin.test.ts
git commit -m "feat(sample): admin menu 域——menu-list/menu-item + L1"
```

---

### Task 4: user-info / get-async-routes / notifications

**Files:**
- Create: `sample/src/admin/user-info/api.ts`
- Create: `sample/src/admin/get-async-routes/api.ts`
- Create: `sample/src/admin/notifications/api.ts`
- Test: `sample/tests/admin.test.ts`（追加 describe）

**Interfaces:**
- Consumes: `http.user = {id, roles, claims}`（auth 守卫注入）；`_platform.users` 表（deps 已在 Task 1 manifest 声明）。
- 路由：`GET /user-info`、`GET /get-async-routes`、`GET /notifications`。

- [ ] **Step 1: 追加 L1 失败测试**

```ts
describe("admin user/notify", () => {
  it("GET /user-info → demo 用户，roles 含 admin", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/user-info", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.username).toBe("demo");
    expect(body.data.roles).toContain("admin");
  });

  it("GET /get-async-routes → admin 绑定的菜单组成路由树（/system 带子节点）", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/get-async-routes", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.length).toBe(1);              // 只有 /system 一棵（menu_type 3 的按钮不下发）
    const sys = body.data[0];
    expect(sys.path).toBe("/system");
    expect(sys.handle.title).toBe("system:menu.system");
    expect(sys.handle.order).toBe(100);
    expect(sys.children.length).toBe(4);           // user/role/menu/dept
    expect(sys.children[0].component).toBe("/system/user");
  });

  it("GET /notifications → 4 条，isRead 为 boolean", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/notifications", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.data.length).toBe(4);
    expect(body.data[0].isRead).toBe(true);
    expect(body.data[1].isRead).toBe(false);
    expect(body.data[0].title).toBe("收到了 14 份新周报");
  });
});
```

- [ ] **Step 2: 跑测试确认失败（404）**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src`
Expected: 3 个新用例 FAIL，其余绿。

- [ ] **Step 3: 写 `user-info/api.ts`**

```ts
async function get(): Promise<void> {
  const u = http.user;
  if (!u) { json.fail(401, "unauthorized"); return; }
  const rows: any[] = await db.query("select username from users where id = ?", [u.id]);
  json.ok({
    id: String(u.id),
    username: rows.length ? rows[0].username : "",
    avatar: "",
    email: "",
    phoneNumber: "",
    description: "",
    roles: u.roles ?? [],
  });
}
get.route = "/user-info";
export default { get };
```

- [ ] **Step 4: 写 `get-async-routes/api.ts`**

```ts
// 动态路由下发：http.user.roles → role.code → role_menu → menu（menu_type 0/1/2）组树。
// 响应形状对齐 react-antd-admin：{ path, component?, handle: {title, icon?, order?, ...}, children? }

async function get(): Promise<void> {
  const u = http.user;
  if (!u) { json.fail(401, "unauthorized"); return; }
  const roles: string[] = u.roles ?? [];
  let menuIds: number[] = [];
  if (roles.length) {
    const ph = roles.map(() => "?").join(",");
    const roleRows: any[] = await db.query("select id from role where code in (" + ph + ")", roles);
    const rids = roleRows.map((r) => r.id);
    if (rids.length) {
      const ph2 = rids.map(() => "?").join(",");
      const binds: any[] = await db.query(
        "select menu_id from role_menu where role_id in (" + ph2 + ")", rids);
      menuIds = binds.map((b) => b.menu_id);
    }
  }
  if (!menuIds.length) { json.ok([]); return; }
  const ph3 = menuIds.map(() => "?").join(",");
  const rows: any[] = await db.query(
    "select id, parent_id, name, path, component, sort, icon, keep_alive, iframe_link, external_link from menu where menu_type in (0, 1, 2) and id in (" + ph3 + ") order by id",
    menuIds);

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
```

- [ ] **Step 5: 写 `notifications/api.ts`**

```ts
async function get(): Promise<void> {
  const rows: any[] = await db.query(
    "select avatar, date, is_read, message, title from notification order by id", []);
  json.ok(rows.map((n) => ({
    avatar: n.avatar ?? "",
    date: n.date,
    isRead: !!n.is_read,
    message: n.message ?? "",
    title: n.title,
  })));
}
get.route = "/notifications";
export default { get };
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src`
Expected: 3 个新用例绿，无回归。

- [ ] **Step 7: Commit**

```bash
git add sample/src/admin/user-info sample/src/admin/get-async-routes sample/src/admin/notifications sample/tests/admin.test.ts
git commit -m "feat(sample): admin user-info/get-async-routes/notifications + L1"
```

---

### Task 5: home/pie + home/line + L2 测试

**Files:**
- Create: `sample/src/admin/home/pie/api.ts`
- Create: `sample/src/admin/home/line/api.ts`
- Test: `sample/tests/admin.test.ts`（追加 describe）
- Test: `sample/test/admin.test.ts`（L2 vitest 新文件）

**Interfaces:**
- Produces: `lineLength(range: string): number`（命名导出，L2 直测；week→7、month→`new Date().getDate()`、year→当年截至上月底累计天数、其他→0）。
- 路由：`GET /home/pie`、`POST /home/line`。

- [ ] **Step 1: 追加 L1 失败测试**

```ts
describe("admin home", () => {
  it("GET /home/pie → 5 个品类", async () => {
    const token = await client.login("demo", "demo1234");
    const r = await client.get("/home/pie?by=month", {
      headers: { Authorization: "Bearer " + token, "X-TENANT-ID": "default" },
    });
    const body = JSON.parse(r.body);
    expect(body.code).toBe(0);
    expect(body.data.length).toBe(5);
    expect(body.data[0].code).toBe("electronics");
    expect(typeof body.data[0].value).toBe("number");
  });

  it("POST /home/line week/month/year/其他 → 7/当天数/累计天数/[]", async () => {
    const token = await client.login("demo", "demo1234");
    const h = { Authorization: "Bearer " + token, "X-TENANT-ID": "default" };
    const week = JSON.parse((await client.post("/home/line", { headers: h, body: JSON.stringify({ range: "week" }) })).body);
    expect(week.data.length).toBe(7);
    const month = JSON.parse((await client.post("/home/line", { headers: h, body: JSON.stringify({ range: "month" }) })).body);
    expect(month.data.length).toBe(new Date().getDate());
    const year = JSON.parse((await client.post("/home/line", { headers: h, body: JSON.stringify({ range: "year" }) })).body);
    const now = new Date();
    let days = 0;
    for (let m = 0; m < now.getMonth(); m++) days += new Date(now.getFullYear(), m + 1, 0).getDate();
    expect(year.data.length).toBe(days);
    const other = JSON.parse((await client.post("/home/line", { headers: h, body: JSON.stringify({ range: "nope" }) })).body);
    expect(other.data.length).toBe(0);
  });
});
```

- [ ] **Step 2: 写 L2 测试 `sample/test/admin.test.ts`**

```ts
// L2 纯 mock 单测：admin 模块 handler 逻辑（不启动 server、不跑 v8）。

import { describe, it, expect } from "vitest";
import roleList from "../src/admin/role-list/api";
import { lineLength } from "../src/admin/home/line/api";
import { invoke } from "./invoke";

describe("admin/role-list (L2 mock)", () => {
  it("wraps dbRows into paged camelCase list", async () => {
    const r = await invoke(roleList, "get", {
      dbRows: [
        { id: 1, name: "超级管理员", code: "admin", status: 1, remark: "最高权限", create_time: 1729752330782, update_time: 1729752330782 },
      ],
    });
    expect(r.code).toBe(0);
    expect(r.data.total).toBe(1);
    expect(r.data.pageSize).toBe(10);
    expect(r.data.list[0].code).toBe("admin");
    expect(r.data.list[0].createTime).toBe(1729752330782);
  });

  it("respects pageSize/current params", async () => {
    const rows = [1, 2, 3].map((i) => ({ id: i, name: "r" + i, code: "c" + i, status: 1, remark: "", create_time: 1, update_time: 1 }));
    const r = await invoke(roleList, "get", { dbRows: rows, params: { pageSize: "2", current: "2" } });
    expect(r.data.total).toBe(3);
    expect(r.data.list.length).toBe(1);
    expect(r.data.list[0].id).toBe(3);
  });
});

describe("admin/home/line lineLength (L2)", () => {
  it("week → 7；未知 → 0", () => {
    expect(lineLength("week")).toBe(7);
    expect(lineLength("nope")).toBe(0);
  });

  it("month → 当天日号；year → 截至上月底累计天数", () => {
    const now = new Date();
    expect(lineLength("month")).toBe(now.getDate());
    let days = 0;
    for (let m = 0; m < now.getMonth(); m++) days += new Date(now.getFullYear(), m + 1, 0).getDate();
    expect(lineLength("year")).toBe(days);
  });
});
```

- [ ] **Step 3: 跑 L1 + L2 确认失败**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src` → admin home 2 用例 FAIL（404）
Run: `cd sample/test && npx vitest run` → 新文件 FAIL（import 不到模块）

- [ ] **Step 4: 写 `home/pie/api.ts`**（mock 不消费 `by`，5 品类固定值——确定性可测）

```ts
function get(): void {
  json.ok([
    { value: 42, code: "electronics" },
    { value: 25, code: "home_goods" },
    { value: 18, code: "apparel_accessories" },
    { value: 60, code: "food_beverages" },
    { value: 33, code: "beauty_skincare" },
  ]);
}
get.route = "/home/pie";
export default { get };
```

- [ ] **Step 5: 写 `home/line/api.ts`**

```ts
// range → 数组长度（对齐 fake/home.fake.ts 的日期逻辑；mock 值是随机数，此处取确定性值便于测试）
export function lineLength(range: string): number {
  const now = new Date();
  if (range === "week") return 7;
  if (range === "month") return now.getDate();
  if (range === "year") {
    let days = 0;
    for (let m = 0; m < now.getMonth(); m++) {
      days += new Date(now.getFullYear(), m + 1, 0).getDate();
    }
    return days;
  }
  return 0;
}

async function post(): Promise<void> {
  const b = http.body as { range?: string } | null;
  const n = lineLength(String(b?.range ?? ""));
  json.ok(Array.from({ length: n }, (_, i) => 100 + ((i * 137) % 901))); // 100–1000，同 mock 区间
}
post.route = "/home/line";
export default { post };
```

- [ ] **Step 6: 跑 L1 + L2 确认通过**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src` → 全绿
Run: `cd sample/test && npx vitest run` → 全绿

- [ ] **Step 7: Commit**

```bash
git add sample/src/admin/home sample/tests/admin.test.ts sample/test/admin.test.ts
git commit -m "feat(sample): admin home/pie + home/line + L1/L2 测试"
```

---

### Task 6: api-manual.md 补模块数据层（新第 3 章，3–12 顺延 4–13）

**Files:**
- Modify: `docs/devkit/api-manual.md`（插入新章 + 全文档章节号顺延 + 第 2/9/10/11/12 章局部补充）
- Modify: `docs/devkit/SKILL.md`（仅当其中引用了旧章节号——先 grep 确认）

**Interfaces:**
- Consumes: 事实来源 `docs/migration.md`（门禁/规则速查）、`sample/src/*/schema.yaml`（格式样例）、`sample/config.yaml`（`migrate_on_start`/`ownership_guard` 注释）。

- [ ] **Step 1: grep 交叉引用现状，列出需顺延的位置**

Run: `grep -n "第 [0-9]* 章\|第[0-9]" docs/devkit/api-manual.md docs/devkit/SKILL.md`
产出需修改的行清单；规则：≥3 的章节号全部 +1（标题、目录行、正文交叉引用、排障表/限制表内的引用）。

- [ ] **Step 2: 目录行更新**（第 13–15 行区域）

```
目录：1 快速开始 / 2 项目结构与模块约定 / 3 模块数据层 / 4 编写 api.ts / 5 导入解析 /
6 全局对象 API 参考 / 7 响应信封与错误码 / 8 鉴权与多租户 / 9 测试 /
10 配置 config.yaml / 11 构建与发布 / 12 运维要点 / 13 安全红线与已知限制
```

- [ ] **Step 3: 在原「## 3. 编写 api.ts」之前插入新章**（标题 `## 3. 模块数据层`，原 3–12 章标题各 +1）。新章内容：

```markdown
## 3. 模块数据层

> 何时读我：模块要建表、改表、跨模块取数、接存量库时。
> 运维视角的完整 runbook 见 `docs/migration.md`；本章只写开发要点。

### 心智模型

- **声明为源**：每模块可选 `schema.yaml` 声明自己拥有的表。启动 / `oj migrate` 时自动
  收敛到声明（**安全前向**：缺表 CREATE、缺可空列 ALTER ADD、缺索引 CREATE INDEX）。
- **演进靠迁移**：无法安全推导的变更（NOT NULL 列新增、疑似改名、类型变更、数据回填）
  一律 fail-fast 并打印迁移模板——手写 `migrations/{seq:04}__{desc}[.{sqlite|mysql|postgres}].sql`。
- **账本记账**：`_oj_migrations_<module>` 记录每模块已应用的迁移；带方言后缀的文件只在
  对应方言库执行。
- **表归属**：schema.yaml 喂归属图（表 → 模块，同表双声明拒启 S002）与 `SchemaRegistry`
  （`db.table()` 列白名单）；跨模块表访问须 manifest `deps:` 声明。

### schema.yaml

```yaml
tables:
  account:
    pk: id                                    # 可选；主键列须在 columns 声明（仅单列）
    columns:
      id: { type: integer, autoincrement: true }
      name: { type: text, null: false }
      role: { type: text }
```

- 列类型最小集：`integer | bigint | text | boolean | double | blob`；`null` 缺省可空。
- 与 manifest `tables:` 双向一致（S005）：声明了表就要进 manifest，反之亦然。

### 三层 SQL 文件

| 文件 | 时机 | 纪律 |
|---|---|---|
| `migrations/{seq:04}__{desc}[.方言].sql` | 启动（auto）/ `oj migrate` | 只前向；账本 `_oj_migrations_<module>`；序列空洞/乱序 S007 报错 |
| `seed.sql`（模块级） | 每次启动重放 | 幂等 `INSERT OR IGNORE`；按 `;` 切分，语句内不得含分号字面量（S006） |
| `fixtures/*.sql` | 仅 `oj test` / `oj fixture` 灌入 | 演示/测试数据，不进 release 产物 |

### 命令

| 命令 | 用途 |
|---|---|
| `oj migrate -c config.yaml -d <dir>` | 应用待执行迁移（`--baseline`：存量库接入，≤head 记为已应用不执行） |
| `oj schema diff -c config.yaml -d <dir>` | 声明 vs 实库对账（D001 漂移 / D002 未声明表），只读，漂移 exit 1 |
| `oj build --check` | 只跑结构检查 S001–S007 不落盘（CI 门禁） |

### 门禁配置（第 10 章 server 表）

- `server.migrate_on_start`：`auto`（dev 默认，启动即应用）/ `verify`（release 默认，
  账本落后拒启 M004，报错附 `oj migrate` 命令）/ `off`。
- `server.ownership_guard`：`warn`（默认，跨模块表访问仅告警）/ `deny`（未声明 deps
  拒绝执行，500 附修复指引）。灰度建议：先 warn 观察日志，声明补齐后切 deny。
- **无主表语义**：模块未部署时其表无主，运行时不设防（部分部署/灰度属设计语义）。

### 跨模块取数（deps）

```yaml
# manifest.yaml
deps:
  user: "^0.1.0"     # 模块名 → 版本范围；裸 SQL join 对方表前必须声明
```

误报逃生门：SQL 注释 `/* oj:allow-table=x,y */`。

### 回滚

schema 回滚**没有自动机制**（refinery 语义，只前向）：破坏性变更前备份数据文件；
反向变更手写新 seq 迁移前向执行。应用回滚 = `dist/manifests.yaml` 指回旧版本目录 + 重启。
```

- [ ] **Step 4: 第 2 章补充**

- 源码树（约第 126–142 行）在 `manifest.yaml` 行后加三行：

```
│   │   ├── schema.yaml            # 声明式表结构（有表模块必配，第 3 章）
│   │   ├── migrations/            # 手写迁移 {seq:04}__{desc}[.方言].sql
│   │   ├── fixtures/              # 演示数据，仅 oj test / oj fixture 灌入
```

- manifest.yaml 小节约第 153–158 行的代码块补两行字段说明：

```yaml
# tables: [account]   # 有表模块必配：拥有的表清单（与 schema.yaml 双向一致 S005）
# deps: { user: "^0.1.0" }  # 跨模块表访问声明（ownership_guard 校验依据）
```

- [ ] **Step 5: 第 10 章（原第 9 章配置）server 表补两行**（插在 `grace_days` 行之后）

```
| `migrate_on_start` | `auto`（dev）/ `verify`（release） | 迁移门禁：auto 启动即应用；verify 账本落后拒启（M004，先 `oj migrate`）；`off` 迁移完全归运维 |
| `ownership_guard` | `"warn"` | 表归属守卫：`warn` 跨模块表访问仅告警；`deny` 未声明 deps 拒绝执行（500 附修复指引） |
```

fail-fast 表补两行：

```
| 同一张表被两个模块 schema.yaml 声明 | 表归属单射违反（S002），启动拒启 |
| release 下迁移账本落后（verify 门禁） | M004 拒启，报错附 `oj migrate` 命令 |
```

- [ ] **Step 6: 第 11 章（原第 10 章构建）`oj build` 小节补一行**

`oj build` 内建结构检查 S001–S007（manifest 合法性/表归属/deps/tables 一致/seed 纪律/迁移序列），违规 fail build；`oj build --check` 只查不落盘，作 CI 门禁。

- [ ] **Step 7: 第 12 章（原第 11 章运维）排障表补三行**

```
| 启动报 M004 / 迁移账本落后 | release verify 门禁：有迁移未应用 | 先 `oj migrate -c config.yaml -d dist` 再启动 |
| 500 报跨模块表访问被拒 | `ownership_guard: deny` 且未声明 deps | manifest 补 `deps:`；或临时 SQL 注释 `/* oj:allow-table=x */` |
| `oj schema diff` exit 1 | 声明与实库漂移（D001/D002） | 按输出对齐 schema.yaml 或补迁移；存量库用 `oj migrate --baseline` |
```

热重载表补一行：`| schema.yaml / migrations 变更 | 否（重启或 oj migrate 生效；dev auto 在下次启动收敛） |`

- [ ] **Step 8: 第 13 章（原第 12 章）已知限制表补两行**

```
| schema 回滚无自动机制 | 迁移只前向；破坏性变更前备份，反向变更写新 seq 迁移 |
| fixtures/ 不进 release 产物 | 演示数据走 fixtures（oj test / oj fixture）；参考数据走模块 seed.sql |
```

- [ ] **Step 9: 交叉引用顺延 + SKILL.md 同步**

按 Step 1 的清单把 api-manual.md 正文所有 ≥3 的「第 N 章」引用 +1；
`docs/devkit/SKILL.md` 若引用章节号则同步（无引用则不动）。

- [ ] **Step 10: 校验无残留旧编号**

Run: `grep -n "第 3 章\|## 3\. 编写\|第 12 章\|## 12\. 安全" docs/devkit/api-manual.md`
Expected: `## 3. 编写` 无匹配；`第 12 章` 仅指向新编号语义正确的位置（人工逐条确认）。

- [ ] **Step 11: Commit**

```bash
git add docs/devkit/api-manual.md docs/devkit/SKILL.md
git commit -m "docs(devkit): api-manual 新增第 3 章模块数据层，章节顺延 + 配置/排障/限制补充"
```

---

### Task 7: 全量验证与 release 复验

**Files:**
- Modify: `sample/README.md`（模块清单加一行 admin，若该文件列了模块）

- [ ] **Step 1: sample/README.md 补 admin 模块一行**（先读文件找模块清单段落；无清单则跳过）

- [ ] **Step 2: L1 全量**

Run: `cargo run -p oj -- test -c sample/config.yaml -d sample/src --format human`
Expected: 全部用例绿（旧 5 文件 + 新 admin.test.ts），退出码 0。

- [ ] **Step 3: L2 全量**

Run: `cd sample/test && npm ci && npx vitest run`
Expected: 全绿。

- [ ] **Step 4: 结构检查 + 构建**

Run: `cargo run -p oj -- build --check -d sample/src -o sample/dist`
Run: `cargo run -p oj -- build -d sample/src -o sample/dist`
Expected: --check 全绿；build 产出 `dist/admin-0.1.0/`（含 routes.js，其中 pattern 为 `role-list` 等根挂载路由，`.route` 已从 api.js 剥离）。

- [ ] **Step 5: release 迁移 + 复验**

Run: `cargo run -p oj -- migrate -c sample/config.yaml -d sample/dist`（release verify 门禁要求先迁移）
Run: 后台 `cargo run -p oj -- server -c sample/config.yaml --api-path sample/dist`，然后：

```bash
TOKEN=$(curl -s -X POST http://localhost:9778/v1/api/auth/login -H 'Content-Type: application/json' -d '{"username":"demo","password":"demo1234"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["data"]["access_token"])')
curl -s http://localhost:9778/v1/api/role-list -H "Authorization: Bearer $TOKEN" -H 'X-TENANT-ID: default'
curl -s http://localhost:9778/v1/api/user-info -H "Authorization: Bearer $TOKEN" -H 'X-TENANT-ID: default'
curl -s http://localhost:9778/v1/api/get-async-routes -H "Authorization: Bearer $TOKEN" -H 'X-TENANT-ID: default'
```

Expected: 三个端点均返回 `{"code":0,…}` 且 data 形状与 L1 断言一致。完毕后杀掉 server。

- [ ] **Step 6: Commit**

```bash
git add sample/README.md sample/dist
git commit -m "chore(sample): README 补 admin 模块 + release 产物重建"
```

---

## Self-Review 记录

- **Spec 覆盖**：5 域 16 端点中 login/logout/refresh-token 复用内置 `/auth/*`（spec 决策 1），
  其余 12 端点 → Task 2–5；数据层 → Task 1；信封/路径/映射决策落在各 handler；
  手册补全 → Task 6；验证矩阵 → Task 7。无遗漏。
- **类型一致性**：`paged(all, pageSize, current)` 全量数组入参、内部切片（total 取自全量长度），
  Task 3 沿用；`mapRole/mapMenu/pageArgs/lineLength` 名字全程一致。
- **占位符**：无 TBD/TODO；所有 handler 与测试代码均为可直接落盘的完整内容。
