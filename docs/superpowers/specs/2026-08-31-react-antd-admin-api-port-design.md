# react-antd-admin API 移植到 sample + api-manual 数据层补全 — 设计

日期：2026-08-31
状态：已批准（2026-08-31）

## 背景

把 `/Users/ever/git/web/react-antd-admin/packages/runtime/src/api` 的前端接口契约
（5 域 16 端点）在 `sample/` 下以 oj 规范实现为真实后端；同时根据扫描结果补齐
`docs/devkit/api-manual.md` 缺失的模块数据层内容（a626215 已落地但手册未覆盖）。

已与用户确认的决策：

- 范围：全部 5 域（认证/用户、首页、通知、角色、菜单）。
- 数据层：真实 DB + schema.yaml + migrations + seed.sql。
- 信封：保持 oj `{code,msg,data}`，前端 react-antd-admin 自行适配。
- 布局：单模块 `admin` + `.route = "/<path>"` 根挂载，URL 与前端逐字一致。

## Part 1 — sample 新模块 `admin`

### 源接口清单（react-antd-admin，基座 `/api`，信封 `{code:200,result,message,success}`）

| 域 | 函数 | 方法 | 路径 | 参数 | 返回 result |
|---|---|---|---|---|---|
| 认证 | fetchLogin | POST | `login` | `{username,password}` | `{token,refreshToken}` |
| 认证 | fetchLogout | POST | `logout` | 无 | `{}` |
| 认证 | fetchRefreshToken | POST | `refresh-token` | `{refreshToken}` | `{token,refreshToken}` |
| 认证 | fetchUserInfo | GET | `user-info` | Authorization 头 | `UserInfoType` |
| 认证 | fetchAsyncRoutes | GET | `get-async-routes` | Authorization 头 | 路由树 |
| 首页 | fetchPie | GET | `home/pie` | query `by`（mock 不消费） | `{value,code}[]` |
| 首页 | fetchLine | POST | `home/line` | `{range: week\|month\|year}` | `number[]` |
| 通知 | fetchNotifications | GET | `notifications` | 无 | `NotificationItem[]` |
| 角色 | fetchRoleList | GET | `role-list` | query `name/status/code` + 分页 | `{list,total,pageSize,current}` |
| 角色 | fetchAddRoleItem | POST | `role-item` | `RoleItemType` | 回显 |
| 角色 | fetchUpdateRoleItem | PUT | `role-item` | `RoleItemType` | 回显 |
| 角色 | fetchDeleteRoleItem | DELETE | `role-item` | body 为裸数字 id | 回显 |
| 角色 | fetchRoleMenu | GET | `role-menu` | 无 | 菜单树精简投影 |
| 角色 | fetchMenuByRoleId | GET | `menu-by-role-id` | query `id` | `number[]` |
| 菜单 | fetchMenuList | GET | `menu-list` | 任意 query（mock 不过滤） | `{list,total,pageSize,current}` |
| 菜单 | fetchAdd/Update/DeleteMenuItem | POST/PUT/DELETE | `menu-item` | `MenuItemType` / 裸 id | `{}` |

### 模块结构

```
sample/src/admin/
├── manifest.yaml            # name: admin；tables: [role, menu, role_menu, notification]；
│                            # deps: { _platform: "^0.1.0" }（deps 是 map：模块→版本范围，
│                            # 参照 order/manifest.yaml；get-async-routes/user-info 读 users 表）
├── schema.yaml
├── migrations/0001__init.sql
├── seed.sql                 # 2 角色 + 8 菜单 + admin 全量 role_menu 绑定 + 4 通知
├── role-list/api.ts         get    .route="/role-list"
├── role-item/api.ts         post/put/del .route="/role-item"
├── role-menu/api.ts         get    .route="/role-menu"
├── menu-by-role-id/api.ts   get    .route="/menu-by-role-id"
├── menu-list/api.ts         get    .route="/menu-list"
├── menu-item/api.ts         post/put/del .route="/menu-item"
├── user-info/api.ts         get    .route="/user-info"
├── get-async-routes/api.ts  get    .route="/get-async-routes"
├── notifications/api.ts     get    .route="/notifications"
├── home/pie/api.ts          get    .route="/home/pie"
└── home/line/api.ts         post   .route="/home/line"
```

### 关键设计决策

1. **登录三件套不重新实现**：复用内置 `POST {base}/auth/{login,refresh,logout}`；
   前端适配层负责 `access_token`↔`token`、`refresh_token`↔`refreshToken` 字段映射。
   不写 `login/logout/refresh-token` 端点。
2. **URL 逐字对齐前端**：每个 handler 挂 `.route="/<前端路径>"` 根挂载；前端只需
   `VITE_API_BASE_URL=/v1/api`。`/home/pie` 等两级路径用 `.route="/home/pie"`。
3. **数据形状以 mock 实际数据为准**（前端类型声明与 mock 多处不一致）：
   `home/line` 返回 `number[]`；`menu-by-role-id` 返回 `number[]`；分页含 `pageSize`；
   menu 的 `keepAlive/hideInMenu/ignoreAccess` 输出 boolean。
4. **字段映射**：DB 列 snake_case，响应 camelCase（`createTime/parentId/menuType…`）；
   `order` 是 SQL 关键字 → 列名 `sort`，输出映射为 `order`。
5. **归属守卫**：sample `ownership_guard: deny`；读 `users` 表的端点靠
   manifest `deps: { _platform: "^0.1.0" }`（map 语法，参照 order 模块既有写法）。
6. **get-async-routes**：从 menu 表取 `menu_type IN (0,1,2)`，按 `http.user.roles`
   → role.id → role_menu 过滤，组装 `{path, component, handle:{title,icon,order}, children}` 树。
7. **user-info**：`http.user` + users 表查 username；`avatar/email/phoneNumber/description`
   users 表无对应列，返回空串（前端可显）。
8. **home/pie**：5 个固定品类 `{value,code}`（code 用 mock 的 5 个品类码），value 固定值
   （确定性、可测；mock 本就是随机数）。
9. **home/line**：复刻 mock 日期逻辑——week→7 个、month→当月已过天数个、
   year→当年截至上月底累计天数个、其他→`[]`；值用确定性的索引函数（可测）。
10. **role-list 过滤**：`name` 包含匹配（LIKE 绑定参数）、`status` 精确、`code` 精确；
    分页 `pageSize/current`（默认 10/1），响应 `{list,total,pageSize,current}`。
11. **role-item/menu-item 的 DELETE**：body 为裸 JSON 数字（`http.body` 解析为 number）。

### 表结构（schema.yaml + migrations/0001__init.sql）

- `role`：id(pk, autoincrement), name, code, status(integer 0/1), remark,
  create_time, update_time（ms 时间戳 integer）
- `menu`：id(pk), parent_id(integer，根为 0), menu_type(0/1/2/3), name, path,
  component, sort, icon, current_active_menu, iframe_link, keep_alive,
  external_link, hide_in_menu, ignore_access, status, create_time, update_time
- `role_menu`：id(pk, autoincrement), role_id, menu_id（pk 仅支持单列；
  (role_id, menu_id) 去重由 handler 保证）
- `notification`：id(pk), avatar, date(text), is_read(integer 0/1), message, title

seed.sql：角色 admin(1)/common(2)；菜单 8 条（沿用 fake/system.fake.ts 示例）；
admin 角色绑定全部菜单、common 绑定子集；通知 4 条（沿用 fake/notification.fake.ts）。
全部 `INSERT OR IGNORE`，无分号字面量。

### 测试

- **L1** `sample/tests/admin.test.ts`：`client.login("demo","demo1234")` + 租户头，
  覆盖 12 个自建端点：role CRUD 闭环（增→列表过滤→改→删）、menu-list、
  notifications、home/pie、home/line 三 range、user-info、get-async-routes、
  menu-by-role-id。断言 oj 信封 `code===0` 与 data 形状。
- **L2** `sample/test/admin.test.ts`：仅两处纯逻辑——role-list 过滤/分页参数解析、
  home/line range→长度映射。

### 前端适配说明（交付注释，不改前端仓库）

- base：`VITE_API_BASE_URL` → `http://host:9778/v1/api`。
- 信封：`{code,msg,data}` ↔ `{code,result,message,success}`（`code:0` 即成功）。
- 登录：`/auth/login` 返回 `access_token/refresh_token/expires_in/user`。
- sample 启用 tenant：所有请求需 `X-TENANT-ID` 头；启用 auth：需 Bearer。

## Part 2 — docs/devkit/api-manual.md 补数据层

手册当前 12 章完全没有模块数据层（a626215 落地的 schema.yaml/migrations/fixtures/
oj migrate/oj schema diff/ownership_guard）。权威细节在 `docs/migration.md`，
手册只写开发视角要点并指向它，避免双源漂移。

1. **新增第 3 章「模块数据层」**（原 3–12 顺延为 4–13，目录行同步）：
   - schema.yaml 字段语义（tables/pk/columns/最小类型集 integer|bigint|text|boolean|double|blob、null、autoincrement）；
   - 三大用途：归属图 + SchemaRegistry 列白名单 + 安全前向收敛（缺表 CREATE/缺可空列 ALTER/缺索引）；
   - fail-fast 边界：NOT NULL 新增、疑似改名 → 打印迁移模板；
   - 迁移三层文件：migrations/`{seq:04}__{desc}[.方言].sql`（账本 `_oj_migrations_<module>`）、
     模块级 seed.sql（幂等重放、S006 纪律）、fixtures/（仅 `oj test`/`oj fixture`，不进 release）；
   - 命令：`oj migrate`（含 `--baseline`）、`oj schema diff`（D001/D002，漂移 exit 1）、
     `oj build --check`（S001–S007，CI 门禁）；
   - `/* oj:allow-table=x,y */` 逃生门；schema 只前向、无自动回滚。
2. **第 2 章**：源码树补 `schema.yaml`/`migrations/`/`fixtures/`；manifest.yaml 补
   `tables:`（与 schema.yaml 双向一致 S005）与 `deps:`（跨模块表访问声明 S003）。
3. **第 9 章**：server 配置表补 `migrate_on_start`（auto/verify/off）、
   `ownership_guard`（warn/deny）两行；fail-fast 表补 S002 表双声明拒启、
   M004 账本落后拒启（release verify）。
4. **排障表**补：`M004` 先 `oj migrate`；ownership deny 500 → manifest 补 deps；
   `oj schema diff` 漂移对账。**已知限制表**补：schema 回滚只前向；fixtures 不进发布产物。
   **热重载表**补：schema.yaml/migrations 变更需重启或 `oj migrate`。

## 验证

1. `cargo build`（release）+ `cargo fmt --check` + `cargo clippy --all-targets -D warnings`。
2. `cargo run -p oj -- build --check`（sample 结构 S001–S007 全绿）。
3. `cargo run -p oj -- test -c sample/config.yaml -d sample/src`（L1 全绿，含新 admin.test.ts）。
4. `cd sample/test && npx vitest run`（L2 全绿）。
5. 手动 curl 走读：login → Bearer + 租户头打 `/v1/api/role-list`、`/v1/api/user-info`、
   `/v1/api/get-async-routes`，核对其信封与 data 形状。
6. `cargo run -p oj -- server -c sample/config.yaml --api-path sample/dist` release 复验
   （先 `oj build` + `oj migrate`）。
