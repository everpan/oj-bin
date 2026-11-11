// 机器人绑定运营端点（spec 2026-11-11 P2）：单条 upsert / 列表 / 删除 /
// CSV 批量导入 / CSV 导出。CSV 解析用最简 split——不支持引号内逗号
// （绑定三元组均为平台标识/ID，不含逗号；复杂 quoting 出现再升级）。
import { nowSecs } from "../../auth/_shared/session";
import { isAdmin, denyNonAdmin } from "../_shared/guard";

const COLS = ["provider", "external_sub", "user_id"] as const;

async function upsertOne(provider: string, externalSub: string, userId: string, by: string): Promise<"insert" | "update"> {
  const n = await db.table("oj_bot_binding").update({ user_id: userId })
    .where({ and: [
      { field: "provider", op: "eq", value: provider },
      { field: "external_sub", op: "eq", value: externalSub },
    ] }).run();
  if (n) return "update";
  await db.table("oj_bot_binding").insert({
    provider, external_sub: externalSub, user_id: userId,
    last_used_at: 0, created_by: by, created_at: nowSecs(),
  }).run();
  return "insert";
}

async function get(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const rows: any[] = await db.table("oj_bot_binding")
    .select(["provider", "external_sub", "user_id", "last_used_at", "created_by", "created_at"])
    .orderBy([{ field: "created_at", dir: "desc" }])
    .all();
  json.ok(rows);
}

async function post(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const b = (http.body || {}) as Record<string, unknown>;
  const provider = String(b.provider ?? "").trim();
  const externalSub = String(b.external_sub ?? "").trim();
  const userId = String(b.user_id ?? "").trim();
  if (!provider || !externalSub || !userId) {
    json.fail(400, "provider/external_sub/user_id required");
    return;
  }
  json.ok({ op: await upsertOne(provider, externalSub, userId, String(http.user!.id)) });
}

async function del(): Promise<void> {
  if (!isAdmin()) return denyNonAdmin();
  const provider = String(http.param("provider", ""));
  const externalSub = String(http.param("external_sub", ""));
  const n = await db.table("oj_bot_binding").delete().where({ and: [
    { field: "provider", op: "eq", value: provider },
    { field: "external_sub", op: "eq", value: externalSub },
  ] }).run();
  if (!n) { json.fail(404, "no such binding"); return; }
  json.ok({ deleted: n });
}

// CSV 批量导入：body 为 text/csv 全文，首行表头 provider,external_sub,user_id。
// 行数上限沿管线 max_upload（请求体闸），不另设。
const importCsv = {
  async post(): Promise<void> {
    if (!isAdmin()) return denyNonAdmin();
    const text = typeof http.body === "string" ? http.body : "";
    const lines = text.split(/\r?\n/).filter((l) => l.trim());
    if (!lines.length) { json.fail(400, "empty csv"); return; }
    const header = lines[0].split(",").map((s) => s.trim());
    if (header.join(",") !== COLS.join(",")) {
      json.fail(400, "csv header must be " + COLS.join(","));
      return;
    }
    const by = String(http.user!.id);
    let ok = 0;
    const skipped: string[] = [];
    for (let i = 1; i < lines.length; i++) {
      const parts = lines[i].split(",").map((s) => s.trim());
      if (parts.length !== 3 || parts.some((p) => !p)) { skipped.push(`line ${i + 1}`); continue; }
      await upsertOne(parts[0], parts[1], parts[2], by);
      ok++;
    }
    json.ok({ imported: ok, skipped, total: lines.length - 1 });
  },
};
importCsv.route = "import";

// CSV 导出：与 import 同列序（roundtrip 兼容）。
const exportCsv = {
  async get(): Promise<void> {
    if (!isAdmin()) return denyNonAdmin();
    const rows: any[] = await db.table("oj_bot_binding")
      .select(["provider", "external_sub", "user_id"])
      .orderBy([{ field: "provider", dir: "asc" }])
      .all();
    const s = json.stream({ contentType: "text/csv" });
    s.write(COLS.join(",") + "\n");
    for (const r of rows) s.write(`${r.provider},${r.external_sub},${r.user_id}\n`);
    s.end();
  },
};
exportCsv.route = "export";

export default { get, post, del, importCsv, exportCsv };
