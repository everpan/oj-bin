import { positiveId, requireRole } from "#_shared/validate";

function get(): void {
  const id = Number(http.param("id", 0));
  const q = db.table("account").select(["id", "name", "role"]);
  (id > 0 ? q.where({ field: "id", op: "eq", value: id }) : q)
    .all()
    .then((r) => json.ok(r))
    .catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { name?: string; role?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  const role = (() => { try { return requireRole(b.role ?? "user"); } catch (e) { return ""; } })();
  if (!role) { json.fail(400, "role must be admin|user"); return; }
  db.table("account").insert({ name: b.name, role })
    .run()
    .then(() => json.ok({ created: true }))
    .catch((e) => json.fail(500, String(e)));
}

function put(): void {
  const b = http.body as { id?: number; name?: string };
  const id = (() => { try { return positiveId(b.id); } catch { return 0; } })();
  if (!id || !b.name) { json.fail(400, "id and name required"); return; }
  db.table("account").update({ name: b.name }).where({ field: "id", op: "eq", value: id })
    .run()
    .then(() => json.ok({ updated: true }))
    .catch((e) => json.fail(500, String(e)));
}

function del(): void {
  const id = positiveId(http.param("id", 0));
  db.table("account").delete().where({ field: "id", op: "eq", value: id })
    .run()
    .then(() => json.ok({ deleted: true }))
    .catch((e) => json.fail(500, String(e)));
}

function patch(): void {
  const b = http.body as { id?: number; role?: string };
  const role = requireRole(b.role);
  db.table("account").update({ role }).where({ field: "id", op: "eq", value: positiveId(b.id) })
    .run()
    .then(() => json.ok({ patched: true }))
    .catch((e) => json.fail(500, String(e)));
}

function head(): void { get(); }

function options(): void {
  json.ok({ methods: ["get", "post", "put", "del", "patch", "head", "options"] });
}

export default { get, post, put, del, patch, head, options };
