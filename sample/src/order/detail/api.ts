const key = (id: string) => "order:detail:" + id;

function get(): void {
  const id = http.param("id", "0");
  kv.get(key(id)).then((hit) => {
    if (hit !== null) {
      json.ok({ cached: true, data: JSON.parse(hit) });
      return;
    }
    db.table("orders").select(["id", "no", "account_id", "amount"]).where({ field: "id", op: "eq", value: Number(id) })
      .all()
      .then((rows) => {
        const row = rows[0] ?? null;
        kv.set(key(id), JSON.stringify(row)).then(() =>
          json.ok({ cached: false, data: row })
        );
      })
      .catch((e) => json.fail(500, String(e)));
  });
}

export default { get };
