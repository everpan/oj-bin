// ext:bridge_ext/bootstrap.js -- JS SDK globals.
// Rust ops do I/O and state; this file shapes the JS-side API
// (exposes the op_* bindings as JS globals).
//
// Globals: json / db / DB / http / redis / kv / log / fetch / finish / __ojRequire
//   + blob(name) / bus / es / ws / plugins / cert / jwt / bcrypt / oidc / crypto / mail / vars
//   + ldap / LDAP(name)
// Plus safe query builder: db.table(name).select(...).where(...).orderBy(...).limit(...).all()
// Not ported yet: Redis(name) (named multi-KV-backend), XORM(name).

import { core, internals } from "ext:core/mod.js";
import {
  op_blob_del,
  op_blob_get,
  op_blob_put,
  op_blob_url,
  op_blob_upload_url,
  op_blob_content_type,
  op_blob_copy,
  op_blob_move,
  op_blob_read_range,
  op_bus_publish,
  op_bus_publish_bin,
  op_bus_subscribe,
  op_bus_kind,
  op_cert_gen,
  op_cert_renew,
  op_db_exec,
  op_db_next_seq,
  op_db_sql_profile,
  op_db_has,
  op_db_allowed,
  op_db_as_system,
  op_db_as_tenant,
  op_db_query,
  op_db_query_build,
  op_db_query_sql,
  op_db_dialect,
  op_db_list_tables,
  op_db_list_columns,
  op_db_list_indexes,
  op_db_list_foreign_keys,
  op_db_client_register,
  op_db_client_test,
  op_db_client_delete,
  op_db_client_re_register,
  op_db_client_list,
  op_db_client_has,
  op_db_access_grant,
  op_db_access_revoke,
  op_db_epoch,
  op_audit_query,
  op_db_session_begin,
  op_db_session_attach,
  op_db_session_detach,
  op_db_session_query,
  op_db_session_exec,
  op_db_session_commit,
  op_db_session_rollback,
  op_db_session_close,
  op_db_session_touch,
  op_db_session_close_all,
  op_db_session_list,
  op_db_session_info,
  op_db_session_stats,
  op_db_tx_begin,
  op_db_tx_commit,
  op_db_tx_rollback,
  op_db_cancel,
  op_db_catalog,
  op_db_list_schemas,
  op_db_list_databases,
  op_mgmt_register,
  op_mgmt_delete,
  op_mgmt_list,
  op_mgmt_has,
  op_mgmt_call,
  op_secret_seal,
  op_secret_open,
  op_db_stream_open,
  op_db_stream_next,
  op_db_stream_info,
  op_db_export,
  op_db_export_status,
  op_db_export_cancel,
  op_db_stream_close,
  op_db_stream_abort,
  op_es_search,
  op_es_index,
  op_es_del,
  op_mq_call,
  op_mq_has,
  op_tasks_stopping,
  op_tasks_sleep,
  op_finish,
  op_http_info,
  op_http_epoch,
  op_driver_error,
  op_log_enabled,
  op_http_file,
  op_http_body_bytes,
  op_json_fail,
  op_json_header,
  op_json_ok,
  op_json_raw,
  op_json_redirect,
  op_json_stream_open,
  op_stream_write,
  op_stream_write_bytes,
  op_stream_end,
  op_kv_get,
  op_kv_set,
  op_kv_del,
  op_kv_expire,
  op_kv_incr,
  op_ldap_call,
  op_log,
  op_mail_enqueue,
  op_mail_profiles,
  op_mail_result,
  op_mail_send,
  op_mail_send_raw,
  op_mail_send_sync,
  op_oidc_info,
  op_sso_providers,
  op_oidc_sign,
  op_oidc_verify,
  op_plugins,
  op_oj_info,
  op_axis_call,
  op_resolve_cjs as __oj_resolve_cjs,
  op_bcrypt_hash,
  op_bcrypt_verify,
  op_jwt_durations,
  op_jwt_sign,
  op_jwt_verify,
  op_random_hex,
  op_sha256_hex,
  op_sha256_bytes,
  op_md5_hex,
  op_md5_bytes,
  op_ed25519_verify,
  op_crypto_random,
  op_aes_gcm_encrypt,
  op_aes_gcm_decrypt,
  op_vars_get,
  op_ws_send,
  op_ws_send_bin,
  op_ws_frame_close,
  op_ws_join,
  op_ws_leave,
  op_ws_broadcast,
  op_ws_room_size,
  op_fs_resolve,
  op_exec_run,
  op_exec_spawn,
  op_exec_read,
  op_exec_kill,
  op_exec_exit,
  op_close,
  op_yaml_parse,
  op_yaml_stringify,
  op_server_reload_config,
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
// DOMException (deno_web 01_dom_exception.js): deno_web's AbortSignal.abort()
// constructs `new DOMException(...)` as the abort reason -- without this global
// `ac.abort()` threw ReferenceError and signal-driven cancellation never fired.
const { DOMException: ojDOMException } = core.loadExtScript(
  "ext:deno_web/01_dom_exception.js",
);
globalThis.DOMException = ojDOMException;
const { fetch: ojFetch } = core.loadExtScript("ext:deno_fetch/26_fetch.js");
globalThis.fetch = ojFetch;
const { AbortController: ojAbortController } = core.loadExtScript(
  "ext:deno_web/03_abort_signal.js",
);
globalThis.AbortController = ojAbortController;

// ----- fs: local file system (v0.1.53, deno_fs) -----
// One-shot APIs only -- no fd handles (ops open/close within the call, so a
// pooled runtime can never leak file descriptors across requests). Access is
// jailed to the config `fs:` root by the PermissionsContainer injected in the
// bridge_ext state closure; without an `fs:` section every call throws
// PermissionDenied. TextEncoder/Decoder are prerequisites of the 30_fs.js
// text helpers and are also useful to handlers, hence mounted globally.
const { TextEncoder: ojTextEncoder, TextDecoder: ojTextDecoder } =
  core.loadExtScript("ext:deno_web/08_text_encoding.js");
globalThis.TextEncoder = ojTextEncoder;
globalThis.TextDecoder = ojTextDecoder;
const ojFs = core.loadExtScript("ext:deno_fs/30_fs.js");
// Every path goes through op_fs_resolve: it folds relative paths against the
// jail root (deno's native semantics resolve against the process cwd, which
// breaks the jail model), then best-effort canonicalizes and issues the jail
// verdict (outside fs.root / fs: absent -> NotCapable), returning the
// canonical absolute path for the deno op (whose own permission check is
// the second line of defense). Joining must stay in Rust: string-concat in
// JS mixes "/" into Windows verbatim roots (\\?\\C:\\...), which disables
// separator normalization and defeats leaf-symlink resolution.
const ojFsResolve = (p) => (typeof p === "string" ? op_fs_resolve(p) : p);
globalThis.fs = {
  readFile: (p, o) => ojFs.readFile(ojFsResolve(p), o),
  readTextFile: (p, o) => ojFs.readTextFile(ojFsResolve(p), o),
  writeFile: (p, d, o) => ojFs.writeFile(ojFsResolve(p), d, o),
  writeTextFile: (p, d, o) => ojFs.writeTextFile(ojFsResolve(p), d, o),
  mkdir: (p, o) => ojFs.mkdir(ojFsResolve(p), o),
  remove: (p, o) => ojFs.remove(ojFsResolve(p), o),
  rename: (a, b) => ojFs.rename(ojFsResolve(a), ojFsResolve(b)),
  stat: (p) => ojFs.stat(ojFsResolve(p)),
  readDir: (p) => ojFs.readDir(ojFsResolve(p)),
};

// ----- exec: external command primitive (argv templates; ojm batch) -----
// Runs ONLY an authorized argv template: literal tokens must match exactly and
// typed placeholders absorb exactly one token each (${path:NAME} is checked
// against fs.root + trusted_paths; ${raw:NAME} rejects option-like/absolute
// forms). No shell. cwd is fs.root; env is empty except the configured
// whitelist (values never enter JS); stdout+stderr combined 10MB cap; timeout
// defaults to 30s, hard cap 600s (larger values are clamped). A process that
// runs to exit (ANY code) resolves {code, stdout, stderr} (signal death maps
// to 128+signal); authorization/template/timeout/limit/spawn failures throw.
// ----- exec: external command execution (ojm batch) -----
// Two shapes (design doc 2026-10-17-exec-streaming-design.md):
//  - exec.run(argv, opts?)     buffered: returns { code, stdout, stderr } when no
//                              onStdout/onStderr; when opts carries onStdout/
//                              onStderr, streams each chunk to the callback and
//                              resolves to { code, stdout, stderr }.
//  - exec.spawn(argv, opts?)   handle: { rid, pid, stdout, stderr, kill, exit };
//                              stdout/stderr are AsyncIterable<Uint8Array>.
// CONTRACT: the AsyncIterable streams MUST be fully consumed (for-await) or
// backpressure blocks the child (bounded pipe, 16*8KiB). The buffered/callback
// forms drain both streams concurrently so they never deadlock.

async function spawnExec(argv, opts) {
  const o = await op_exec_spawn(argv, opts === undefined ? null : opts);
  const mkStream = (rid) => {
    return (async function* () {
      try {
        while (true) {
          const chunk = await op_exec_read(rid, 65536);
          if (chunk.byteLength === 0) break;
          yield chunk;
        }
      } finally {
        op_close(rid);
      }
    })();
  };
  return {
    rid: o.rid,
    pid: o.pid,
    stdout: mkStream(o.stdout_rid),
    stderr: mkStream(o.stderr_rid),
    kill: () => op_exec_kill(o.rid),
    exit: async () => {
      const r = await op_exec_exit(o.rid);
      op_close(o.rid);
      return r;
    },
  };
}

async function runExecStreaming(argv, opts) {
  const h = await spawnExec(argv, opts);
  const dec = new TextDecoder();
  const outBuf = [];
  const errBuf = [];
  const sink = (stream, buf, cb) => {
    return (async () => {
      for await (const chunk of stream) {
        buf.push(chunk);
        if (cb) cb(chunk);
      }
    })();
  };
  const t1 = sink(h.stdout, outBuf, opts.onStdout);
  const t2 = sink(h.stderr, errBuf, opts.onStderr);
  const r = await h.exit();
  await Promise.all([t1, t2]);
  const merge = (parts) => {
    let len = 0;
    for (const p of parts) len += p.byteLength;
    const u = new Uint8Array(len);
    let off = 0;
    for (const p of parts) {
      u.set(p, off);
      off += p.byteLength;
    }
    return dec.decode(u);
  };
  return { code: r.code, stdout: merge(outBuf), stderr: merge(errBuf) };
}

globalThis.exec = {
  run: (argv, opts) => {
    if (opts && (opts.onStdout || opts.onStderr)) {
      return runExecStreaming(argv, opts);
    }
    return op_exec_run(argv, opts === undefined ? null : opts);
  },
  spawn: (argv, opts) => spawnExec(argv, opts),
};

// ----- yaml: bounded parse / stringify (ojm batch) -----
// parse runs on the blocking pool with its own 5s deadline behind three budgets
// (1 MiB input, 100k nodes incl. alias replay, nesting depth 128); errors are
// named (budget exceeded / line+column for syntax). Out-of-range integers arrive
// as decimal strings; duplicate mapping keys: last one wins. stringify is for
// MACHINE ARTIFACTS ONLY (manifests.yaml round-trip) -- it loses comments, so
// never use it to rewrite config.yaml (do text surgery instead).
globalThis.yaml = {
  parse: (text) => op_yaml_parse(String(text)),
  stringify: (value) => op_yaml_stringify(value === undefined ? null : value),
};

// ----- server: platform-level controls (ojm batch) -----
// reloadConfig triggers the in-process config hot reload. It shares the SAME
// executor instance and the SAME reload lock as the HTTP POST
// {base}/_system/config endpoint: a concurrent trigger fails with a busy
// error (409 semantics, never silently queued). Requires the caller module to
// be platform-privileged (config platform.modules); outside `oj serve`
// (oj test / embedded) it throws "reload not available". While the reload
// runs (seconds: DB verify + transpile) the actor executing this handler is
// occupied and is not returned to the pool until it completes.
globalThis.server = {
  reloadConfig: () => op_server_reload_config(),
};

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
  // Streaming response (v0.1.35): open a channel and push chunks; bypasses the
  // {code,msg,data} envelope (first byte is the raw body). opts: { status?, contentType? }.
  stream: (opts) => {
    op_json_stream_open(opts === undefined ? null : opts);
    return {
      // Binary (Uint8Array/ArrayBuffer/TypedArray/DataView) goes through the byte
      // op; everything else is written as UTF-8 text (CSV/JSON/SSE rows). Only
      // Uint8Array passes the op's #[buffer] binding directly -- other shapes are
      // converted here, else ArrayBuffer/views die with TypeError (v0.1.66 fix).
      write: (c) => {
        if (c instanceof Uint8Array) return op_stream_write_bytes(c);
        if (c instanceof ArrayBuffer) return op_stream_write_bytes(new Uint8Array(c));
        if (ArrayBuffer.isView(c))
          return op_stream_write_bytes(new Uint8Array(c.buffer, c.byteOffset, c.byteLength));
        return op_stream_write(String(c));
      },
      end: () => op_stream_end(),
    };
  },
  // SSE convenience over stream(): sets text/event-stream and frames writes as
  // `data: <c>\n\n`. opts: { status?, heartbeatSecs? }.
  sse: (opts) => {
    const o = opts === undefined ? {} : Object.assign({}, opts);
    o.sse = true;
    op_json_stream_open(o);
    return {
      write: (c) => op_stream_write("data: " + String(c) + "\n\n"),
      end: () => op_stream_end(),
    };
  },
};

// ----- http helpers: current request context (lazy proxy; fresh per request) -----
// Snapshot memo: op_http_info deep-copies the whole RequestInfo (headers/query clone +
// body re-parse) per call. Handlers touch http.* many times per request, so cache the
// last snapshot and re-pull only when the Rust-side request epoch (ReqState.epoch,
// bumped on reset and on mid-request req mutation) moves on.
let httpMemo = null;
let httpMemoEpoch = -1;
const httpInfo = () => {
  const ep = op_http_epoch();
  if (ep !== httpMemoEpoch) {
    httpMemo = op_http_info();
    httpMemoEpoch = ep;
  }
  return httpMemo;
};
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
    // module identity (name + version) injected at route dispatch; frozen so a
    // handler cannot rewrite the framework's trust basis (ownership guard / bound_db
    // / platform privilege swap all key off it). null when there is no module
    // context (task bridge / WS frames / run_with / introspection).
    if (p === "module") {
      const m = httpInfo().module;
      return m === null ? null : Object.freeze(m);
    }
    return httpInfo()[p];
  },
});

// ----- log: structured logging (msg + alternating key/value pairs, like zap SugaredLogger) -----
function logCall(level, msg, kv) {
  // Level gate: skip fields build + stringify entirely when tracing filters the
  // level out (debug under production info is otherwise pure waste).
  if (!op_log_enabled(level)) return;
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
  // uploadUrl(key, opts?) -> presigned direct-upload JSON (v0.1.30); s3 only
  // (local backend: use the direct PUT route instead; the error says so).
  uploadUrl: (key, opts) => op_blob_upload_url(String(name), String(key), opts === undefined ? null : JSON.stringify(opts)),
  contentType: (key) => op_blob_content_type(String(name), String(key)),
  // copy(src, dst): server-side copy, src is kept (v0.1.47, ABI 11).
  // move(src, dst): src is gone -- local renames when both keys share a parent dir.
  copy: (src, dst) => op_blob_copy(String(name), String(src), String(dst)),
  move: (src, dst) => op_blob_move(String(name), String(src), String(dst)),
  // readRange(key, offset, len) -> Uint8Array; short read is truncated at EOF
  // (empty array when offset is past the end).
  readRange: (key, offset, len) => op_blob_read_range(String(name), String(key), Number(offset), Number(len)),
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
// send/broadcast: string -> text frame; Uint8Array/ArrayBuffer -> binary frame (opcode 0x2, v0.1.16).
// rooms (v0.1.30): same-process fanout. join/leave need a ws handler context (sess id);
// broadcast also works from HTTP handlers (conn = 0 -> exclude nobody); echo to self is
// the caller's job (socket.io semantics). roomSize is callable anywhere.
globalThis.ws = {
  send: (data) =>
    typeof data === "string"
      ? op_ws_send(data)
      : op_ws_send_bin(data instanceof ArrayBuffer ? new Uint8Array(data) : data),
  close: () => op_ws_frame_close(),
  join: (room) => op_ws_join(String(room), wsConnId(true)),
  leave: (room) => op_ws_leave(String(room), wsConnId(true)),
  broadcast: (room, data) =>
    typeof data === "string"
      ? op_ws_broadcast(String(room), data, wsConnId(false))
      : op_ws_broadcast_bin(
          String(room),
          data instanceof ArrayBuffer ? new Uint8Array(data) : data,
          wsConnId(false),
        ),
  roomSize: (room) => op_ws_room_size(String(room)),
};

// WS connection id injected per frame (__ws_conn); strict = throw outside ws handler.
function wsConnId(strict) {
  const c = globalThis.__ws_conn;
  if (typeof c === "number" && c > 0) return c;
  if (strict) throw new Error("ws.join/leave: available only inside a ws handler");
  return 0;
}

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

// ----- ldap / LDAP(key): directory search + bind-as-auth (the oj-ldap plugin owns the
// ldap3 connections; host validates args). All methods reject on validation/protocol/
// network errors; only "ldap not configured" also throws at lookup. ldap.bind(dn,pw)
// resolves true/false (false = LDAP refused the credentials, rc != 0); search resolves
// [{dn, attrs:{k:[v]}, bin:{k:[base64]}}] (bin = binary attributes, base64-encoded).
globalThis.LDAP = class {
  constructor(key = "default") { this.key = key; }
  bind(dn, pw) {
    return op_ldap_call(ojStringify({ op: "bind", key: this.key, dn: String(dn), pw: String(pw) }));
  }
  search(base, opts = {}) {
    const o = opts || {};
    return op_ldap_call(ojStringify({
      op: "search", key: this.key, base: String(base),
      scope: o.scope, filter: o.filter, attrs: o.attrs,
      bindDn: o.bindDn, bindPw: o.bindPw,
    }));
  }
  searchPaged(base, opts = {}) {
    const o = opts || {};
    return op_ldap_call(ojStringify({
      op: "search_paged", key: this.key, base: String(base),
      scope: o.scope, filter: o.filter, attrs: o.attrs, page_size: o.pageSize,
      bindDn: o.bindDn, bindPw: o.bindPw,
    }));
  }
  whoami() {
    return op_ldap_call(ojStringify({ op: "whoami", key: this.key }));
  }
  compare(dn, attr, val) {
    return op_ldap_call(ojStringify({
      op: "compare", key: this.key, dn: String(dn), attr: String(attr), val: String(val),
    }));
  }
};
const ojLdapDefault = new LDAP("default");
globalThis.ldap = ojLdapDefault;

// ----- plugins: loaded plugin introspection (name/semver/abi/fingerprint + host ABI) -----
globalThis.plugins = () => op_plugins();

// ----- ojInfo: assembly-frozen diagnostics (same source as the `oj info` CLI) -----
// Values are names only (config sections/keys, no values); there is no public HTTP
// endpoint for this -- handlers call ojInfo() in-process.
globalThis.ojInfo = () => op_oj_info();

// ----- sso: HTTP form / CAS SSO provider registry (assembly-time http_sso: config) -----
// sso.providers -> map of provider name -> cfg; missing section throws
// "http sso not configured" (same semantics as oidc).
globalThis.sso = {
  get providers() { return op_sso_providers(); },
};

// ----- axis: generic plugin axes (self-reported names outside the typed axes) -----
// axis("cache").get("k") -> op_axis_call("cache", "get", "[\"k\"]"); unknown axis
// throws listing the available names. ojStringify is BigInt-safe (plain
// JSON.stringify throws on BigInt). "then" guard: keeps the proxy thenable-safe.
globalThis.axis = (name) => new Proxy({}, {
  get: (_, op) => {
    if (typeof op !== "string" || op === "then") return undefined;
    return (...args) => op_axis_call(String(name), op, ojStringify(args));
  },
});

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
// U-3 v0.1.93: per-alias generation counter tracked separately from the wrapper so
// DB(name) can revalidate on EVERY call (not just on cache-miss). Kept in a module-local
// Map; correctness never depends on this value (query ops re-resolve the accessor by
// name in the host), it only refreshes the JS wrapper promptly after delete/reRegister.
const dbEpoch = new Map();

// T9: structured db errors (BridgeError) carry `code`/`data`/`retryable`/
// `sessionDisposition` as own props on the JS Error (attached by deno_core). `data`
// arrives as a JSON string; parse it in place so handlers can read
// `e.data.capability` directly. `retryable` arrives as 1/0 (PropertyValue only knows
// String/Number) and is normalized to a boolean here. name/message are preserved
// verbatim (existing catch text matches stay byte-for-byte unchanged).
function normDbErr(e) {
  if (!e) { return e; }
  if (typeof e.data === "string") {
    try { e.data = JSON.parse(e.data); } catch (_) {}
  }
  if (typeof e.retryable === "number") {
    e.retryable = e.retryable !== 0;
  }
  return e;
}

// single entry point for db ops. async ops (query/exec/nextSeq/listTables/query_build)
// surface errors as Promise rejections that an outer try/catch cannot catch, so we
// attach a rejection handler here. sync ops (dialect/query_sql etc.) keep their
// try/catch; for a non-Promise we pass through unchanged to preserve sync semantics.
// `data` still arrives as a JSON string; normDbErr parses it in place so a handler can
// read e.data.capability directly.
function dbOpWrap(p) {
  if (p && typeof p.then === "function") {
    return p.then(
      (r) => r,
      (e) => { throw normDbErr(e); },
    );
  }
  return p;
}

// REQ-05 (U-18 batch Q3): per-call opts for db.query/db.exec. Only `timeoutMs` exists and
// it can only TIGHTEN the configured db_access.<alias>.statement_timeout_ms -- a larger
// value is rejected with a structured OJ_DB_CAPACITY naming the config key.
function callOpts(opts) {
  if (!opts || opts.timeoutMs === undefined || opts.timeoutMs === null) return null;
  return { timeoutMs: opts.timeoutMs };
}

// v0.1.95: one awaitable surface for db.session. Some of its ops are sync (attach/detach/
// list/info/stats/touch/closeAll); wrapping them keeps "every session call returns a Promise"
// true, so a failure always arrives as a rejection at the caller's `await` instead of as a
// synchronous throw that an outer try/catch around a promise chain would miss.
function dbSessionCall(fn) {
  return async (...args) => {
    try {
      return await fn(...args);
    } catch (e) {
      throw normDbErr(e);
    }
  };
}

globalThis.DB = function (name) {
  name = String(name);
  // U-3 v0.1.93: re-check epoch on EVERY call (host-authoritative generation counter).
  // cache-hit now revalidates instead of short-circuiting, so a deleted/reRegistered
  // client db is invalidated in JS promptly. Correctness never depends on the JS-held
  // epoch: query ops re-resolve the accessor by name against the host registry.
  const epoch = op_db_epoch(name);
  if (dbCache.has(name) && dbEpoch.get(name) === epoch) {
    return dbCache.get(name);
  }
  if (!op_db_has(name)) {
    dbCache.delete(name);
    dbEpoch.delete(name);
    return undefined;
  }
  dbEpoch.set(name, epoch);
  dbCache.set(name, {
      ...condFactories(),
      // raw SQL + bound params (params optional).
      query: (sql, params, opts) => dbOpWrap(op_db_query(name, String(sql), params === undefined ? null : encodeParams(params), callOpts(opts))),
      exec: (sql, params, opts) => dbOpWrap(op_db_exec(name, String(sql), params === undefined ? null : encodeParams(params), callOpts(opts))),
      // U-18 task-based export (v0.1.104): returns {taskId} immediately; a Rust
      // consumer task drains the cursor into a blob artifact (csv/jsonl).
      export: (args) => dbOpWrap(op_db_export(name, args)),
      exportStatus: (taskId) => dbOpWrap(op_db_export_status(String(taskId))),
      exportCancel: (taskId) => dbOpWrap(op_db_export_cancel(String(taskId))),
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
      nextSeq: (n) => dbOpWrap(op_db_next_seq(name, String(n))),
      dialect: () => { try { return op_db_dialect(name); } catch (e) { throw normDbErr(e); } },
      // Dialect-agnostic catalog introspection (v0.1.82): names of all business-visible
      // tables in the current database, INCLUDING tables unknown to the schema registry
      // (api-only plans / legacy tables). Optional prefix filter (escaped, bound param).
      listTables: (prefix) => dbOpWrap(op_db_list_tables(name, prefix === undefined || prefix === null ? null : String(prefix), null)),
      // REQ-04 (v0.1.105): schema/database listing (REQ-04 section 5.1). Cross-dialect
      // homogeneous {name, owner?, isDefault?, comment?}; system catalogs filtered by
      // the platform (mysql: information_schema/mysql/performance_schema/sys; pg schema:
      // pg_catalog/pg_toast%/information_schema; pg database: templates). Account-level
      // visibility follows the DB account's own privilege model.
      listSchemas: (prefix) => dbOpWrap(op_db_list_schemas(name, prefix === undefined || prefix === null ? null : String(prefix))),
      listDatabases: (prefix) => dbOpWrap(op_db_list_databases(name, prefix === undefined || prefix === null ? null : String(prefix))),
      // REQ-04 (v0.1.105) section 5.2 option B: restricted read-only introspection
      // view pinned to one schema/database (mysql: schema==database; pg: namespace;
      // sqlite: OJ_DB_UNSUPPORTED capability=for_schema). No query/exec/stream on
      // the handle; pool-scoped only (no tx/session variant). Malformed scope names
      // return empty results, never errors, never leak.
      forSchema: (schema) => {
        const s = String(schema);
        return {
          listTables: (prefix) => dbOpWrap(op_db_list_tables(name, prefix === undefined || prefix === null ? null : String(prefix), s)),
          listColumns: (table) => dbOpWrap(op_db_list_columns(name, String(table), s)),
          listIndexes: (table) => dbOpWrap(op_db_list_indexes(name, String(table), s)),
          listForeignKeys: (table) => dbOpWrap(op_db_list_foreign_keys(name, String(table), s)),
          catalog: (kind, arg) => dbOpWrap(op_db_catalog(name, String(kind), arg === undefined ? null : String(arg), s)),
        };
      },
      listColumns: (table) => dbOpWrap(op_db_list_columns(name, String(table), null)),
      listIndexes: (table) => dbOpWrap(op_db_list_indexes(name, String(table), null)),
      listForeignKeys: (table) => dbOpWrap(op_db_list_foreign_keys(name, String(table), null)),
      // U-14 batch a (v0.1.100): catalog introspection -- new-generation envelope
      // {kind, dialect, items}. kinds: views | routines | sequences | ddl | comment |
      // constraints | stats (per-dialect support matrix in db-guide section 10; unsupported
      // kinds reject with OJ_DB_UNSUPPORTED, never silent). ddl/comment/constraints
      // take a table name arg; views/routines/sequences/stats are whole-db.
      catalog: (kind, arg) => dbOpWrap(op_db_catalog(name, String(kind), arg === undefined ? null : String(arg), null)),
      // SQL execution profile (v0.1.51, on by default in dev): per-request SQL stats + slow queries + events.
      // Returns { count, totalMs, slow, byDb, events }; empty object when tracing is off.
      sqlProfile: () => op_db_sql_profile(),
      // db access control introspection (v0.1.89): aliases this caller may reach, with
      // the capability bits that apply. Without it a UI can only list every connection
      // and fail on click -- a fake entry. Sync op; executes no SQL.
      allowed: () => op_db_allowed(),
      // PR-2 Phase A streaming query. Primary form: callback (onRow enriches/transforms each
      // row, e.g. writing to json.stream). Otherwise returns an async iterable (escape hatch via
      // for-await). signal is wired through the abort event to op_db_stream_abort.
      // op_db_stream_open is an async op (returns a Promise<{id, executionId}>); it is resolved
      // lazily on the first next()/onRow so this arrow need not be async (for-await needs a sync
      // async-iterable). executionId (U-5) is the cross-request handle for db.cancel(); it
      // surfaces via opts.onOpen(executionId) or it.executionId().
      stream: (sql, params, opts) => {
        let idsP = null;
        // REQ-05: final stats snapshot -- read BEFORE close() deregisters the stream, so
        // it.info() keeps working after natural completion (truncate signal included).
        let lastInfo = null;
        const readInfo = () =>
          openIds()
            .then((v) => dbOpWrap(op_db_stream_info(v.id)))
            .then((i) => {
              lastInfo = i;
              return i;
            })
            .catch(() => null);
        const openIds = () => (idsP ||= dbOpWrap(op_db_stream_open(name, String(sql), params === undefined ? null : encodeParams(params), callOpts(opts))));
        const swallow = (p) => { p.then(() => {}).catch(() => {}); };
        const wireAbort = () => swallow(openIds().then((v) => op_db_stream_abort(v.id)));
        const signal = opts && opts.signal;
        if (signal) signal.addEventListener("abort", wireAbort);
        const close = () => {
          if (signal) signal.removeEventListener("abort", wireAbort);
          // open may itself have failed (guard/unsupported): a second rejection here
          // would surface as an unhandled rejection on top of the consumer's error.
          swallow(openIds().then((v) => op_db_stream_close(v.id)));
        };
        const emitOpen = (v) => { if (opts && typeof opts.onOpen === "function") opts.onOpen(v.executionId); };
        if (opts && typeof opts.onRow === "function") {
          return (async () => {
            try {
              const v = await openIds();
              emitOpen(v);
              for (;;) {
                const row = await dbOpWrap(op_db_stream_next(v.id));
                if (row === null) break;
                opts.onRow(row);
              }
            } finally {
              close();
            }
          })();
        }
        return {
          // U-5 v0.1.96: the cross-request cancellation handle (triggers open as a side effect).
          executionId: () => openIds().then((v) => v.executionId),
          // REQ-05 (U-18 batch Q6): stream runtime info -- {rows, bytes, truncated}.
          // `truncated` is the explicit max_result_bytes signal (truncate mode); fail mode
          // surfaces as a structured error row instead (OJ_DB_CAPACITY via next()).
          info: () => (lastInfo ? Promise.resolve(lastInfo) : readInfo()),
          [Symbol.asyncIterator]() {
            return {
              next: async () => {
                try {
                  const v = await openIds();
                  emitOpen(v);
                  const row = await dbOpWrap(op_db_stream_next(v.id));
                  if (row === null) {
                    await readInfo(); // snapshot final stats before close() deregisters
                    close();
                    return { done: true, value: undefined };
                  }
                  return { done: false, value: row };
                } catch (e) {
                  await readInfo(); // snapshot final stats before close() deregisters
                  close();
                  throw e;
                }
              },
            };
          },
        };
      },
      // U-5 v0.1.96: cancel a streaming execution opened by ANY request of the same subject
      // (id comes from onOpen/executionId()). Idempotent while the execution is registered;
      // unknown/finished/foreign ids reject with the same "not found" message.
      cancel: dbSessionCall((id) => op_db_cancel(String(id))),
      // transaction: db.tx(async (tx) => { await tx.exec(...); ... })
      // commit on resolve, rollback on throw/reject; tx rides the same connection
      // (query/exec/table route to the active tx). Nested tx is rejected by the op.
      tx: async (fn) => {
        await dbOpWrap(op_db_tx_begin(name));
        try {
          const out = await fn({
            ...condFactories(),
            query: (sql, params) => dbOpWrap(op_db_query(name, String(sql), params === undefined ? null : encodeParams(params), null)),
            exec: (sql, params) => dbOpWrap(op_db_exec(name, String(sql), params === undefined ? null : encodeParams(params), null)),
            table: (t) => queryBuilder(name, String(t)),
            // Same-connection allocator: MySQL's LAST_INSERT_ID is session-scoped.
            nextSeq: (n) => dbOpWrap(op_db_next_seq(name, String(n))),
            // Engine name for dialect-aware SQL in shared libs; isTx marks the tx-scoped
            // handle (DDL must never run inside db.tx -- MySQL implicit commit).
            dialect: () => { try { return op_db_dialect(name); } catch (e) { throw normDbErr(e); } },
            // Catalog introspection rides the active tx session (same connection).
            listTables: (prefix) => dbOpWrap(op_db_list_tables(name, prefix === undefined || prefix === null ? null : String(prefix), null)),
      listColumns: (table) => dbOpWrap(op_db_list_columns(name, String(table), null)),
      listIndexes: (table) => dbOpWrap(op_db_list_indexes(name, String(table), null)),
      listForeignKeys: (table) => dbOpWrap(op_db_list_foreign_keys(name, String(table), null)),
            isTx: true,
            // SQL execution profile (v0.1.51, on by default in dev): same per-request profile as db.sqlProfile().
            sqlProfile: () => op_db_sql_profile(),
            fromJSON: (snap) => builderFromReq(encodeParams(snap)),
            asSystem: () => { op_db_as_system(); return dbCache.get(name); },
            asTenant: (id) => { op_db_as_tenant(String(id)); return dbCache.get(name); },
          });
          await dbOpWrap(op_db_tx_commit(name));
          return out;
        } catch (e) {
          // Rollback is best-effort: its own failure must never mask the business error.
          try {
            await op_db_tx_rollback(name);
          } catch (_) {}
          throw e;
        }
      },
      // U-4 v0.1.95: cross-request transactions (db.session). A session is a host-owned
      // transaction identified by an opaque id; it OUTLIVES the request that opened it, so
      // a multi-step wizard can commit across several HTTP calls. Two ways to use it:
      //   (a) attach(id) -> this request's db.query/exec/table all ride that connection;
      //   (b) explicit session.query(id, sql, params) / session.exec(...) per statement.
      // begin() requires db_session.enabled + an authorized caller. Sessions expire on
      // idle/max-lifetime and are rolled back by the host; commit/rollback/close end them.
      // Ids are bearer tokens: never log or return them to end users verbatim.
      session: {
        begin: dbSessionCall((opts) => op_db_session_begin(name, opts === undefined ? null : opts)),
        attach: dbSessionCall((id) => op_db_session_attach(String(id))),
        detach: dbSessionCall(() => op_db_session_detach()),
        query: dbSessionCall((id, sql, params) => op_db_session_query(String(id), String(sql), params === undefined ? null : encodeParams(params))),
        exec: dbSessionCall((id, sql, params) => op_db_session_exec(String(id), String(sql), params === undefined ? null : encodeParams(params))),
        commit: dbSessionCall((id) => op_db_session_commit(String(id))),
        rollback: dbSessionCall((id) => op_db_session_rollback(String(id))),
        // idempotent cleanup: best-effort rollback, never throws on an already-dead session.
        close: dbSessionCall((id) => op_db_session_close(String(id))),
        // push the idle timer without extending max lifetime; returns { expiresInMs }.
        touch: dbSessionCall((id) => op_db_session_touch(String(id))),
        closeAll: dbSessionCall(() => op_db_session_close_all()),
        list: dbSessionCall(() => op_db_session_list()),
        info: dbSessionCall((id) => op_db_session_info(String(id))),
        stats: dbSessionCall(() => op_db_session_stats()),
      },
    });
  return dbCache.get(name);
};
globalThis.db = globalThis.DB("default");

// L3 dynamic data-source registration (client axis, v0.1.91): register a DSN
// as a named data source at runtime. The alias then enters the DB(name) resolution
// path (reusing L1 access control + L2 introspection + all query/exec/stream), so
// only the "admin" surface is exposed here; consumption always goes through DB(name).
// NOTE: delete must evict dbCache (DB() caches instances by name), otherwise after a
// delete, DB(name) would still return the stale instance.
// Guarded: `db` is undefined when no default db is configured (some test/probe
// contexts); the client ops are still reachable via the registered op names directly.
if (globalThis.db) {
  // U-18 T2.5 (REQ-06): these surfaces are typed `Promise<...>` in global.d.ts, so every
// async method must return a REAL Promise (instanceof Promise, .catch/.finally chainable) --
// sync op values are wrapped via dbSessionCall, keeping normDbErr rejection semantics.
globalThis.db.client = {
  register: dbSessionCall((name, dsn) => op_db_client_register(String(name), String(dsn))),
  test: dbSessionCall((dsn) => op_db_client_test(String(dsn))),
  delete: dbSessionCall(async (name) => {
    const ok = await op_db_client_delete(String(name));
    dbCache.delete(String(name));
    dbEpoch.delete(String(name));
    return ok;
  }),
  reRegister: dbSessionCall((name, dsn) => op_db_client_re_register(String(name), String(dsn))),
  list: dbSessionCall(() => op_db_client_list()),
  has: (name) => op_db_client_has(String(name)),
  };
  // U-1 v0.1.93 runtime dynamic authorization (access-control domain; same privilege
  // gate and fail-closed as db.client). grant(alias, allow, opts?) injects the "allow"
  // dimension (roles/subjects four-dim union) into the runtime table so authorized
  // callers can read the db without a restart (default readOnly=true); revoke(alias)
  // falls back to config (not deleted). allow must restrict at least one dimension;
  // an empty allow (grant-to-everyone) is rejected by the host.
  globalThis.db.access = {
    grant: dbSessionCall(
      (name, allow, opts) => op_db_access_grant(String(name), allow, opts === undefined ? null : opts),
    ),
    revoke: dbSessionCall((name) => op_db_access_revoke(String(name))),
  };
  // U-3 v0.1.93: host-authoritative generation counter for a db alias. Lets a handler
  // detect that a client db was deleted/reRegistered across isolates (compare against a
  // previously seen value). Read-only; no privilege.
  globalThis.db.epoch = (name) => op_db_epoch(String(name));
}
// ----- U-9 v0.1.98: security audit query (unified exit for DB_CLIENT_*/DB_ACCESS_AUDIT/DB_SESSION_*)
// In-process bounded ring buffer; retention by count+age. Gated by audit.allow (re-checked per
// call). filter: {kind?, user?, tenant?, alias?, sinceMs?, untilMs?, limit?}; newest first.
// This doubles as the export channel (handlers serve the JSON as they see fit).
globalThis.audit = {
  query: async (filter) => {
    try {
      return await op_audit_query(filter === undefined ? null : filter);
    } catch (e) {
      throw normDbErr(e);
    }
  },
};
// ----- U-15: management connector surface (privileged; NOT reachable via axis() --
// mgmt.axes claimed at assembly are removed from the public generic-axis registry).
// register(name, dsn) probes __ops__ + __connect__ + ping (fail-loud); call dispatches
// through the read/write allow tiers (readOnly is connector-self-reported) with a
// per-connector concurrency gate; every action lands in the audit log (zero credentials).
globalThis.mgmt = {
  register: (name, dsn) => dbOpWrap(op_mgmt_register(String(name), String(dsn))),
  delete: (name) => dbOpWrap(op_mgmt_delete(String(name))),
  list: () => dbOpWrap(op_mgmt_list()),
  has: (name) => op_mgmt_has(String(name)),
  call: (name, op, args) =>
    dbOpWrap(op_mgmt_call(String(name), String(op), args === undefined ? "{}" : JSON.stringify(args))),
};
// ----- U-10 v0.1.101: runtime credential face (same X25519+AES-GCM envelope as config
// ENC[...]). seal encrypts with the deployment public key (secrets.public_key_path, or
// derived from the private key); open decrypts with the deployment private key and lands
// a zero-credential SECRET_OPEN audit event per call. Both fail closed when the assembly
// has no keypair (error names how to configure). The private key never crosses into JS.
globalThis.secret = {
  seal: (plaintext) => op_secret_seal(String(plaintext)),
  open: (token) => op_secret_open(String(token)),
};

// ----- safe query builder (fluent, structured) -----
// usage: db.table("user").select(["id","name"]).where({field:"age",op:"gte",value:18})
//          .orderBy([{field:"id",dir:"desc"}]).limit(10).all()
// builderFromReq(snap) rebuilds a builder from a plain req snapshot (see db.fromJSON);
// snapshots are opaque req objects: identity/db binding is re-pointed at the restoring
// module's bound db (snapshot `db` is only the JS-visible name it was created with).
function builderFromReq(snap) {
  const req = Object.assign(
    { db: "default", table: "", columns: [], conditions: [], order_by: [], limit: null, offset: null, verb: "select", values: [], sets: {}, joins: [], group_by: [], having: null, distinct: false, unions: [], with: [], returning: [], or_ignore: false, from_select: null, insert_cols: [] },
    snap,
  );
  req.db = String(req.db);
  // table may be an {name, as?} object (aliased, v0.1.81) -- do not String() it.
  if (typeof req.table === "string") req.table = String(req.table);
  const api = {
    select(cols) {
      req.columns = (cols || []).map((c) => {
        if (typeof c === "string") return String(c);
        // constant column (v0.1.81): `select(["name", { lit: "svc", as: "scope" }])`.
        // The value is a bound param like any other -- never spliced into the SQL text.
        if (c && c.lit !== undefined) return encodeParams({ lit: c.lit, as: String(c.as) });
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
    // Dynamic filter (v0.1.81): skip when the guard value is absent.
    // 0 and "" are KEPT (status=0 is a legitimate filter value); empty arrays are
    // skipped because `IN ()` is a permanently-false predicate in the engine and
    // would silently return 0 rows, indistinguishable from "no match".
    whereIf(test, cond) {
      if (test === undefined || test === null || test === false) return api;
      if (Array.isArray(test) && test.length === 0) return api;
      req.conditions.push(encodeParams(unwrapTree(cond)));
      return api;
    },
    orderBy(items) { req.order_by = (items || []).map((i) => ({ field: String(i.field), dir: i.dir ? String(i.dir) : null })); return api; },
    limit(n) { req.limit = n | 0; return api; },
    offset(n) { req.offset = n | 0; return api; },
    all() { return dbOpWrap(op_db_query_build(req)); },
    insert(rows) { req.verb = "insert"; req.values = (Array.isArray(rows) ? rows : [rows]).map((r) => encodeParams({ ...r })); return api; },
    // Insert returning clause (whitelisted columns, e.g. ["id"]): run() then resolves
    // to a row array [{id: n}] instead of the affected-row count. pg/sqlite render a
    // single sea-query RETURNING statement; mysql has no RETURNING and takes
    // LAST_INSERT_ID() in a second step on the same connection (use db.tx for safety).
    returning(cols) { req.returning = (cols || []).map(String); return api; },
    // Insert-from-select (v0.1.81): `db.table("t").fromSelect(db.table("s").select(["id","name"]), ["id","name"])`.
    // The two column lists must line up by position -- the engine rejects any arity
    // mismatch instead of silently writing the source rows into the wrong columns.
    fromSelect(sub, cols) {
      req.verb = "insert";
      req.from_select = unwrapSub(sub);
      req.insert_cols = (cols || []).map(String);
      return api;
    },
    // Skip rows that collide with a unique key / primary key (v0.1.81).
    // sqlite/pg render ON CONFLICT DO NOTHING; mysql has no DO NOTHING and falls back
    // to the identity update `ON DUPLICATE KEY UPDATE <pk> = <pk>` (needs a primary key).
    orIgnore() { req.or_ignore = true; return api; },
    update(sets) { req.verb = "update"; req.sets = encodeParams({ ...sets }); return api; },
    delete() { req.verb = "delete"; return api; },
    // truncate(): explicit clear-all (v0.1.82). Named destructive verb instead of a
    // forgotten where; renders DELETE FROM t. Tenant guard still applies -- on a
    // tenant-scoped table it clears the CURRENT tenant's rows only (db.asSystem for all).
    // A user-supplied where is rejected by the engine ("truncate does not accept where").
    truncate() { req.verb = "delete"; req.truncate = true; return api; },
    // Base-table alias (v0.1.81): `db.table("a").as("x")`. Names the base table for
    // column qualification; ownership/tenant guards still key off the REAL name.
    as(alias) {
      const base = typeof req.table === "string" ? { name: req.table } : req.table;
      req.table = { name: String(base.name), as: String(alias) };
      return api;
    },
    // join(table, on, kind, alias?): `on` entries are either {left,right} (col=col) or
    // {left,op,value} (col <op> value, v0.1.81). `value` goes through encodeParams so
    // toBigInt() markers survive. The optional 4th arg aliases the JOINED table.
    join(table, on, kind, alias) {
      const jt = typeof table === "string" ? { name: table } : table;
      const t = alias === undefined || alias === null
        ? { name: String(jt.name) }
        : { name: String(jt.name), as: String(alias) };
      req.joins.push({
        table: t,
        on: (on || []).map((p) => {
          // Keep BOTH shapes faithfully: serde_v8 drops `undefined` values, so a
          // half-written {left,right,op} must still reach Rust to be rejected there
          // (the engine is the authoritative entry point, not this chain layer).
          const e = { left: String(p.left) };
          const has = (x) => x !== undefined && x !== null;
          if (has(p.right)) e.right = String(p.right);
          if (has(p.op)) {
            e.op = String(p.op);
            e.value = has(p.value) ? encodeParams(p.value) : null;
          }
          return e;
        }),
        kind: kind ? String(kind) : "inner",
      });
      return api;
    },
    distinct() { req.distinct = true; return api; },
    groupBy(cols) { req.group_by = (cols || []).map(String); return api; },
    having(cond) { req.having = encodeParams(unwrapTree(cond)); return api; },
    union(other, kind) { req.unions.push({ kind: kind ? String(kind) : "distinct", query: unwrapSub(other) }); return api; },
    with(name, columns, query) { req.with.push({ name: String(name), columns: (columns || []).map(String), query: unwrapSub(query) }); return api; },
    run() {
      if ((req.verb === "update" || req.verb === "delete") && req.conditions.length === 0 && !req.truncate) {
        throw new Error(req.verb + " requires where");
      }
      if (req.verb === "insert" && req.values.length === 0 && !req.from_select) {
        throw new Error("insert needs either values or fromSelect");
      }
      return dbOpWrap(op_db_query_build(req));
    },
    toSQL() { try { return op_db_query_sql(req); } catch (e) { throw normDbErr(e); } },
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

// ----- module driver (perf F2) -----
// Resident dispatch runner: execute the framework-built driver body via a per-request
// AsyncFunction compilation -- await-capable like the old side-module driver body
// (TLA-style awaits), and strict via the "use strict" prologue to match module
// semantics, without the per-request module load (compile + module-map growth).
// Errors are marshaled through op_driver_error; Rust maps them to the same
// RunError::Core path as before. Reset/capture semantics unchanged.
const OjAsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
globalThis.__ojRun = async () => {
  try {
    await new OjAsyncFunction('"use strict";\n' + globalThis.__ojBody)();
  } catch (e) {
    // Include the error class name (e.g. "NotCapable: ..."): the old side-module
    // path surfaced "Name: message" via mod_evaluate; e.message alone drops it
    // (handlers and tests match on the class name).
    const n = e && e.name !== undefined ? String(e.name) : "";
    const m = e && e.message !== undefined ? String(e.message) : String(e);
    op_driver_error(n && m !== n ? n + ": " + m : m);
  }
};

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
  // string -> op_sha256_hex; Uint8Array/ArrayBuffer -> op_sha256_bytes (binary
  // digest, e.g. non-utf8 payloads). JsBuffer only accepts views, so unwrap
  // ArrayBuffer first.
  sha256Hex: (data) => {
    if (typeof data === "string") return op_sha256_hex(data);
    if (data instanceof ArrayBuffer) data = new Uint8Array(data);
    return op_sha256_bytes(data);
  },
  // md5: BROKEN cryptographically - non-security uses only (cache keys, etags,
  // legacy compat). Never for credentials/signatures/integrity checks.
  // Same dispatch shape as sha256Hex.
  md5Hex: (data) => {
    if (typeof data === "string") return op_md5_hex(data);
    if (data instanceof ArrayBuffer) data = new Uint8Array(data);
    return op_md5_bytes(data);
  },
  randomHex: (n) => op_random_hex(n === undefined ? null : n | 0),
  // WebCrypto shape: fill any TypedArray view (wasm-bindgen glue depends on it).
  // ponytail: op caps at 65536, no re-check here; non-view throws TypeError.
  getRandomValues: (view) => {
    if (!ArrayBuffer.isView(view)) {
      throw new TypeError("crypto.getRandomValues: argument must be a TypedArray view");
    }
    const bytes = op_crypto_random(view.byteLength);
    new Uint8Array(view.buffer, view.byteOffset, view.byteLength).set(bytes);
    return view;
  },
  // AES-GCM application-level encryption: key from vars.get(...) (may be sealed
  // via ENC[...] in config). plaintext utf8 in, base64(nonce12 || ct || tag16) out.
  aesGcmEncrypt: (plaintext, key) => op_aes_gcm_encrypt(String(plaintext), String(key)),
  aesGcmDecrypt: (ciphertext, key) => op_aes_gcm_decrypt(String(ciphertext), String(key)),
  // Ed25519 verification (bundle signature check). publicKey: hex/base64 raw
  // 32-byte key (no PEM/SPKI); sig: base64 only (no base64url). Format errors
  // throw; signature mismatch returns false.
  verifyEd25519: (publicKey, msg, sigBase64) =>
    op_ed25519_verify(String(publicKey), String(msg), String(sigBase64)),
});

// ----- base64: atob/btoa globals (wasm-bindgen glue; same algorithm as the oidc inline helper) -----
(function () {
  const B = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const WS = /[ \t\n\r\f]/;
  const INVALID = "InvalidCharacterError: The string to be decoded is not correctly encoded.";
  globalThis.atob = (input) => {
    let clean = "";
    for (const c of String(input)) {
      if (WS.test(c)) continue;
      if (c === "=" && clean.length >= 2) break; // tolerate trailing padding
      clean += c;
    }
    if (clean.length % 4 === 1) throw new Error(INVALID);
    let out = "";
    let bits = 0;
    let acc = 0;
    for (const c of clean) {
      const v = B.indexOf(c);
      if (v < 0) throw new Error(INVALID);
      acc = (acc << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += String.fromCharCode((acc >> bits) & 0xff);
      }
    }
    return out;
  };
  globalThis.btoa = (input) => {
    const s = String(input);
    let out = "";
    for (let i = 0; i < s.length; i += 3) {
      const c0 = s.charCodeAt(i);
      const c1 = i + 1 < s.length ? s.charCodeAt(i + 1) : null;
      const c2 = i + 2 < s.length ? s.charCodeAt(i + 2) : null;
      if (c0 > 255 || (c1 !== null && c1 > 255) || (c2 !== null && c2 > 255)) {
        throw new Error(
          "InvalidCharacterError: The string to be encoded contains characters outside of the Latin1 range."
        );
      }
      const n = (c0 << 16) | ((c1 || 0) << 8) | (c2 || 0);
      out += B[(n >> 18) & 63] + B[(n >> 12) & 63] +
        (c1 !== null ? B[(n >> 6) & 63] : "=") +
        (c2 !== null ? B[n & 63] : "=");
    }
    return out;
  };
})();
