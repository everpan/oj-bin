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
