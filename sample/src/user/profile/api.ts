function get(): void {
  const id = Number(http.param("id", 0));
  db.table("account").select(["id", "name", "role"]).where({ field: "id", op: "eq", value: id })
    .all()
    .then((r) => json.ok(r[0] ?? null))
    .catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { id?: number; name?: string };
  if (!b.id || !b.name) { json.fail(400, "id and name required"); return; }
  // `?` 是 oj 统一占位符：sqlite/mysql/postgres 通用，pg 由宿主自动改写 $n，
  // 无需 db.dialect() 分支（0.1.67 起）。
  db.exec("update account set name = ? where id = ?", [b.name, b.id])
    .then(() => json.ok({ renamed: true }))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
