// .route 声明的 TS 支持（编辑器不报错；dev server 不依赖此文件运行）。
//
// 下面为 src/bridge/bootstrap.js 在运行时注入的全局对象提供类型声明，
// 使 sample/src 中的业务代码在编辑器内不报 TS 错误。
//
// ext_boot.js（config.yaml 同目录，可选）在运行时补充的全局**不在本文件声明** ——
// 它们是项目自定义的，框架无法预知。用 ext_boot.js 增补了全局（如 `json.page()`）后，
// 在业务项目里另建一个 .d.ts 自行声明（并加进 tsconfig 的 include），否则编辑器报
// TS2339 "Property 'page' does not exist"。运行时不受影响——类型只影响编辑器与 tsc。

// JSON 反序列化后可能出现的值（SQL 行 / KV / fetch body 等）。
type Json = string | number | boolean | null | Json[] | { [k: string]: Json };

// 数据库查询返回的一行（列名 -> 值）。
type Row = Record<string, Json>;

// db.table(...).where(cond) 的单个条件。叶子用 field/op/value；组合用 and/or/not
// （也可经 db.leaf/db.and/db.or/db.not 构造）——故 field 可缺省（纯组合节点）。
interface WhereCond {
  field?: string;
  op?: string; // eq/neq/gt/gte/lt/lte/like/in/nin/null/notnull/... 依服务端支持
  value?: unknown;
  and?: WhereCond[];
  or?: WhereCond[];
  not?: WhereCond;
}

// db.table(...).orderBy(items) 的单个排序项。
interface OrderByItem {
  field: string;
  dir?: "asc" | "desc" | null;
}

// db.table(name) 返回的安全查询构造器（流式、结构化）。
interface QueryBuilder {
  select(cols?: (string | object)[]): QueryBuilder;
  where(cond: WhereCond): QueryBuilder;
  orderBy(items?: OrderByItem[]): QueryBuilder;
  limit(n: number): QueryBuilder;
  offset(n: number): QueryBuilder;
  all(): Promise<Json[]>;
  // DML：动词由 insert/update/delete 声明，run() 终执行（返回受影响行数）。
  // insert + returning(["id"]) 时 run() 改返回行数组 [{ id: n }]——
  // pg/sqlite 是单条 RETURNING 语句；mysql 同连接两步取 LAST_INSERT_ID()（建议放 db.tx）。
  insert(rows: object | object[]): QueryBuilder;
  update(sets: object): QueryBuilder;
  delete(): QueryBuilder;
  returning(cols?: string[]): QueryBuilder;
  // JOIN：on = 左右列对；kind 缺省 "inner"（left/right/... 依服务端支持）。
  join(
    table: string,
    on: { left: string; right: string }[],
    kind?: string,
  ): QueryBuilder;
  // DISTINCT / GROUP BY / HAVING / UNION / CTE（WITH）。
  distinct(): QueryBuilder;
  groupBy(cols?: string[]): QueryBuilder;
  having(cond: WhereCond): QueryBuilder;
  union(other: QueryBuilder, kind?: string): QueryBuilder;
  with(name: string, columns: string[], query: QueryBuilder): QueryBuilder;
  run(): Promise<number | Row[]>;
  // 只构造不执行（与 .all()/.run() 完全相同的校验管线）。
  toSQL(): { sql: string; params: unknown[] };
  // 序列化当前构造成员（可经 db.fromJSON 复原继续链式调用）。
  toJSON(): object;
}

// DB(name) 返回的命名数据库实例。
interface DBInstance {
  // 原始 SQL + 绑定参数（params 可选）。返回行数组。
  query(sql: string, params?: unknown[]): Promise<Row[]>;
  // 执行写操作，返回受影响行数。
  exec(sql: string, params?: unknown[]): Promise<number>;
  // 安全查询构造器（标识符白名单 + 参数化值）。
  table(name: string): QueryBuilder;
  // 条件构造器：leaf(field, op, value) / and / or / not，供 where/having 组合。
  leaf(field: string, op: string, value?: unknown): WhereCond;
  and(...conds: WhereCond[]): WhereCond;
  or(...conds: WhereCond[]): WhereCond;
  not(cond: WhereCond): WhereCond;
  // 从 toJSON() 快照复原构造器（继续链式调用）。
  fromJSON(snap: object): QueryBuilder;
  // 系统逃生通道（tenant sql_guard）：本请求绕过租户注入/校验。显式且被审计，
  // 业务 handler 不得使用。
  asSystem(): DBInstance;
  // 事务：回调 resolve 提交 / throw 回滚再抛；tx.query/exec/table 同签名走同一连接。
  // 每请求至多一个活跃事务（嵌套报错）；请求结束未完结自动回滚。
  tx(fn: (tx: DBInstance) => unknown): Promise<unknown>;
}

// json.* ：统一响应信封 + 响应头。
interface JsonApi {
  ok(data?: unknown): void;
  fail(code: number, msg: string, data?: unknown): void;
  header(name: string, value: string): void;
  // 裸 JSON 200（不套信封）：标准协议端点（如 OIDC）用；错误仍走 fail()。
  raw(data?: unknown): void;
}

// http.* ：当前请求上下文（只读，懒加载，per-request 最新）。
interface HttpApi {
  method: string;
  params: Record<string, string>;
  query: Record<string, string>;
  headers: Record<string, string>;
  body: any;
  // WS Binary 帧时 body 为 null；原始帧字节走 bodyBytes()（v0.1.16，文本帧亦可用）。
  bodyBytes(): Promise<Uint8Array>;
  // 取路由参数或 query 参数：路径参数优先，query 兜底，均缺失返回 def 原值。
  param(name: string, def?: unknown): any;
  // 租户 id（tenant 启用时从租户头提取；未启用为 null）。
  tenantId: string | null;
  // 已验签用户（auth 启用且通过 Bearer 守卫；否则 null）。
  user: AuthUser | null;
  // multipart 上传文件元信息（非 multipart 为空数组）。
  files: UploadedFileMeta[];
  // 取第 i 个上传文件的字节（越界报错 no such file）。
  file(i: number): Promise<Uint8Array>;
}

// 已验签用户（JWT claims）。
interface AuthUser {
  id: string | number;
  roles: string[];
  claims: Record<string, Json>;
}

// multipart 上传文件元信息。
interface UploadedFileMeta {
  field: string;
  filename: string;
  content_type: string;
  size: number;
}

// log.* ：结构化日志（zap SugaredLogger 风格：msg + 交替键值对）。
interface Logger {
  debug(msg: string, ...kv: unknown[]): void;
  info(msg: string, ...kv: unknown[]): void;
  warn(msg: string, ...kv: unknown[]): void;
  error(msg: string, ...kv: unknown[]): void;
}

// redis.* / kv.* ：KV 存储（redis.default 配置即真 Redis，否则进程内存 KV）。
interface KVApi {
  get(key: string): Promise<string | null>;
  set(key: string, value: string): Promise<boolean>;
  del(key: string): Promise<boolean>;
  // 设过期（秒）。真 Redis 走 EXPIRE；内存 KV 惰性过期。
  expire(key: string, ttlSec: number): Promise<boolean>;
  // 自增返回新值（键不存在从 0 起）。
  incr(key: string): Promise<number>;
}

// ws.* ：WebSocket 生命周期钩子内的主动发送/关闭控制（HTTP 路径 no-op）。
// send 帧型由参数类型决定：string → Text 帧(0x1)；Uint8Array → Binary 帧(0x2)（v0.1.16）。
interface WSApi {
  send(data: string | Uint8Array): void;
  close(): void;
}

// blob.* ：对象存储（可调用取命名实例：blob("media").put(...)；裸调用 blob.put(...) 等价 default）。
interface BlobApi {
  (name?: string): BlobApi;
  put(key: string, bytes: Uint8Array, contentType?: string): Promise<boolean>;
  get(key: string): Promise<Uint8Array>;
  // 幂等：不存在视为成功。
  del(key: string): Promise<boolean>;
  // local = {base}/blob/{key}；s3 = presigned URL（15min）。
  url(key: string): Promise<string>;
  // local 缺失 sidecar 且无法按扩展名推断时返回空串；s3 无 Content-Type 时返回 null。
  contentType(key: string): Promise<string | null>;
}

// bus.* ：主题广播。publish 广播给订阅 topic 的全部 WS 会话，返回接收方数；
// subscribe 仅 WS 会话内可用（HTTP 路径报错）；kind 报告活跃 broker 类型。
interface BusApi {
  // data 为 Uint8Array/ArrayBuffer → Binary 帧原字节（不包信封，v0.1.16）；其余 → JSON 信封 Text 帧。
  publish(topic: string, data?: unknown): Promise<number>;
  subscribe(topic: string): Promise<void>;
  kind(): Promise<string>;
}

// cert.* ：JWS 证书生成/重签（RSA + RS256 在 Rust 侧；纯内存，不落盘）。
interface CertApi {
  // 生成密钥对并签发 JWS。bits >= 2048；nbf/exp 为 Unix 秒，exp 必须 > nbf。
  generate(
    bits: number,
    nbf: number,
    exp: number,
  ): Promise<{ private_pem: string; public_pem: string; cert_jws: string }>;
  // 用现有 PKCS#8 私钥重签续期（公钥不变），返回新 cert.jws 串。
  renew(privatePem: string, nbf: number, exp: number): Promise<string>;
}

// es.* ：Elasticsearch 薄客户端（直通 ES 响应体；未配置调用报 es not configured）。
interface EsApi {
  search(index: string, dsl?: unknown): Promise<Json>;
  index(index: string, id: string, doc?: unknown): Promise<Json>;
  del(index: string, id: string): Promise<Json>;
}

// jwt.* ：JWT 签发/验签（secret/alg/durations 由 config auth: 注入；未配置调用报
// jwt not configured）。iat/exp 由 Rust 补（JS 不可控有效期），exp 取 access 时长。
interface JwtApi {
  // payload 至少含 sub；roles 可选（缺省空数组）。返回签名 token。
  sign(payload: { sub: string; roles?: string[] }): string;
  // 验签 + exp（leeway 0）；篡改/过期/算法不符直接抛错。
  verify(token: string): { sub: string; roles: string[]; iat: number; exp: number };
  // 配置的有效期（秒；getter 惰性求值）。
  readonly accessDuration: number;
  readonly refreshDuration: number;
}

// oidc.* ：内置 OP/RP（RS256 私钥只在 Rust 侧；issuer/rp/clients 由 config oidc: 注入；
// 未配置段调用报 oidc not configured）。verify 传 jwks 时按其 RS256 kid 匹配验签，
// 不传（或 null）回落本地私钥；篡改/过期/无匹配钥直接抛错。
interface OidcApi {
  // claims 至少含 iss/sub/aud/iat/exp。返回签名 token。
  sign(claims: Record<string, unknown>): string;
  // 验签 + exp；jwks 可选（缺省用本地钥）。
  verify(token: string, jwks?: unknown): Record<string, unknown>;
  // 本地公钥 JWKS（keys[0].kid/kty/alg/n/e）。
  jwks(): unknown;
  // config oidc.issuer（getter 惰性求值）。
  readonly issuer: string;
  // RP 侧 tenant → IdP 配置。
  readonly rp: Record<string, { issuer: string; client_id: string; client_secret: string; scope: string }>;
  // OP 侧客户端白名单。
  readonly clients: Record<string, { secret: string; redirect_uris: string[]; tenant: string }>;
}

// mail / Mail(key) ：邮件投递（config smtp: 段 + oj-mail 插件启用；未配置调用报
// mail not configured）。宿主做校验/附件解析，插件做投递；所有方法**都 resolve 信封**
// （校验失败 = {code:5}，不抛），只有「未配置」抛异常。
// code：0 成功（data.messageId 为投递凭据、data.jobId 为作业号）/ 1 网络/超时 /
// 2 SMTP 5xx / 3 鉴权 / 4 队列满 / 5 入参校验（地址、白名单、正文、附件、profile）。
interface MailEnvelope {
  code: number;
  msg: string;
  data: { jobId?: string; messageId?: string } & Record<string, Json>;
}

// 附件（引用式：字节不进 JS、不走 base64）：filename 必填，blobKey / path 二选一。
interface MailAttachment {
  filename: string;
  // blob 后端里的键（经 blob 字段选后端，缺省 "default"）。
  blobKey?: string;
  blob?: string;
  // 项目根内的本地文件路径（../ 越界即拒）。
  path?: string;
  // 显式 MIME（缺省按扩展名 → 字节嗅探 → application/octet-stream）。
  mime?: string;
}

interface MailSendRequest {
  from: string;
  to: string[];
  cc?: string[];
  bcc?: string[];
  subject?: string;
  text?: string;
  html?: string;
  headers?: Record<string, string>;
  attachments?: MailAttachment[];
}

interface MailRawRequest extends MailSendRequest {
  // RFC5322 原文（与 attachments 互斥）。
  raw: string;
}

interface MailApi {
  // 异步 transport；resolve 投递结果信封。
  send(m: MailSendRequest): Promise<MailEnvelope>;
  // 同步 transport（插件 worker 内阻塞投递）。
  sendSync(m: MailSendRequest): Promise<MailEnvelope>;
  // 入队即回 {code:0,data:{jobId}}；真实完成经 bus topic "mail.result" 上送。
  enqueue(m: MailSendRequest): Promise<MailEnvelope>;
  // 查宿主侧结果（未命中/已过期 → null）。
  result(jobId: string): Promise<Json | null>;
  // 原始 MIME 投递。
  sendRaw(o: MailRawRequest): Promise<MailEnvelope>;
}

interface Mail {
  new (key?: string): MailApi;
  // 已配置 profile 名清单（非密钥面：连接信息/凭据不进 JS）。
  profiles(): Promise<string[]>;
}

// bcrypt.* ：密码哈希（Rust 侧 spawn_blocking，CPU 密集不卡 isolate）。
interface BcryptApi {
  hash(password: string, cost?: number): Promise<string>;
  // 非法 hash 返回 false（不抛错）。
  verify(password: string, hash: string): Promise<boolean>;
}

declare global {
  interface Function {
    route?: string;
  }

  const json: JsonApi;
  const http: HttpApi;
  const log: Logger;
  const kv: KVApi;
  const redis: KVApi;
  const ws: WSApi;
  const blob: BlobApi;
  const bus: BusApi;
  const es: EsApi;
  // 默认 profile（"default"）的 mail 实例；其它 profile 用 new Mail("name")。
  const mail: MailApi;
  const Mail: Mail;

  // 命名数据库实例；未配置的名字返回 undefined。
  function DB(name: string): DBInstance | undefined;
  // 默认（"default"）数据库实例。
  const db: DBInstance;
  const cert: CertApi;
  const jwt: JwtApi;
  const oidc: OidcApi;
  const bcrypt: BcryptApi;

  // crypto 增补（bootstrap 对原生 crypto 做 Object.assign 合并，原生成员保留）：
  // sha256Hex = 十六进制摘要；randomHex = nBytes 字节随机数的 hex（默认 32 字节）。
  interface Crypto {
    sha256Hex(s: string): string;
    randomHex(nBytes?: number): string;
  }

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
  // WS 帧测试面（v0.1.16）：path 形如 "/echo-bin/ws"（相对 base）。
  interface TestWsFrame {
    binary: boolean;
    data: string | Uint8Array;
  }
  interface TestWs {
    send(data: string | Uint8Array): Promise<void>;
    // 下一帧：{binary, data}；对端关闭 {closed: true}；超时无帧 null（默认 1000ms）。
    next(ms?: number): Promise<TestWsFrame | { closed: true } | null>;
    close(): Promise<void>;
  }
  interface Client {
    ws(path: string): TestWs;
    get(path: string, opts?: ClientOptions): Promise<ClientResp>;
    post(path: string, opts?: ClientOptions): Promise<ClientResp>;
    put(path: string, opts?: ClientOptions): Promise<ClientResp>;
    del(path: string, opts?: ClientOptions): Promise<ClientResp>;
    patch(path: string, opts?: ClientOptions): Promise<ClientResp>;
    head(path: string, opts?: ClientOptions): Promise<ClientResp>;
    options(path: string, opts?: ClientOptions): Promise<ClientResp>;
    // 登录助手：POST /auth/login → 返回 access_token（失败抛错）。headers 透传（如租户头）。
    login(username: string, password: string, headers?: Record<string, string>): Promise<string>;
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
  function fetch(url: string, options?: {
    method?: string;
    headers?: Record<string, string>;
    body?: string | null;
  }): Promise<OjFetchResponse>;
  // 已加载插件自省：[{name, semver, abi_version, fingerprint, host_abi_version}]。
  function plugins(): any[];

  // 命名 MQ 客户端（config kafkas:/rabbits: 段；未配置的名 → undefined）。
  // 消费方法（poll/commit/ack/nack）仅长任务上下文可用（HTTP/WS 内调用报错）。
  function Kafka(name: string): OjKafkaClient | undefined;
  function RabbitMQ(name: string): OjRabbitClient | undefined;
  // 长任务上下文（对象全局）：stopping = 停机信号（HTTP/WS 上下文恒 false）；
  // sleep = 等待原语（本运行时无 timer 全局，setTimeout 不可用）。
  const tasks: {
    stopping(): boolean;
    sleep(ms: number): Promise<void>;
  };
}

interface OjMqMessage {
  topic: string;
  partition?: number;
  offset?: number;
  key?: string;
  value: any;
  // 非 UTF-8 载荷时 value 为 null，value_b64 为 base64 字符串（v0.1.16）；生产侧传
  // Uint8Array 亦编码进 value_b64（record 载荷 = 原始字节）。
  value_b64?: string;
  headers?: Record<string, string>;
  ts?: number;
  delivery_tag?: number; // rabbit 专属：ack/nack 载荷原样回传
}

interface OjKafkaClient {
  kind(): Promise<string>;
  send(topic: string, o: { key?: string; partition?: number; headers?: Record<string, string>; value: any }): Promise<{ sent: number }>;
  poll(topics: string[], o?: { max?: number; timeoutMs?: number }): Promise<{ messages: OjMqMessage[] }>;
  commit(m: OjMqMessage): Promise<Record<string, never>>;
  metadata(): Promise<any>;
}

interface OjRabbitClient {
  kind(): Promise<string>;
  publish(exchange: string, routingKey: string, value: any, o?: { headers?: Record<string, string> }): Promise<{ sent: number }>;
  poll(queues: string[], o?: { max?: number; timeoutMs?: number }): Promise<{ messages: OjMqMessage[] }>;
  ack(m: OjMqMessage): Promise<Record<string, never>>;
  nack(m: OjMqMessage, requeue?: boolean): Promise<Record<string, never>>;
  metadata(): Promise<any>;
}

// oj 的导入别名 `#x`（本模块根）/ `#/m/x`（src 根）是「引用方所在模块」相对的，
// tsconfig 的 `paths` 无法表达模块相对解析，只能枚举模块目录（见 tsconfig.json），
// 因此仅能命中**已存在**的文件。无法静态定位的别名（如尚未创建的 `#_shared/view`）
// 由 `types/oj-modules.d.ts`（非模块，真 ambient `declare module "#*"`）兜底为 `any`
// 而非报 TS2307；已存在、能被 `paths` 命中的别名仍走真实文件类型。运行时由 oj 的
// loader 解析，本文件与该兜底只影响编辑器 / tsc。

// fetch 返回的 Response（浏览器风格子集）。
interface OjFetchResponse {
  ok: boolean;
  status: number;
  statusText: string;
  headers: Record<string, string>;
  json(): Promise<Json | null>;
  text(): Promise<string>;
  arrayBuffer(): Promise<Uint8Array>;
  clone(): OjFetchResponse;
}

export {};
