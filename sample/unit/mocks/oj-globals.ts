// L2 纯 mock 全局（无真实 deno 运行时）：vitest 单测时把运行时注入的全局
// db/json/http/bus/log 替换成可控桩，从而在不启动 server、不跑 v8 的情况下
// 直接调用真实 api.ts handler 的业务逻辑（对标 L1 的 client 派发，但全本地、零 IO）。
//
// 定位：本层只测「handler 内部分支 / 数据塑形 / 纯函数 / bus 事件内容 / 发出了什么 SQL」。
// 路由、鉴权、租户、真实 DB、统一信封、HTTP 状态码一律归 L1（`sample/tests/`，oj test）——
// 判据与去重规则见 docs/modules/08-testing.md §4。

export interface ResponseCapture {
  code: number;
  msg: string;
  data: any;
}

export interface GlobalsOptions {
  body?: any;
  params?: Record<string, string>;
  query?: Record<string, string>;
  headers?: Record<string, string>;
  user?: any;
  dbRows?: any[];
}

// 模块级最近一次 publish 记录（installGlobals 每次重置），供测试断言事件总线。
let published: Array<{ topic: string; msg: any }> = [];
// 模块级 SQL 调用记录：L2 可断言 handler 走了哪个分支（如「带 id 的查询 / 不带 id 的列表」），
// 「发出了什么 SQL」在 L1 里只能经响应间接观察，是本层的独特价值。
let sqlCalls: SqlCall[] = [];

export interface SqlCall {
  fn: "query" | "exec";
  sql: string;
  params?: any[];
}

export function installGlobals(opts: GlobalsOptions = {}): ResponseCapture {
  published = [];
  sqlCalls = [];
  const cap: ResponseCapture = { code: -1, msg: "", data: undefined };

  (globalThis as any).http = {
    method: "",
    params: opts.params ?? {},
    query: opts.query ?? {},
    headers: opts.headers ?? {},
    body: opts.body ?? null,
    user: opts.user,
    param: (n: string, d = "") => opts.params?.[n] ?? d,
  };

  (globalThis as any).json = {
    ok: (data?: any) => {
      cap.code = 0;
      cap.msg = "ok";
      cap.data = data;
      return cap;
    },
    fail: (code: number, msg: string, data?: any) => {
      cap.code = code;
      cap.msg = msg;
      cap.data = data;
      return cap;
    },
    header: () => {},
  };

  (globalThis as any).bus = {
    publish: (topic: string, msg: any) => published.push({ topic, msg }),
  };

  (globalThis as any).db = {
    query: async (sql: string, params?: any[]) => {
      sqlCalls.push({ fn: "query", sql, params });
      return opts.dbRows ?? [];
    },
    exec: async (sql: string, params?: any[]) => {
      sqlCalls.push({ fn: "exec", sql, params });
      return 1;
    },
  };

  (globalThis as any).log = { debug() {}, info() {}, warn() {}, error() {} };

  return cap;
}

export function lastPublished(): Array<{ topic: string; msg: any }> {
  return published;
}

export function lastSqlCalls(): SqlCall[] {
  return sqlCalls;
}
