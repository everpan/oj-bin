// .route 声明的 TS 支持（编辑器不报错；dev server 不依赖此文件运行）。
//
// 下面为 src/bridge/bootstrap.js 在运行时注入的全局对象提供类型声明，
// 使 sample/src 中的业务代码在编辑器内不报 TS 错误。

// JSON 反序列化后可能出现的值（SQL 行 / KV / fetch body 等）。
type Json = string | number | boolean | null | Json[] | { [k: string]: Json };

// 数据库查询返回的一行（列名 -> 值）。
type Row = Record<string, Json>;

// db.table(...).where(cond) 的单个条件。
interface WhereCond {
  field: string;
  op?: string; // eq/neq/gt/gte/lt/lte/like/in/... 依服务端支持
  value?: unknown;
  and?: WhereCond[];
  or?: WhereCond[];
}

// db.table(...).orderBy(items) 的单个排序项。
interface OrderByItem {
  field: string;
  dir?: "asc" | "desc" | null;
}

// db.table(name) 返回的安全查询构造器（流式、结构化）。
interface QueryBuilder {
  select(cols?: string[]): QueryBuilder;
  where(cond: WhereCond): QueryBuilder;
  orderBy(items?: OrderByItem[]): QueryBuilder;
  limit(n: number): QueryBuilder;
  offset(n: number): QueryBuilder;
  all(): Promise<Json[]>;
}

// DB(name) 返回的命名数据库实例。
interface DBInstance {
  // 原始 SQL + 绑定参数（params 可选）。返回行数组。
  query(sql: string, params?: unknown[]): Promise<Row[]>;
  // 执行写操作，返回受影响行数。
  exec(sql: string, params?: unknown[]): Promise<number>;
  // 安全查询构造器（标识符白名单 + 参数化值）。
  table(name: string): QueryBuilder;
}

// json.* ：统一响应信封 + 响应头。
interface JsonApi {
  ok(data?: unknown): void;
  fail(code: number, msg: string, data?: unknown): void;
  header(name: string, value: string): void;
}

// http.* ：当前请求上下文（只读，懒加载，per-request 最新）。
interface HttpApi {
  method: string;
  params: Record<string, string>;
  query: Record<string, string>;
  headers: Record<string, string>;
  body: any;
  // 取路由参数或 query 参数（字符串）；缺失时回退 def（默认 ""）。
  param(name: string, def?: string): string;
}

// log.* ：结构化日志（zap SugaredLogger 风格：msg + 交替键值对）。
interface Logger {
  debug(msg: string, ...kv: unknown[]): void;
  info(msg: string, ...kv: unknown[]): void;
  warn(msg: string, ...kv: unknown[]): void;
  error(msg: string, ...kv: unknown[]): void;
}

// redis.* / kv.* ：内置内存 KV（M0）。
interface KVApi {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<boolean>;
}
interface KVWithDel extends KVApi {
  del(key: string): Promise<boolean>;
}

// ws.* ：WebSocket 帧循环控制。
interface WSApi {
  send(data: string): void;
  close(): void;
}

declare global {
  interface Function {
    route?: string;
  }

  const json: JsonApi;
  const http: HttpApi;
  const log: Logger;
  const kv: KVWithDel;
  const redis: KVApi;
  const ws: WSApi;

  // 命名数据库实例；未配置的名字返回 undefined。
  function DB(name: string): DBInstance | undefined;
  // 默认（"default"）数据库实例。
  const db: DBInstance;

  // ---- oj test L1 测试 SDK（oj test 运行时注入；仅测试文件使用） ----
  // 进程内 HTTP 派发助手，对标 Go Fiber app.Test：client.get/post/... 触发真实
  // 路由 + 真实运行时 + 真实后端（零 TCP）。path 为相对 base 的路径（如 "/user/account"）。
  interface ClientResp {
    status: number;
    headers: Record<string, string>;
    body: string;
    upgrade: boolean;
  }
  interface ClientOptions {
    headers?: Record<string, string>;
    body?: string;
  }
  interface Client {
    get(path: string, opts?: ClientOptions): Promise<ClientResp>;
    post(path: string, opts?: ClientOptions): Promise<ClientResp>;
    put(path: string, opts?: ClientOptions): Promise<ClientResp>;
    del(path: string, opts?: ClientOptions): Promise<ClientResp>;
    patch(path: string, opts?: ClientOptions): Promise<ClientResp>;
    head(path: string, opts?: ClientOptions): Promise<ClientResp>;
    options(path: string, opts?: ClientOptions): Promise<ClientResp>;
    // 登录助手：POST /auth/login → 返回 access_token（失败抛错）。
    login(username: string, password: string): Promise<string>;
  }
  const client: Client;

  // 轻量测试框架（vitest 风格子集）：describe/it/expect/beforeEach。
  function describe(name: string, fn: () => void): void;
  function it(name: string, fn: () => void | Promise<void>): void;
  function beforeEach(fn: () => void | Promise<void>): void;
  function expect(actual: unknown): {
    toBe(e: unknown): void;
    toEqual(e: unknown): void;
    toBeTruthy(): void;
    toBeFalsy(): void;
    toContain(sub: unknown): void;
  };

  // 标记会话结束。
  function finish(): void;
  // CJS 同步 require（eval + 进程级缓存）。
  function __ojRequire(name: string, referrerPath?: string): any;
  // 浏览器兼容的 fetch（url 必填，options 同 RequestInit 的常用子集）。
  function fetch(
    url: string,
    options?: { method?: string; headers?: Record<string, string>; body?: string | null },
  ): Promise<any>;
}

export {};
