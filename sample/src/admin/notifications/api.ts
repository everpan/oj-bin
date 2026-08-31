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
