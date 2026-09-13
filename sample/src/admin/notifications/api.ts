async function get(): Promise<void> {
  const rows: any[] = await db.table("notification")
    .select(["avatar", "date", "is_read", "message", "title"])
    .orderBy([{ field: "id", dir: "asc" }])
    .all();
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
