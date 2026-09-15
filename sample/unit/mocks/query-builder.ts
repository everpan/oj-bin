// L2 用的 `db.table(...)` 构造器桩。
//
// 定位：**镜像** `src/bridge/bootstrap.js` 的 `builderFromReq` JS 面（方法名与链式语义一致，
// 见该文件 §safe query builder），让 L2 能在不跑 v8 的前提下调用真实 handler 里的构造器代码。
//
// SQL 由本文件按**规范形**渲染（小写关键字、`col, col`、`?` 占位）——真实渲染是 sea-query
// 按方言产出的（如 sqlite 的 `SELECT "id" FROM "account"`），mock 不复刻方言细节。保留的
// 信号是 spec 真正要断言的：**handler 选了哪张表 / 哪些列 / 什么过滤条件 / 绑定参数是什么**。
// 因此 spec 断言的是规范形，而非某个方言的真身（判据见 docs/testing.md §L2）。
//
// 覆盖范围：L2 用到的单条件形态（`{field, op, value}`）与 select/insert/update/delete 四个动词。
// 遇到未覆盖的形态（树形 or/and、子查询、join、聚合列等）**直接抛错**，避免静默渲染出
// 与真实语义不符的字符串让断言失去意义。

export interface SqlCall {
  fn: "query" | "exec";
  sql: string;
  params: any[];
}

const OPS: Record<string, string> = {
  eq: "=",
  ne: "!=",
  gt: ">",
  gte: ">=",
  lt: "<",
  lte: "<=",
  like: "like",
};

/** 单条件 → SQL 片段（值走 `?` 占位并推入 params）。 */
function renderCond(cond: any, params: any[]): string {
  if (!cond || typeof cond !== "object" || Array.isArray(cond)) {
    throw new Error(`mock query-builder: 未覆盖的条件形态 ${JSON.stringify(cond)}`);
  }
  const field = String(cond.field ?? "");
  const op = String(cond.op ?? "eq");
  if (!field) {
    throw new Error(`mock query-builder: 条件缺 field（${JSON.stringify(cond)}）`);
  }
  if (op === "isNull" || op === "isNotNull") {
    return `${field} is ${op === "isNotNull" ? "not " : ""}null`;
  }
  if (op === "in" || op === "notIn") {
    const list = Array.isArray(cond.value) ? cond.value : [cond.value];
    if (list.length === 0) return op === "in" ? "1 = 0" : "1 = 1"; // 空集合语义
    params.push(...list);
    return `${field} ${op === "in" ? "in" : "not in"} (${list.map(() => "?").join(", ")})`;
  }
  const sqlOp = OPS[op];
  if (!sqlOp) {
    throw new Error(`mock query-builder: 未覆盖的比较 op ${op}`);
  }
  params.push(cond.value);
  return `${field} ${sqlOp} ?`;
}

/** 条件列表 → ` where a = ? and b = ?`（无条件返回空串）。 */
function renderWhere(conditions: any[], params: any[]): string {
  if (!conditions.length) return "";
  return ` where ${conditions.map((c) => renderCond(c, params)).join(" and ")}`;
}

function renderOrders(orders: any[]): string {
  if (!orders.length) return "";
  const parts = orders.map((o) => {
    if (!o || typeof o !== "object" || Array.isArray(o)) {
      throw new Error(`mock query-builder: 未覆盖的 orderBy 形态 ${JSON.stringify(o)}`);
    }
    return `${String(o.field)}${o.dir ? ` ${String(o.dir)}` : ""}`;
  });
  return ` order by ${parts.join(", ")}`;
}

function renderCols(cols: any[]): string {
  if (!cols.length) return "*";
  return cols
    .map((c) => {
      if (typeof c === "string") return c;
      throw new Error(`mock query-builder: 未覆盖的 select 列形态 ${JSON.stringify(c)}`);
    })
    .join(", ");
}

/** 构造器桩：`db.table("t")` 返回它（同 bootstrap.js 的 API 面，未用到的动词也保留）。 */
export function tableBuilder(
  table: string,
  record: (call: SqlCall) => void,
  rows: () => any[],
) {
  const req = {
    table,
    columns: [] as any[],
    conditions: [] as any[],
    order_by: [] as any[],
    limit: null as number | null,
    offset: null as number | null,
    verb: "select",
    values: [] as any[],
    sets: {} as Record<string, any>,
    returning: [] as string[],
  };

  const build = (): SqlCall => {
    const params: any[] = [];
    let sql: string;
    if (req.verb === "select") {
      sql =
        `select ${renderCols(req.columns)} from ${req.table}` +
        renderWhere(req.conditions, params) +
        renderOrders(req.order_by) +
        (req.limit === null ? "" : ` limit ${req.limit}`) +
        (req.offset === null ? "" : ` offset ${req.offset}`);
    } else if (req.verb === "insert") {
      const cols = Object.keys(req.values[0] ?? {});
      if (!cols.length) throw new Error("mock query-builder: insert 无列");
      const tuples = req.values.map((row) => {
        if (Object.keys(row).length !== cols.length) {
          throw new Error("mock query-builder: 多行 insert 的列不一致");
        }
        return `(${cols.map(() => "?").join(", ")})`;
      });
      for (const row of req.values) params.push(...cols.map((c) => row[c]));
      sql =
        `insert into ${req.table} (${cols.join(", ")}) values ${tuples.join(", ")}` +
        (req.returning.length ? ` returning ${req.returning.join(", ")}` : "");
    } else if (req.verb === "update") {
      const cols = Object.keys(req.sets);
      const whereParams: any[] = [];
      const whereSql = renderWhere(req.conditions, whereParams);
      sql = `update ${req.table} set ${cols.map((c) => `${c} = ?`).join(", ")}${whereSql}`;
      // 参数顺序与 SQL 一致：先 set 值，再 where 值
      params.push(...cols.map((c) => req.sets[c]), ...whereParams);
    } else {
      sql = `delete from ${req.table}` + renderWhere(req.conditions, params);
    }
    return { fn: "query", sql, params };
  };

  const api: any = {
    select(cols?: any[]) {
      req.columns = (cols ?? []).map((c: any) => (typeof c === "string" ? String(c) : c));
      return api;
    },
    where(cond: any) {
      req.conditions.push(cond);
      return api;
    },
    orderBy(items?: any[]) {
      req.order_by = items ?? [];
      return api;
    },
    limit(n: number) {
      req.limit = n | 0;
      return api;
    },
    offset(n: number) {
      req.offset = n | 0;
      return api;
    },
    insert(rowOrRows: any) {
      req.verb = "insert";
      req.values = Array.isArray(rowOrRows) ? rowOrRows.map((r) => ({ ...r })) : [{ ...rowOrRows }];
      return api;
    },
    returning(cols?: string[]) {
      req.returning = (cols ?? []).map(String);
      return api;
    },
    update(sets: Record<string, any>) {
      req.verb = "update";
      req.sets = { ...sets };
      return api;
    },
    delete() {
      req.verb = "delete";
      return api;
    },
    all() {
      record(build());
      return Promise.resolve(rows());
    },
    run() {
      // 与运行时同款守卫（bootstrap.js run()）：写操作必须有 where / insert 必须有行
      if ((req.verb === "update" || req.verb === "delete") && req.conditions.length === 0) {
        throw new Error(req.verb + " requires where");
      }
      if (req.verb === "insert" && req.values.length === 0) {
        throw new Error("insert needs at least one row");
      }
      const call = build();
      record(call);
      if (req.returning.length) return Promise.resolve(rows());
      return Promise.resolve(1);
    },
    toSQL() {
      return build().sql;
    },
  };
  return api;
}
