// ext:bridge_ext/bootstrap.js -- JS SDK globals.
// Rust ops do I/O and state; this file shapes the JS-side API
// (exposes the op_* bindings as JS globals).
//
// Globals: json / db / DB / http / redis / kv / log / fetch / finish / __ojRequire
//   + blob(name) / bus / es / ws / plugins / cert / jwt / bcrypt / oidc / crypto / mail / vars
// Plus safe query builder: db.table(name).select(...).where(...).orderBy(...).limit(...).all()
// Not ported yet: Redis(name) (named multi-KV-backend), XORM(name).

import { core, internals } from "ext:core/mod.js";
import {
  op_blob_del,
  op_blob_get,
  op_blob_put,
  op_blob_url,
  op_blob_content_type,
  op_bus_publish,
  op_bus_publish_bin,
  op_bus_subscribe,
  op_bus_kind,
  op_cert_gen,
  op_cert_renew,
  op_db_exec,
  op_db_next_seq,
  op_db_has,
  op_db_as_system,
  op_db_as_tenant,
  op_db_query,
  op_db_query_build,
  op_db_query_sql,
  op_db_tx_begin,
  op_db_tx_commit,
  op_db_tx_rollback,
  op_es_search,
  op_es_index,
  op_es_del,
  op_mq_call,
  op_mq_has,
  op_tasks_stopping,
  op_tasks_sleep,
  op_finish,
  op_http_info,
  op_http_file,
  op_http_body_bytes,
  op_json_fail,
  op_json_header,
  op_json_ok,
  op_json_raw,
  op_json_redirect,
  op_kv_get,
  op_kv_set,
  op_kv_del,
  op_kv_expire,
  op_kv_incr,
  op_log,
  op_mail_enqueue,
  op_mail_profiles,
  op_mail_result,
  op_mail_send,
  op_mail_send_raw,
  op_mail_send_sync,
  op_oidc_info,
  op_oidc_sign,
  op_oidc_verify,
  op_plugins,
  op_resolve_cjs as __oj_resolve_cjs,
  op_bcrypt_hash,
  op_bcrypt_verify,
  op_jwt_durations,
  op_jwt_sign,
  op_jwt_verify,
  op_random_hex,
  op_sha256_hex,
  op_vars_get,
  op_ws_send,
  op_ws_send_bin,
  op_ws_frame_close,
} from "ext:core/ops";

// Outbound WHATWG WebSocket client (deno_websocket ext). Registered by
// bridge::ws_client_extensions; see api-manual "WebSocket" section.
import { WebSocket as ojWsClient } from "ext:deno_websocket/01_websocket.js";

// WHATWG fetch (deno_fetch ext, v0.1.8): real Response/Headers, streaming body,
// AbortSignal. https/wss roots (webpki-roots) injected in ws_client_extensions.
// 26_fetch.js / 03_abort_signal.js are classic-script chunks (lazy_loaded_js),
// pulled via core.loadExtScript -- they are NOT ext: ESM modules.
// 26_fetch.js unconditionally pulls ext:deno_telemetry (hosted by the deno CLI,
// not shipped with deno_fetch); every touchpoint is gated on TRACING_ENABLED,
// so a no-op stub short-circuits it without registering that extension.
if (!internals.__telemetry) {
  internals.__telemetry = {
    TRACING_ENABLED: false,
    builtinTracer: () => null,
    ContextManager: { active: () => null },
    enterSpan: () => null,
    restoreSnapshot: () => {},
    PROPAGATORS: [],
  };
}
if (!internals.__telemetryUtil) {
  internals.__telemetryUtil = {
    updateSpanFromClientResponse: () => {},
    updateSpanFromError: () => {},
    updateSpanFromRequest: () => {},
  };
}
// WHATWG URL/URLSearchParams (deno_web 00_url.js): fetch JS parses URLs via
// `new URL(...)` -- the global would otherwise be undefined in this runtime.
const { URL: ojURL, URLSearchParams: ojURLSearchParams } = core.loadExtScript(
  "ext:deno_web/00_url.js",
);
globalThis.URL = ojURL;
globalThis.URLSearchParams = ojURLSearchParams;
const { fetch: ojFetch } = core.loadExtScript("ext:deno_fetch/26_fetch.js");
globalThis.fetch = ojFetch;
const { AbortController: ojAbortController } = core.loadExtScript(
  "ext:deno_web/03_abort_signal.js",
);
globalThis.AbortController = ojAbortController;

// ----- json: unified envelope + response headers -----
// BigInt-safe JSON.stringify (v0.1.22): serde_v8 hands i64 beyond 2^53-1 to JS as a
// BigInt, and JSON.stringify(1n) throws -- which used to turn any response carrying
// such a value into a 500. DB reads now arrive as decimal strings (see jsnum.rs), but
// values produced in JS (toBigInt) or coming back from ES can still be BigInt, so
// serialize every BigInt as its decimal string to match the wire contract.
function ojStringify(v) {
  return JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? x.toString() : x));
}

// ----- big integers (v0.1.22; u64 since v0.1.24): JS number is f64, so 64-bit ints need BigInt --
// Reads hand such values over as decimal strings (see jsnum.rs). toBigInt() / toUBigInt() convert
// back and are the only way to *write* a 64-bit integer precisely: serde_v8 rejects BigInt
// outright, so encodeParams() tags it for the host to bind as i64 / u64 (docs/numeric-limits.md).
const OJ_I64_KEY = "$oj$i64";
const OJ_U64_KEY = "$oj$u64";
const OJ_I64_MAX = 9223372036854775807n;
const OJ_I64_MIN = -9223372036854775808n;
const OJ_U64_MAX = 18446744073709551615n;

/** Signed 64-bit: [-2^63, 2^63-1]; out of range -> RangeError. */
globalThis.toBigInt = (v) => {
  if (typeof v === "bigint") {
    if (v > OJ_I64_MAX || v < OJ_I64_MIN) {
      throw new RangeError(
        "toBigInt: " + v.toString() + " is out of i64 range (use toUBigInt for MySQL BIGINT UNSIGNED)",
      );
    }
    return v;
  }
  if (typeof v === "number") {
    // Any |v| > 2^53-1 already sits on the f64 grid -- the original integer is gone.
    // Fail loud: Number("<big>") is exactly the trap this guards against.
    if (!Number.isSafeInteger(v)) {
      throw new TypeError(
        "toBigInt: " + v + " is not a safe integer (|v| > 2^53-1) and may already be lossy; pass the original decimal string instead",
      );
    }
    return BigInt(v);
  }
  if (typeof v === "string") {
    // Canonical decimal only: no leading zeros (except "0"), no "+1"/"-0"/spaces.
    // Mirrors the host-side check in oj-plugin-ffi/src/jsint.rs.
    if (!/^(0|-?[1-9][0-9]*)$/.test(v)) {
      throw new TypeError(
        "toBigInt: expected a canonical decimal integer string, got " + JSON.stringify(v),
      );
    }
    const b = BigInt(v);
    if (b > OJ_I64_MAX || b < OJ_I64_MIN) {
      throw new RangeError("toBigInt: " + v + " is out of i64 range");
    }
    return b;
  }
  throw new TypeError("toBigInt: expected string | number | bigint, got " + typeof v);
};

/** Unsigned 64-bit: [0, 2^64-1]. Only MySQL BIGINT UNSIGNED can hold it; PG/SQLite reject it. */
globalThis.toUBigInt = (v) => {
  if (typeof v === "bigint") {
    if (v < 0n || v > OJ_U64_MAX) {
      throw new RangeError("toUBigInt: " + v.toString() + " is out of u64 range");
    }
    return v;
  }
  if (typeof v === "number") {
    if (!Number.isSafeInteger(v) || v < 0) {
      throw new TypeError(
        "toUBigInt: " + v + " is not a non-negative safe integer; pass the original decimal string instead",
      );
    }
    return BigInt(v);
  }
  if (typeof v === "string") {
    if (!/^(0|[1-9][0-9]*)$/.test(v)) {
      throw new TypeError(
        "toUBigInt: expected a canonical non-negative decimal integer string, got " + JSON.stringify(v),
      );
    }
    const b = BigInt(v);
    if (b > OJ_U64_MAX) {
      throw new RangeError("toUBigInt: " + v + " is out of u64 range");
    }
    return b;
  }
  throw new TypeError("toUBigInt: expected string | number | bigint, got " + typeof v);
};

globalThis.toDouble = (v) => {
  if (typeof v === "number") return v;
  if (typeof v === "bigint") return Number(v);
  if (typeof v === "string") {
    const s = v.trim();
    const n = s === "" ? NaN : Number(s);
    if (Number.isNaN(n)) {
      throw new TypeError("toDouble: not a numeric string: " + JSON.stringify(v));
    }
    return n;
  }
  throw new TypeError("toDouble: expected string | number | bigint, got " + typeof v);
};

// BigInt -> wire marker for the host (see docs/numeric-limits.md). Range-checked here so an
// out-of-range bigint fails at the call site instead of silently binding as text.
// i64 range -> $oj$i64 (signed intent); (i64::MAX, u64::MAX] -> $oj$u64 (unsigned intent, the
// only way to carry MySQL BIGINT UNSIGNED precisely); beyond -> RangeError.
function intMarker(v) {
  if (v >= OJ_I64_MIN && v <= OJ_I64_MAX) {
    return { [OJ_I64_KEY]: v.toString() };
  }
  if (v > OJ_I64_MAX && v <= OJ_U64_MAX) {
    return { [OJ_U64_KEY]: v.toString() };
  }
  throw new RangeError(
    "db param: bigint " + v.toString() + " is out of 64-bit range (i64/u64)",
  );
}

// Deep-encode BigInt for the op boundary. Idempotent: markers written below are plain
// objects and pass through untouched, so req snapshots can be re-encoded safely.
function encodeParams(v) {
  if (typeof v === "bigint") return intMarker(v);
  if (Array.isArray(v)) return v.map(encodeParams);
  if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) {
    // Old path rejected these at serde_v8; fail loud instead of silently flattening a
    // Uint8Array into {"0":..,"1":..} (binary payloads belong in blob.put).
    throw new TypeError("db param: binary values are not supported (use blob.put)");
  }
  if (v !== null && typeof v === "object") {
    // JSON-aware values (Date, custom toJSON): follow JSON semantics -- encoding them
    // generically would flatten a Date to {} and lose the value.
    if (typeof v.toJSON === "function") return encodeParams(v.toJSON());
    const o = {};
    for (const k of Object.keys(v)) o[k] = encodeParams(v[k]);
    return o;
  }
  return v;
}

// Deep JSON-faithful clone with BigInt tagged (snapshot APIs: toJSON / subquery embedding).
// JSON round-trip keeps the semantics the old JSON.parse(JSON.stringify(req)) had -- notably
// Date -> ISO string; the recursive encoder above would flatten a Date to {}.
function encodeSnapshot(v) {
  return JSON.parse(
    JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? intMarker(x) : x)),
  );
}

globalThis.json = {
  // data is JSON.stringify'd on the JS side, so the op can splice it into the
  // envelope verbatim, avoiding the serde_v8 deserialize + serde_json re-serialize cost.
  ok: (data) => op_json_ok(data === undefined ? "null" : ojStringify(data)),
  fail: (code, msg, data) =>
    op_json_fail(code | 0, String(msg), data === undefined ? "null" : ojStringify(data)),
  header: (name, value) => op_json_header(String(name), String(value)),
  // 3xx redirect per RFC 9110 sec.15.4: Location + a short hypertext note with the
  // target link (empty body on HEAD; ignored by clients that auto-follow). Non-3xx
  // code falls back to 302.
  // Named helpers per RFC 9110 semantics:
  //   301 movedPermanently   permanent move (SEO weight transfers); cacheable
  //   302 found              temporary; the historical default (HTTP/1.0 "Moved Temporarily")
  //   303 seeOther           always re-fetch target with GET (correct post-POST redirect)
  //   307 temporaryRedirect  temporary; request method/body preserved
  //   308 permanentRedirect  permanent move; method/body preserved (301 with method kept)
  redirect: Object.assign(
    (url, code) => op_json_redirect(String(url), code === undefined ? 302 : code | 0),
    {
      movedPermanently: (url) => op_json_redirect(String(url), 301),
      found: (url) => op_json_redirect(String(url), 302),
      seeOther: (url) => op_json_redirect(String(url), 303),
      temporaryRedirect: (url) => op_json_redirect(String(url), 307),
      permanentRedirect: (url) => op_json_redirect(String(url), 308),
    },
  ),
  // bare JSON 200 (no envelope); OP external endpoints speak standard OIDC JSON.
  // Errors still go through fail() so callers can just test !res.ok on the envelope.
  raw: (data) => op_json_raw(data === undefined ? "null" : ojStringify(data)),
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
    // raw bytes of the current request / WS frame (text and binary frames alike).
    if (p === "bodyBytes") return () => op_http_body_bytes();
    return httpInfo()[p];
  },
});

// ----- log: structured logging (msg + alternating key/value pairs, like zap SugaredLogger) -----
function logCall(level, msg, kv) {
  const fields = {};
  for (let i = 0; i + 1 < kv.length; i += 2) fields[String(kv[i])] = kv[i + 1];
  // JSON.stringify once on the JS side, hand the JSON string straight to Rust
  // (avoid double serialization via serde_v8 + to_string). BigInt-safe (ojStringify).
  op_log(level, String(msg), ojStringify(fields));
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

// ----- blob: object storage (blob(name) named multi-backend; bare call = blob("default")) -----
globalThis.blob = (name) => ({
  put: (key, bytes, ct) => op_blob_put(String(name), String(key), bytes, ct === undefined ? null : String(ct)),
  get: (key) => op_blob_get(String(name), String(key)),
  del: (key) => op_blob_del(String(name), String(key)),
  url: (key) => op_blob_url(String(name), String(key)),
  contentType: (key) => op_blob_content_type(String(name), String(key)),
});
// back-compat: blob.put(...) === blob("default").put(...)
Object.assign(globalThis.blob, globalThis.blob("default"));

// ----- internal: mq op passthrough (Kafka/RabbitMQ/tasks globals bind on top) -----
globalThis.__ojMq = {
  call: op_mq_call,
  has: op_mq_has,
  stopping: op_tasks_stopping,
};

// ----- Kafka / RabbitMQ: named mq clients (per-name JS cache guarantees identity, like DB) -----
// Kind-shaped surfaces: kafka = send/poll/commit; rabbit = publish(alias of send)/poll/ack/nack.
// Consumers (poll/commit/ack/nack) are gated Rust-side to task contexts (long-running tasks).
// Binary payloads: value as Uint8Array/ArrayBuffer rides as value_b64 (base64, v0.1.16);
// poll messages with non-UTF-8 payloads come back with value_b64 set (value = null).
function b64FromBytes(u8) {
  const T = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let out = "";
  for (let i = 0; i < u8.length; i += 3) {
    const b0 = u8[i], b1 = u8[i + 1], b2 = u8[i + 2];
    out += T[b0 >> 2];
    out += T[((b0 & 3) << 4) | (b1 === undefined ? 0 : b1 >> 4)];
    out += b1 === undefined ? "=" : T[((b1 & 15) << 2) | (b2 === undefined ? 0 : b2 >> 6)];
    out += b2 === undefined ? "=" : T[b2 & 63];
  }
  return out;
}
function mqArgs(o) {
  if (o && o.value instanceof ArrayBuffer) return mqArgs({ ...o, value: new Uint8Array(o.value) });
  if (o && ArrayBuffer.isView(o.value)) {
    const { value, ...rest } = o;
    return { ...rest, value_b64: b64FromBytes(new Uint8Array(value.buffer, value.byteOffset, value.byteLength)) };
  }
  return o || {};
}
const mqCache = new Map();
function mqClient(kind) {
  return function (name) {
    name = String(name);
    const key = kind + " " + name;
    if (!mqCache.has(key)) {
      if (!op_mq_has(kind, name)) return undefined;
      const inst = {
        kind: () => op_mq_call(kind, name, "kind", null),
        metadata: () => op_mq_call(kind, name, "metadata", null),
      };
      if (kind === "kafka") {
        inst.send = (topic, o) => op_mq_call(kind, name, "send", { topic, ...mqArgs(o) });
        inst.poll = (topics, o) => op_mq_call(kind, name, "poll", { topics, ...(o || {}) });
        inst.commit = (m) => op_mq_call(kind, name, "commit", m);
      } else {
        inst.publish = (exchange, routingKey, value, o) =>
          op_mq_call(kind, name, "send", { exchange, routingKey, ...mqArgs({ value, ...(o || {}) }) });
        inst.poll = (queues, o) => op_mq_call(kind, name, "poll", { queues, ...(o || {}) });
        inst.ack = (m) => op_mq_call(kind, name, "ack", m);
        inst.nack = (m, requeue) => op_mq_call(kind, name, "nack", { ...m, requeue: !!requeue });
      }
      mqCache.set(key, inst);
    }
    return mqCache.get(key);
  };
}
globalThis.Kafka = mqClient("kafka");
globalThis.RabbitMQ = mqClient("rabbit");

// ----- tasks: long-running task context (stopping flag; always false outside task bridges) -----
// sleep is the only legal wait inside task loops (no timer globals in this runtime).
globalThis.tasks = {
  stopping: () => op_tasks_stopping(),
  sleep: (ms) => op_tasks_sleep(ms),
};

// ----- ws: WebSocket frame-loop control (send collected per frame, close ends conn; no-op outside WS) -----
// send: string -> text frame; Uint8Array/ArrayBuffer -> binary frame (opcode 0x2, v0.1.16).
globalThis.ws = {
  send: (data) =>
    typeof data === "string"
      ? op_ws_send(data)
      : op_ws_send_bin(data instanceof ArrayBuffer ? new Uint8Array(data) : data),
  close: () => op_ws_frame_close(),
};

// ----- WebSocket: outbound WHATWG client (tasks + handlers; no task-context gate:
// a connection carries no offset/ack consumer session, unlike MQ poll) -----
globalThis.WebSocket = ojWsClient;

// ----- bus: publish/subscribe (WS sessions subscribe; any handler publishes broadcast frames) -----
// kind() reports the active broker type ("local" | "kafka" | "rabbitmq") so handlers
// can detect distributed-event capability.
globalThis.bus = {
  // Uint8Array/ArrayBuffer -> binary broadcast (raw bytes to subscribers, v0.1.16);
  // anything else -> JSON envelope {"topic","data"} text frame.
  publish: (topic, data) =>
    data instanceof ArrayBuffer || ArrayBuffer.isView(data)
      ? op_bus_publish_bin(String(topic), data instanceof ArrayBuffer ? new Uint8Array(data) : data)
      : op_bus_publish(String(topic), data === undefined ? null : data),
  subscribe: (topic) => op_bus_subscribe(String(topic)),
  kind: () => op_bus_kind(),
};

// ----- es: Elasticsearch thin client (search/index/del; es not configured errors) -----
globalThis.es = {
  search: (index, dsl) => op_es_search(String(index), dsl === undefined ? null : dsl),
  index: (index, id, doc) => op_es_index(String(index), String(id), doc === undefined ? null : doc),
  del: (index, id) => op_es_del(String(index), String(id)),
};

// ----- mail / Mail(key): SMTP send (host validates + resolves attachment bytes; the
// oj-mail plugin owns transports/queue). All methods resolve the {code,msg,data} envelope
// -- validation failures come back as {code:5} (no throw); only "mail not configured"
// throws. Delivery errors: 1 network/timeout (and every delivery-time failure, incl.
// SMTP 5xx and auth); 4 queue full; 5 bad input. 2 (smtp 5xx) and 3 (auth) are RESERVED
// / not enabled -- both fold into 1, so test failures with `code !== 0`.
// enqueue() resolves {code:0,data:{jobId}} and the real completion is published on the
// bus topic "mail.result" as the flat {jobId,code,msg,messageId} (no to/subject).
// jobId is HOST-generated and unguessable (a caller-supplied jobId is stripped);
// result(jobId) only returns results owned by this caller (profile + module + tenant).
// attachments[i]: {filename, blobKey|path, mime?} -- blobKey rides the blob registry,
// path must stay inside the project root.
globalThis.Mail = class {
  constructor(key = "default") { this.key = key; }
  send(m) { return op_mail_send(this.key, ojStringify(m)); }
  sendSync(m) { return op_mail_send_sync(this.key, ojStringify(m)); }
  enqueue(m) { return op_mail_enqueue(this.key, ojStringify(m)); }
  result(id) { return op_mail_result(this.key, String(id)); }
  sendRaw(o) { return op_mail_send_raw(this.key, ojStringify(o)); }
  static profiles() { return op_mail_profiles(); }
};
const ojMailDefault = new Mail("default");
globalThis.mail = ojMailDefault;

// ----- plugins: loaded plugin introspection (name/semver/abi/fingerprint + host ABI) -----
globalThis.plugins = () => op_plugins();

// ----- vars: deployment-time constants (config `vars:` section) -----
// Synchronous read (no I/O -- the table is frozen at assembly). FAIL-CLOSED: only keys
// declared under `vars:` are readable, anything else returns null; there is no "read
// arbitrary OS env / arbitrary config key" channel. Values are the YAML scalars as text
// (PORT: 3000 -> "3000"; nested maps/lists are a config parse error). Typical use --
// deployment decides, business code never hardcodes a release constant:
//   const web = vars.get("WEB_URL") ?? "http://localhost:3000";
globalThis.vars = {
  get: (name) => op_vars_get(String(name)),
};

// ----- db / DB(name): named instances; JS-side cache guarantees identity (db === DB("default")) -----
// ----- condition tree factory (pure JSON tree; compose/inspect in JS, zero new ops) -----
function unwrapCond(c) { return c && typeof c.tree === "function" ? c.tree() : c; }
// builder -> plain req snapshot (subquery/exists embedding passes builders where a
// tree is expected); non-builders pass through untouched.
function unwrapSub(v) {
  return v && v.__req ? encodeSnapshot(v.__req) : v;
}
// deep-unwrap a condition tree: condObj -> plain tree, builders -> req snapshots,
// recursing into and/or arrays and not/subquery/exists slots.
function unwrapTree(t) {
  t = unwrapCond(t);
  if (!t || typeof t !== "object" || Array.isArray(t)) return t;
  if (t.__req) return unwrapTree(unwrapSub(t));
  const o = {};
  for (const k of Object.keys(t)) {
    if (k === "and" || k === "or") o[k] = t[k].map(unwrapTree);
    else if (k === "not" || k === "subquery" || k === "exists") o[k] = unwrapTree(unwrapSub(t[k]));
    else o[k] = t[k];
  }
  return o;
}
function condObj(tree) {
  const api = {
    tree: () => tree,
    and: (...cs) => condObj({ and: [tree, ...cs.map(unwrapCond)] }),
    or: (...cs) => condObj({ or: [tree, ...cs.map(unwrapCond)] }),
    not: () => condObj({ not: tree }),
    fields: () => {
      const out = [];
      (function walk(t) {
        if (t && typeof t === "object") {
          if (t.field !== undefined) out.push(String(t.field));
          for (const k of ["and", "or"]) if (Array.isArray(t[k])) t[k].forEach(walk);
          if (t.not) walk(t.not);
        }
      })(tree);
      return [...new Set(out)];
    },
    has(f) { return api.fields().includes(String(f)); },
  };
  return api;
}
function condFactories() {
  return {
    leaf: (field, op, value) => condObj({ field: String(field), op: String(op), value }),
    and: (...cs) => condObj({ and: cs.map(unwrapCond) }),
    or: (...cs) => condObj({ or: cs.map(unwrapCond) }),
    not: (c) => condObj({ not: unwrapCond(c) }),
  };
}
const dbCache = new Map();
globalThis.DB = function (name) {
  name = String(name);
  if (!dbCache.has(name)) {
    if (!op_db_has(name)) return undefined;
    dbCache.set(name, {
      ...condFactories(),
      // raw SQL + bound params (params optional).
      query: (sql, params) => op_db_query(name, String(sql), params === undefined ? null : encodeParams(params)),
      exec: (sql, params) => op_db_exec(name, String(sql), params === undefined ? null : encodeParams(params)),
      // safe query builder: identifier whitelist + parameterized values.
      table: (t) => queryBuilder(name, String(t)),
      // system escape hatch (tenant sql_guard): this request bypasses tenant
      // injection/checks. Explicit + audited; business handlers must not use it.
      asSystem: () => { op_db_as_system(); return dbCache.get(name); },
      // anonymous-request tenant declaration (v0.1.20): the handler states which
      // tenant this request queries. Only valid on anonymous requests (tenant
      // anonymous_paths hit, no tenant header) and only when tenant.allow_as_tenant
      // is on. Unlike asSystem, tenant conditions are STILL enforced.
      asTenant: (id) => { op_db_as_tenant(String(id)); return dbCache.get(name); },
      // rebuild a builder from a toJSON() snapshot (continues the chain on this db).
      fromJSON: (snap) => builderFromReq(encodeParams(snap)),
      // platform sequence allocator (v0.1.24): single-statement atomic next value.
      // Race-free replacement for `select max(id) + 1`; the platform table
      // `_oj_sequences` is created on first use (see docs/db-guide.md).
      nextSeq: (n) => op_db_next_seq(name, String(n)),
      // transaction: db.tx(async (tx) => { await tx.exec(...); ... })
      // commit on resolve, rollback on throw/reject; tx rides the same connection
      // (query/exec/table route to the active tx). Nested tx is rejected by the op.
      tx: async (fn) => {
        await op_db_tx_begin(name);
        try {
          const out = await fn({
            ...condFactories(),
            query: (sql, params) => op_db_query(name, String(sql), params === undefined ? null : encodeParams(params)),
            exec: (sql, params) => op_db_exec(name, String(sql), params === undefined ? null : encodeParams(params)),
            table: (t) => queryBuilder(name, String(t)),
            // Same-connection allocator: MySQL's LAST_INSERT_ID is session-scoped.
            nextSeq: (n) => op_db_next_seq(name, String(n)),
            fromJSON: (snap) => builderFromReq(encodeParams(snap)),
            asSystem: () => { op_db_as_system(); return dbCache.get(name); },
            asTenant: (id) => { op_db_as_tenant(String(id)); return dbCache.get(name); },
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
// builderFromReq(snap) rebuilds a builder from a plain req snapshot (see db.fromJSON);
// snapshots are opaque req objects: identity/db binding is re-pointed at the restoring
// module's bound db (snapshot `db` is only the JS-visible name it was created with).
function builderFromReq(snap) {
  const req = Object.assign(
    { db: "default", table: "", columns: [], conditions: [], order_by: [], limit: null, offset: null, verb: "select", values: [], sets: {}, joins: [], group_by: [], having: null, distinct: false, unions: [], with: [], returning: [] },
    snap,
  );
  req.db = String(req.db); req.table = String(req.table);
  const api = {
    select(cols) {
      req.columns = (cols || []).map((c) => {
        if (typeof c === "string") return String(c);
        // deep-unwrap case when conds (condObj/builder, same as where/having)
        if (c && c.case && c.case.when) {
          // CASE carries values too (when[].then / else) -- encode the whole node.
          return encodeParams({ ...c, case: { ...c.case, when: c.case.when.map((w) => ({ ...w, cond: unwrapTree(w.cond) })) } });
        }
        return { ...c };
      });
      return api;
    },
    where(cond) { req.conditions.push(encodeParams(unwrapTree(cond))); return api; },
    orderBy(items) { req.order_by = (items || []).map((i) => ({ field: String(i.field), dir: i.dir ? String(i.dir) : null })); return api; },
    limit(n) { req.limit = n | 0; return api; },
    offset(n) { req.offset = n | 0; return api; },
    all() { return op_db_query_build(req); },
    insert(rows) { req.verb = "insert"; req.values = (Array.isArray(rows) ? rows : [rows]).map((r) => encodeParams({ ...r })); return api; },
    // Insert returning clause (whitelisted columns, e.g. ["id"]): run() then resolves
    // to a row array [{id: n}] instead of the affected-row count. pg/sqlite render a
    // single sea-query RETURNING statement; mysql has no RETURNING and takes
    // LAST_INSERT_ID() in a second step on the same connection (use db.tx for safety).
    returning(cols) { req.returning = (cols || []).map(String); return api; },
    update(sets) { req.verb = "update"; req.sets = encodeParams({ ...sets }); return api; },
    delete() { req.verb = "delete"; return api; },
    join(table, on, kind) { req.joins.push({ table: String(table), on: (on || []).map((p) => ({ left: String(p.left), right: String(p.right) })), kind: kind ? String(kind) : "inner" }); return api; },
    distinct() { req.distinct = true; return api; },
    groupBy(cols) { req.group_by = (cols || []).map(String); return api; },
    having(cond) { req.having = encodeParams(unwrapTree(cond)); return api; },
    union(other, kind) { req.unions.push({ kind: kind ? String(kind) : "distinct", query: unwrapSub(other) }); return api; },
    with(name, columns, query) { req.with.push({ name: String(name), columns: (columns || []).map(String), query: unwrapSub(query) }); return api; },
    run() {
      if ((req.verb === "update" || req.verb === "delete") && req.conditions.length === 0) {
        throw new Error(req.verb + " requires where");
      }
      if (req.verb === "insert" && req.values.length === 0) {
        throw new Error("insert needs at least one row");
      }
      return op_db_query_build(req);
    },
    toSQL() { return op_db_query_sql(req); },
    toJSON() { return encodeSnapshot(req); },
    // Internal: expose req for subquery/union/cte embedding (not documented API).
    __req: req,
  };
  return api;
}
function queryBuilder(name, table) {
  return builderFromReq({ db: name, table });
}

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

// ----- cert: JWS certificate issue/renew (RSA keygen + RS256 signing live in Rust) -----
// generate -> {private_pem, public_pem, cert_jws}; renew -> new cert_jws (same public key).
globalThis.cert = {
  generate: (bits, nbf, exp) => op_cert_gen(bits | 0, nbf, exp),
  renew: (privatePem, nbf, exp) => op_cert_renew(String(privatePem), nbf, exp),
};

// ----- jwt: sign/verify (secret/alg/durations injected at assembly; not configured errors) -----
globalThis.jwt = {
  sign: (claims) => op_jwt_sign(claims === undefined ? null : claims),
  verify: (token) => op_jwt_verify(String(token)),
  get accessDuration() { return op_jwt_durations().access; },
  get refreshDuration() { return op_jwt_durations().refresh; },
};

// ----- bcrypt: password hashing (spawn_blocking on Rust side) -----
globalThis.bcrypt = {
  hash: (password, cost) => op_bcrypt_hash(String(password), cost === undefined ? null : cost | 0),
  verify: (password, hash) => op_bcrypt_verify(String(password), String(hash)),
};

// ----- oidc: RS256 sign/verify primitives + assembly-time config (keys stay in Rust) -----
globalThis.oidc = (() => {
  const info = () => op_oidc_info();
  return {
    sign: (claims) => op_oidc_sign(claims === undefined ? null : claims),
    verify: (token, jwks) => op_oidc_verify(String(token), jwks === undefined ? null : jwks),
    jwks: () => info().jwks,
    get issuer() { return info().issuer; },
    get rp() { return info().rp; },
    get clients() { return info().clients; },
  };
})();

// ----- crypto: sha256/random helpers (merge, keep native getRandomValues if present) -----
globalThis.crypto = Object.assign(globalThis.crypto || {}, {
  sha256Hex: (s) => op_sha256_hex(String(s)),
  randomHex: (n) => op_random_hex(n === undefined ? null : n | 0),
});
