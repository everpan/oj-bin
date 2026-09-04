// ext:oj_test_ext/test_bootstrap.js - L1 test SDK (auto-run as oj_test_ext esm entry).
// Injects: globalThis.client (HTTP dispatch helper) + a tiny describe/it/expect/beforeEach framework.
// Test files (*.test.ts) call client.get/post/... which trigger op_client_dispatch -> App oneshot.

import { op_client_dispatch } from "ext:core/ops";

const METHODS = ["get", "post", "put", "del", "patch", "head", "options"];

function buildClient() {
  const c = {};
  for (const m of METHODS) {
    // opts: { headers?: Record<string,string>, body?: string }
    // SDK name "del" maps to the wire method "DELETE" (routes::method_name
    // only maps real HTTP verbs; uppercasing "del" yields the unmapped "DEL").
    const wire = m === "del" ? "DELETE" : m.toUpperCase();
    c[m] = (path, opts = {}) =>
      op_client_dispatch(wire, path, opts.headers ?? {}, opts.body ?? "");
  }
  // login helper: POST /auth/login -> returns data.access_token.
  // usage: const token = await client.login("demo", "demo1234", {"X-TENANT-ID": "default"});
  c.login = async (username, password, headers) => {
    const r = await c.post("/auth/login", {
      headers: headers || {},
      body: JSON.stringify({ username, password }),
    });
    if (r.status !== 200) throw new Error("login failed: " + r.status);
    const data = JSON.parse(r.body).data;
    return data.access_token;
  };
  return c;
}

globalThis.client = buildClient();

// ----- Tiny test framework (vitest-style subset) -----
// __tests accumulates every it(); test files run describe/it at top level (side esm import).

globalThis.__tests = [];
globalThis.__suite = null;
globalThis.__beforeEach = null;

globalThis.describe = (name, fn) => {
  globalThis.__suite = name;
  try {
    fn();
  } finally {
    globalThis.__suite = null;
  }
};

globalThis.it = (name, fn) => {
  globalThis.__tests.push({ suite: globalThis.__suite, name, fn });
};

globalThis.beforeEach = (fn) => {
  globalThis.__beforeEach = fn;
};

function fmt(v) {
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}

globalThis.expect = (actual) => ({
  toBe: (e) => {
    if (actual !== e) throw new Error(`expected ${fmt(e)} but got ${fmt(actual)}`);
  },
  toEqual: (e) => {
    if (fmt(actual) !== fmt(e)) throw new Error(`expected ${fmt(e)} but got ${fmt(actual)}`);
  },
  toBeTruthy: () => {
    if (!actual) throw new Error(`expected truthy but got ${fmt(actual)}`);
  },
  toBeFalsy: () => {
    if (actual) throw new Error(`expected falsy but got ${fmt(actual)}`);
  },
  toContain: (sub) => {
    if (!String(actual).includes(String(sub))) {
      throw new Error(`expected ${fmt(actual)} to contain ${fmt(sub)}`);
    }
  },
});

// Run all registered tests; write result to globalThis.__testSummary for Rust to read.
globalThis.__runTests = async () => {
  const results = [];
  for (const t of globalThis.__tests) {
    try {
      if (globalThis.__beforeEach) await globalThis.__beforeEach();
      await t.fn();
      results.push({ suite: t.suite, name: t.name, ok: true, error: null });
    } catch (e) {
      results.push({
        suite: t.suite,
        name: t.name,
        ok: false,
        error: String((e && e.stack) || e),
      });
    }
  }
  const passed = results.filter((r) => r.ok).length;
  const summary = {
    total: results.length,
    passed,
    failed: results.length - passed,
    tests: results,
  };
  globalThis.__testSummary = summary;
  // Rust reads this JSON string via contextless scope, avoiding serde_v8 context-bound pitfall.
  globalThis.__testSummaryJson = JSON.stringify(summary);
  return summary;
};
