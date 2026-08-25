// L2 纯 mock 全局（无真实 deno 运行时）：vitest 单测时把运行时注入的全局
// db/json/http/bus/log 替换成可控桩，从而在不启动 server、不跑 v8 的情况下
// 直接调用真实 api.ts handler 的业务逻辑（对标 L1 的 client 派发，但全本地、零 IO）。

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

export function installGlobals(opts: GlobalsOptions = {}): ResponseCapture {
  published = [];
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
    query: async (_sql: string, _params?: any[]) => opts.dbRows ?? [],
    exec: async (_sql: string, _params?: any[]) => 1,
  };

  (globalThis as any).log = { debug() {}, info() {}, warn() {}, error() {} };

  return cap;
}

export function lastPublished(): Array<{ topic: string; msg: any }> {
  return published;
}
