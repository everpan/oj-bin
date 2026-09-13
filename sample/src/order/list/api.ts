import { requireRole } from "../../user/_shared/validate";

function get(): void {
  const role = requireRole(http.param("role", "admin")); // 跨模块相对导入（UC-13）
  db.table("orders")
    .join("account", [{ left: "orders.account_id", right: "account.id" }], "inner")
    .select(["orders.id", "orders.no", "orders.amount", "account.name", "account.role"])
    .where({ field: "account.role", op: "eq", value: role })
    .orderBy([{ field: "orders.id", dir: "asc" }])
    .all()
    // builder 不支持列别名，这里把 account.name 映射回 account_name
    .then((r) => json.ok(r.map((row) => ({ ...row, account_name: row.name }))))
    .catch((e) => json.fail(500, String(e)));
}

export default { get };
