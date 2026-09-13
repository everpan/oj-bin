import { escapeHtml } from "escape-goat";

function post(): void {
  const b = http.body as { account_id?: number; amount?: number; no?: string };
  if (!b.account_id || !b.amount || !b.no) {
    json.fail(400, "account_id, amount, no required");
    return;
  }
  const no = escapeHtml(String(b.no)); // 裸 specifier 参与请求处理（UC-15）
  db.table("orders").insert({ no, account_id: b.account_id, amount: b.amount })
    .run()
    .then(() => json.ok({ created: true, no }))
    .catch((e) => json.fail(500, String(e)));
}

function get(): void {
  const id = Number(http.param("id", 0));
  db.table("orders").select(["id", "no", "account_id", "amount"]).where({ field: "account_id", op: "eq", value: id })
    .all()
    .then((r) => json.ok(r))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
