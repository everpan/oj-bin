// ext:bridge_ext/bootstrap.js -- JS SDK globals (port of Go bridge.go Apply).
// Rust ops do I/O and state; this file shapes the JS-side API
// (equivalent of the goja map[string]any bindings).
//
// Globals: json / db / DB / http / redis / log / fetch / finish / __ojRequire
// Plus safe query builder: db.table(name).select(...).where(...).orderBy(...).limit(...).all()
// Not ported yet: ws, Redis(name), XORM(name).

import {
  op_blob_del,
  op_blob_get,
  op_blob_put,
  op_blob_url,
  op_bus_publish,
  op_bus_subscribe,
  op_db_exec,
  op_db_has,
  op_db_query,
  op_db_query_build,
  op_db_tx_begin,
  op_db_tx_commit,
  op_db_tx_rollback,
  op_es_search,
  op_es_index,
  op_es_del,
  op_fetch,
  op_finish,
  op_http_info,
  op_http_file,
  op_json_fail,
  op_json_header,
  op_json_ok,
  op_kv_get,
  op_kv_set,
  op_kv_del,
  op_kv_expire,
  op_kv_incr,
  op_log,
  op_resolve_cjs as __oj_resolve_cjs,
  op_ws_send,
  op_ws_close,
} from "ext:core/ops";

// ----- json: unified envelope + response headers -----
globalThis.json = {
  // data is JSON.stringify'd on the JS side, so the op can splice it into the
  // envelope verbatim, avoiding the serde_v8 deserialize + serde_json re-serialize cost.
  ok: (data) => op_json_ok(data === undefined ? "null" : JSON.stringify(data)),
  fail: (code, msg, data) =>
    op_json_fail(code | 0, String(msg), data === undefined ? null : data),
  header: (name, value) => op_json_header(String(name), String(value)),
};

// ----- http helpers: current request context (lazy proxy; fresh per request) -----
const httpInfo = () => op_http_info();
globalThis.http = new Proxy({}, {
  get: (_t, p) => {
    if (p === "param") {
      return (name, def) => {
        const info = httpInfo();
        const v = info.params[name] !== undefined ? info.params[name] : info.query[name];
        return v === undefined ? def : v;
      };
    }
    if (p === "file") return (i) => op_http_file(i | 0);
    return httpInfo()[p];
  },
});

// ----- log: structured logging (msg + alternating key/value pairs, like zap SugaredLogger) -----
function logCall(level, msg, kv) {
  const fields = {};
  for (let i = 0; i + 1 < kv.length; i += 2) fields[String(kv[i])] = kv[i + 1];
  // JSON.stringify once on the JS side, hand the JSON string straight to Rust
  // (avoid double serialization via serde_v8 + to_string).
  op_log(level, String(msg), JSON.stringify(fields));
}
globalThis.log = {
  debug: (msg, ...kv) => logCall(0, msg, kv),
  info: (msg, ...kv) => logCall(1, msg, kv),
  warn: (msg, ...kv) => logCall(2, msg, kv),
  error: (msg, ...kv) => logCall(3, msg, kv),
};

// ----- redis: KV backend (in-memory default; real Redis when configured) -----
globalThis.redis = {
  get: (key) => op_kv_get(String(key)),
  set: (key, value) => op_kv_set(String(key), String(value)),
  del: (key) => op_kv_del(String(key)),
  // ttl in seconds (op takes ms)
  expire: (key, ttlSeconds) => op_kv_expire(String(key), Number(ttlSeconds) * 1000),
  incr: (key) => op_kv_incr(String(key)),
};

// ----- kv: same KV as redis global (spec name for oj handlers) -----
globalThis.kv = {
  get: (key) => op_kv_get(String(key)),
  set: (key, value) => op_kv_set(String(key), String(value)),
  del: (key) => op_kv_del(String(key)),
  expire: (key, ttlSeconds) => op_kv_expire(String(key), Number(ttlSeconds) * 1000),
  incr: (key) => op_kv_incr(String(key)),
};

// ----- blob: object storage (put/get/del/url; contentType via http blob route) -----
globalThis.blob = {
  put: (key, bytes, ct) => op_blob_put(String(key), bytes, ct === undefined ? null : String(ct)),
  get: (key) => op_blob_get(String(key)),
  del: (key) => op_blob_del(String(key)),
  url: (key) => op_blob_url(String(key)),
};

// ----- ws: WebSocket frame-loop control (send collected per frame, close ends conn; no-op outside WS) -----
globalThis.ws = {
  send: (data) => op_ws_send(String(data)),
  close: () => op_ws_close(),
};

// ----- bus: publish/subscribe (WS sessions subscribe; any handler publishes broadcast frames) -----
globalThis.bus = {
  publish: (topic, data) => op_bus_publish(String(topic), data === undefined ? null : data),
  subscribe: (topic) => op_bus_subscribe(String(topic)),
};

// ----- es: Elasticsearch thin client (search/index/del; es not configured errors) -----
globalThis.es = {
  search: (index, dsl) => op_es_search(String(index), dsl === undefined ? null : dsl),
  index: (index, id, doc) => op_es_index(String(index), String(id), doc === undefined ? null : doc),
  del: (index, id) => op_es_del(String(index), String(id)),
};

// ----- db / DB(name): named instances; JS-side cache guarantees identity (db === DB("default")) -----
const dbCache = new Map();
globalThis.DB = function (name) {
  name = String(name);
  if (!dbCache.has(name)) {
    if (!op_db_has(name)) return undefined;
    dbCache.set(name, {
      // raw SQL + bound params (params optional).
      query: (sql, params) => op_db_query(name, String(sql), params === undefined ? null : params),
      exec: (sql, params) => op_db_exec(name, String(sql), params === undefined ? null : params),
      // safe query builder: identifier whitelist + parameterized values.
      table: (t) => queryBuilder(name, String(t)),
      // transaction: db.tx(async (tx) => { await tx.exec(...); ... })
      // commit on resolve, rollback on throw/reject; tx rides the same connection
      // (query/exec/table route to the active tx). Nested tx is rejected by the op.
      tx: async (fn) => {
        await op_db_tx_begin(name);
        try {
          const out = await fn({
            query: (sql, params) => op_db_query(name, String(sql), params === undefined ? null : params),
            exec: (sql, params) => op_db_exec(name, String(sql), params === undefined ? null : params),
            table: (t) => queryBuilder(name, String(t)),
          });
          await op_db_tx_commit(name);
          return out;
        } catch (e) {
          await op_db_tx_rollback(name);
          throw e;
        }
      },
    });
  }
  return dbCache.get(name);
};
globalThis.db = globalThis.DB("default");

// ----- safe query builder (fluent, structured) -----
// usage: db.table("user").select(["id","name"]).where({field:"age",op:"gte",value:18})
//          .orderBy([{field:"id",dir:"desc"}]).limit(10).all()
function queryBuilder(name, table) {
  const req = { db: name, table, columns: [], conditions: [], order_by: [], limit: null, offset: null };
  const api = {
    select(cols) { req.columns = (cols || []).map(String); return api; },
    where(cond) { req.conditions.push(cond); return api; },
    orderBy(items) { req.order_by = (items || []).map((i) => ({ field: String(i.field), dir: i.dir ? String(i.dir) : null })); return api; },
    limit(n) { req.limit = n | 0; return api; },
    offset(n) { req.offset = n | 0; return api; },
    all() { return op_db_query_build(req); },
  };
  return api;
}

// ----- fetch: browser Fetch API compatible -----
function buildResponse(raw) {
  const bytes = new Uint8Array(raw.body);
  let consumed = false;
  return {
    ok: raw.ok,
    status: raw.status,
    statusText: raw.statusText,
    headers: raw.headers,
    json: async () => (raw.bodyText ? JSON.parse(raw.bodyText) : null),
    text: async () => raw.bodyText,
    arrayBuffer: async () => bytes,
    clone: () => buildResponse(raw),
    body: {
      getReader: () => ({
        read: async () =>
          consumed
            ? { done: true, value: undefined }
            : ((consumed = true), { done: false, value: bytes }),
      }),
    },
  };
}

globalThis.fetch = async function (url, options = {}) {
  if (!url) throw new Error("fetch: url is required");
  const method = String(options.method || "GET").toUpperCase();
  const headers = {};
  if (options.headers) {
    for (const k of Object.keys(options.headers)) {
      headers[k] = String(options.headers[k]);
    }
  }
  const body = options.body == null ? null : String(options.body);
  const raw = await op_fetch(String(url), method, headers, body);
  return buildResponse(raw);
};

// ----- finish: mark session done -----
globalThis.finish = () => op_finish();

// ----- __ojRequire: sync require() for CJS interop (eval + process-wide cache) -----
const __ojReqCache = new Map();
globalThis.__ojRequire = (name, referrerPath) => {
  const key = referrerPath + "::" + name;
  if (!__ojReqCache.has(key)) {
    const resolved = __oj_resolve_cjs(name, referrerPath); // op: returns {path, code}
    const fn = new Function("module", "exports", "require", resolved.code);
    const m = { exports: {} };
    fn(m, m.exports, (n) => globalThis.__ojRequire(n, resolved.path));
    __ojReqCache.set(key, m.exports);
  }
  return __ojReqCache.get(key);
};
