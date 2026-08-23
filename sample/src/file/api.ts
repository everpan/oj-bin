// catch-all 路由（设计 §3）：{*path} 至少吃一段。
// /v1/api/file/a/b/c → path="a/b/c"；/v1/api/file → 404。
function get(): void {
  json.ok({ segs: http.param("path", "").split("/") });
}
get.route = "{*path}";
export default { get };
