// L1 内容版本化：commit/get/history/diff/rollback/乐观锁/CAS 去重/事务一致性。
// 运行：oj test -c sample/config.yaml -d sample/src
import { createVersions } from "../src/_platform/_shared/versions";

const versions = createVersions(db);

describe("versions (CAS + revisions)", () => {
  it("commit/history/get/rollback", async () => {
    const eid = `v1-${Date.now()}`;   // 测试库持久：实体 id 每次运行唯一
    for (let i = 1; i <= 5; i++) {
      await versions.commit("doc", eid, `content v${i}`, { author: "tester", message: `m${i}` });
    }
    const log = await versions.history("doc", eid);
    expect(log.length).toBe(5);
    expect(await versions.get("doc", eid, 3)).toBe("content v3");
    expect(await versions.get("doc", eid, "head")).toBe("content v5");
    const d = await versions.diff("doc", eid, 4, 5);
    expect(d.some((l) => l.type === "del" && l.text === "content v4")).toBe(true);
    expect(d.some((l) => l.type === "add" && l.text === "content v5")).toBe(true);
    const r = await versions.rollback("doc", eid, 2, { author: "tester", message: "回滚到 v2" });
    expect(r.version).toBe(6);
    expect(await versions.get("doc", eid, "head")).toBe("content v2");
  });

  it("expect 乐观锁：撞版本 409 / 同内容 CAS 去重", async () => {
    await versions.commit("doc", "v2", "A", {});
    const head = Number((await versions.history("doc", "v2"))[0].version);
    let conflictErr: { status?: number } | null = null;
    try {
      await versions.commit("doc", "v2", "B", { expect: head - 1 });
    } catch (e) {
      conflictErr = e as { status?: number };
    }
    expect(conflictErr?.status).toBe(409);
    await versions.commit("doc", "v2", "B", {});          // last-write-wins
    const shaB = await crypto.sha256Hex("B");
    const before = Number((await db.query(
      "SELECT COUNT(*) AS n FROM oj_ver_content WHERE sha256 = ?", [shaB]))[0].n);
    await versions.commit("doc", "v3", "B", {});          // 同内容不同实体 → 不重复落内容
    const after = Number((await db.query(
      "SELECT COUNT(*) AS n FROM oj_ver_content WHERE sha256 = ?", [shaB]))[0].n);
    expect(after).toBe(before);
  });

  it("db.tx 回滚连带修订回滚（同一事务）", async () => {
    const txEid = `tx1-${Date.now()}`;
    await db.tx(async () => {
      await versions.commit("doc", txEid, "X", { author: "t" });
      throw new Error("boom");
    }).catch(() => {});
    expect((await versions.history("doc", txEid)).length).toBe(0);
  });

  it("diff 大文档：格子预算闸", async () => {
    const big1 = Array.from({ length: 2500 }, (_, i) => `l${i}`).join("\n");
    const big2 = Array.from({ length: 2500 }, (_, i) => `x${i}`).join("\n");
    await versions.commit("doc", "big", big1, {});
    await versions.commit("doc", "big", big2, {});
    let msg = "";
    try { await versions.diff("doc", "big", 1, 2); } catch (e) { msg = String(e); }
    expect(msg).toContain("diff too large");
  });
});
