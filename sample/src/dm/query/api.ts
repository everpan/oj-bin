// 达梦查询样例（oj-db-jdbc 插件，env-gated）：
// 1. config.yaml db: 段配 dm: 'jdbc:{"url":"jdbc:dm://127.0.0.1:5237",...}'（见该文件注释；
//    凭据建议 oj secret seal 成 ENC[...]，驱动 jar 放 <exe>/plugins/<triple>/jdbc/drivers/）。
// 2. jdbc 方言在宿主是「未知方言」：db.table 构造器被结构化门控（OJ_DB_UNSUPPORTED），
//    一律走 DB("dm").query 原始 SQL + 绑参（placeholder 为 ?）。
// 3. 未配置 db.dm 时 DB("dm") 返回 undefined——本样例如实 500，不静默。
get.schema = { description: "达梦参数查询样例（需 config 配 db.dm 为 jdbc DSN）" };

function get(): void {
  const id = Number(http.param("id", 0));
  const dm = DB("dm");
  if (!dm) {
    json.fail(500, "db.dm not configured (see sample/config.yaml comments)");
    return;
  }
  // account 换成你的表名；绑定参数防注入，字符串拼接仅限可信标识符。
  dm.query("select id, name from account where id = ?", [id])
    .then((rows) => json.ok(rows))
    .catch((e) => json.fail(500, String(e)));
}
