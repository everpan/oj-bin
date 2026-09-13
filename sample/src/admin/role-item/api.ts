import { mapRole } from "../_shared/map";

async function post(): Promise<void> {
  const b = http.body as { name?: string; code?: string; status?: number; remark?: string } | null;
  if (!b || !b.name || !b.code) { json.fail(400, "name and code required"); return; }
  const now = Date.now();
  // builder 不支持 RETURNING，保留裸 query 原子取回生成的主键
  const rows: any[] = await db.query(
    "insert into role (name, code, status, remark, create_time, update_time) values (?, ?, ?, ?, ?, ?) returning id",
    [b.name, b.code, b.status ?? 1, b.remark ?? "", now, now],
  );
  json.ok({ ...mapRole({ ...b, status: b.status ?? 1, remark: b.remark ?? "", create_time: now, update_time: now }), id: rows[0].id });
}

async function put(): Promise<void> {
  const b = http.body as { id?: number; name?: string; code?: string; status?: number; remark?: string } | null;
  const id = Number(b?.id ?? 0);
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  if (!b || !b.name?.trim() || !b.code?.trim()) { json.fail(400, "name and code required"); return; }
  const now = Date.now();
  const n = await db.table("role").update({
    name: b.name,
    code: b.code,
    status: b.status ?? 1,
    remark: b.remark ?? "",
    update_time: now,
  }).where({ field: "id", op: "eq", value: id }).run();
  if (n === 0) { json.fail(404, "no such role"); return; }
  const rows: any[] = await db.table("role")
    .select(["id", "name", "code", "status", "remark", "create_time", "update_time"])
    .where({ field: "id", op: "eq", value: id })
    .all();
  json.ok(mapRole(rows[0]));
}

async function del(): Promise<void> {
  const id = Number(http.body);   // 前端 DELETE 的 body 是裸 JSON 数字
  if (!(id > 0)) { json.fail(400, "id required"); return; }
  let found = true;
  try {
    await db.tx(async (tx: any) => {
      await tx.table("role_menu").delete().where({ field: "role_id", op: "eq", value: id }).run();
      const n = await tx.table("role").delete().where({ field: "id", op: "eq", value: id }).run();
      if (n === 0) { found = false; throw new Error("no such role"); }
    });
  } catch (_e) {
    if (!found) { json.fail(404, "no such role"); return; }
    throw _e;
  }
  json.ok({ deleted: true });
}

post.route = "/role-item";
put.route = "/role-item";
del.route = "/role-item";
export default { post, put, del };
