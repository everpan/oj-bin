// ext:oj_exec_ext/exec_bootstrap.js - exec SDK (auto-run as oj_exec_ext esm entry).
// Overrides: globalThis.log -> op_exec_log (terminal direct-out, not tracing);
// Defines: globalThis.console (deno_core default has none) + globalThis.args.
// NOTE: embedded via deno_core::ascii_str_include! -- keep this file ASCII-only.

import { op_exec_log, op_exec_log_err, op_exec_args } from "ext:core/ops";

// Script argv after `--`, injected via ExecOptions (state closure) -- NOT a static literal.
globalThis.args = op_exec_args();

// BigInt-safe stringify (own copy: bridge bootstrap's ojStringify is module-local there).
function stringify(v) {
  return JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? x.toString() : x));
}

// console.* : free-form multi-arg -> "a 1 {"x":2}" (strings bare, rest JSON).
// Channel split (v0.1.55): log/info (level 1) print raw to stdout (pipe-friendly,
// no prefix); debug/warn/error go to stderr with a level label (diagnostics).
function emit(level, xs) {
  op_exec_log(
    level,
    xs.map((x) => (typeof x === "string" ? x : stringify(x))).join(" ")
  );
}
globalThis.console = {
  debug: (...xs) => emit(0, xs),
  log: (...xs) => emit(1, xs),
  info: (...xs) => emit(1, xs),
  warn: (...xs) => emit(2, xs),
  error: (...xs) => emit(3, xs),
};

// log.* : zap-style msg + alternating kv pairs; fields appended as JSON when non-empty.
// Always stderr with a level label via op_exec_log_err (logs are diagnostics,
// not pipe results -- never routed to stdout, even at info level).
function logCall(level, msg, kv) {
  const fields = {};
  for (let i = 0; i + 1 < kv.length; i += 2) fields[String(kv[i])] = kv[i + 1];
  const s = stringify(fields);
  op_exec_log_err(level, s === "{}" ? String(msg) : String(msg) + " " + s);
}
globalThis.log = {
  debug: (msg, ...kv) => logCall(0, msg, kv),
  info: (msg, ...kv) => logCall(1, msg, kv),
  warn: (msg, ...kv) => logCall(2, msg, kv),
  error: (msg, ...kv) => logCall(3, msg, kv),
};
