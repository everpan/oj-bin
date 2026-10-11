// 默认库（--db 缺省 = default）查询；sqlite 下 db.query 直查合法。
const r = await db.query("select 1 as v", []);
console.log("DBV=" + r[0].v);
