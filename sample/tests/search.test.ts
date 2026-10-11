// L1 引擎原生 FTS（sqlite 腿 = bundled FTS5 trigram）。mysql/pg 腿 env-gated 真库后续基建。
import { createSearch } from "../src/_platform/_shared/search";

const search = createSearch(db);

describe("search (sqlite FTS5)", () => {
  it("index/query/highlight/remove", async () => {
    await db.exec("CREATE TABLE IF NOT EXISTS search_docs (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT, body TEXT)");
    await db.exec("DELETE FROM search_docs");
    await search.ensureIndex("search_docs", { fields: ["title", "body"] });
    await search.index("search_docs", { id: 1, title: "部署手册", body: "在生产环境部署 oj 服务" });
    await search.index("search_docs", { id: 2, title: "公告", body: "本周例会取消" });

    const hits = await search.query("search_docs", "环境部署", { limit: 10 });   // trigram ≥3 字
    expect(hits.length).toBe(1);
    expect(Number(hits[0].id)).toBe(1);

    const hl = await search.query("search_docs", "部署 oj", { limit: 10, highlight: true });
    expect(String(hl[0].fragments.body)).toContain("<mark>");

    const short = await search.query("search_docs", "部署", { limit: 10 });      // <3 字 → LIKE 回退
    expect(short.some((h) => Number(h.id) === 1)).toBe(true);

    await search.remove("search_docs", 1);
    expect((await search.query("search_docs", "环境部署", { limit: 10 })).length).toBe(0);
  });

  it("ensureIndex 拒绝在 db.tx 内调用（DDL 隐式提交）", async () => {
    await db.tx(async (tx) => {
      let msg = "";
      try { await createSearch(tx as unknown as DBInstance).ensureIndex("nope", { fields: ["x"] }); } catch (e) { msg = String(e); }
      expect(msg).toContain("inside db.tx");
    });
  });

  it("标识符白名单", async () => {
    let msg = "";
    try { await search.query("docs; DROP TABLE users", "x"); } catch (e) { msg = String(e); }
    expect(msg).toContain("invalid");
  });
});
