// 路径参数路由（设计 §3）：挂 .route 后目录镜像被替换——
// /v1/api/user/item/{id} 可达，/v1/api/user/item 404。
function detail(): void {
  const id = Number(http.param("id", 0));
  if (!(id > 0)) {
    json.fail(400, "id required");
    return;
  }
  db.query("select id, name, role from account where id = ?", [id])
    .then((r) => (r.length ? json.ok(r[0]) : json.fail(404, "no such account")))
    .catch((e) => json.fail(500, String(e)));
}
detail.route = "{id}";
export default { get: detail };
