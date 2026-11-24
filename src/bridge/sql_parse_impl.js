// ext:bridge_ext/sql_parse_impl.js -- sql.parse implementation (v0.1.143, REQ-PARSE).
// Frozen contract: docs/superpowers/plans/2026-11-24-upstream-reqs-phase2-sql-parse.md
// (tables/columns/ctes/lineage/ast; lineage MVP = single-level CTE pass-through).
// Pure synchronous function over the vendored node-sql-parser AST: no ops, no
// I/O, no tenant data. Everything here runs inside the caller's runtime only.
import nodeSqlParser from "ext:bridge_ext/vendor_sql_parser.js";

// Frozen dialect table (grep-verified against the 5.4.0 bundle; duckdb is NOT
// in 5.4.0 -- add it here first if a future vendor bump brings it).
const DIALECTS = [
  "mysql", "mariadb", "postgresql", "sqlite", "snowflake", "transactsql",
  "trino", "bigquery", "hive", "flinksql", "db2", "redshift", "athena", "noql",
];
const DIALECT_ALIASES = { pg: "postgresql", mssql: "transactsql" };

// Budget: input length cap (chars), mirroring yaml.parse's bounded-parse style.
// The AST JSON can reach ~10x the input size -- that is the caller's cost, the
// budget keys on the INPUT only.
const MAX_INPUT_CHARS = 1048576;

/** Structured parse failure: Error with frozen own props (see plan contract). */
function fail(reason, message, extra) {
  const e = new Error(message);
  e.code = "OJ_SQL_PARSE";
  e.reason = reason;
  if (extra) {
    for (const k of Object.keys(extra)) e[k] = extra[k];
  }
  throw e;
}

// ---------------------------------------------------------------------------
// AST walk helpers (node-sql-parser 5.4.0 shapes; probe-verified in tests)
// ---------------------------------------------------------------------------

/** Select-item output name: explicit alias, else the column name, else null. */
function outputName(item) {
  const as = item && item.as;
  if (typeof as === "string" && as.length > 0) return as;
  if (as && typeof as.value === "string" && as.value.length > 0) return as.value;
  const expr = item && item.expr;
  if (expr && expr.type === "column_ref") return expr.column;
  return null;
}

/** FROM-entry shape per probe: { db, table, as } -- string table + db key. */
function isFromEntry(o) {
  return (
    o !== null && typeof o === "object" && !Array.isArray(o) &&
    typeof o.table === "string" && "db" in o
  );
}

/** Deep-walk an AST fragment, invoking visit(node) on every object node. */
function walk(v, visit) {
  if (Array.isArray(v)) {
    for (const x of v) walk(x, visit);
    return;
  }
  if (v === null || typeof v !== "object") return;
  visit(v);
  for (const k of Object.keys(v)) walk(v[k], visit);
}

/**
 * Pass 1 scope collection: base sources [{name, schema, alias}] (appearance
 * order, from-entries everywhere incl. nested subqueries) and CTE names
 * (top-level `with` only). CTE definitions map name -> stmt for lineage.
 */
function collectScope(ast) {
  const sources = [];
  const ctes = [];
  const cteDefs = new Map();
  const seen = new Set();
  const cteSet = new Set();
  // CTE names first (a table named like a CTE is the CTE within this query).
  walk(ast, (o) => {
    if (Array.isArray(o.with)) {
      for (const w of o.with) {
        if (w && w.name && typeof w.name.value === "string") {
          ctes.push(w.name.value);
          cteSet.add(w.name.value);
          if (w.stmt) cteDefs.set(w.name.value, w.stmt);
        }
      }
    }
  });
  const addSource = (o) => {
    if (!isFromEntry(o)) return;
    if (cteSet.has(o.table)) return;
    const key = (o.db || "") + "." + o.table;
    if (seen.has(key)) return;
    seen.add(key);
    sources.push({ name: o.table, schema: o.db || null, alias: o.as || null });
  };
  walk(ast, (o) => {
    if (isFromEntry(o)) addSource(o);
    // CREATE TABLE / INSERT / UPDATE targets share the {db, table} shape.
    if (o.type === "create" && Array.isArray(o.table)) {
      for (const t of o.table) addSource(t);
    }
  });
  return { sources, ctes, cteSet, cteDefs };
}

/**
 * Pass 2 column collection: every column_ref in the AST, qualifier resolved
 * through the alias map (alias -> base source name, or CTE name at the
 * boundary); bare columns resolve only under a single-source scope.
 */
function collectColumns(ast, scope) {
  const aliasMap = new Map();
  for (const s of scope.sources) if (s.alias) aliasMap.set(s.alias, s.name);
  const seen = new Set();
  const out = [];
  walk(ast, (o) => {
    if (o.type !== "column_ref" || typeof o.column !== "string") return;
    let table = null;
    if (typeof o.table === "string" && o.table.length > 0) {
      table = aliasMap.get(o.table) || (scope.cteSet.has(o.table) ? o.table : null);
    } else if (scope.sources.length === 1) {
      table = scope.sources[0].name;
    }
    const key = (table || "") + "." + o.column;
    if (seen.has(key)) return;
    seen.add(key);
    out.push({ table, column: o.column });
  });
  return out;
}

// ---------------------------------------------------------------------------
// Entry point (frozen contract; lineage filled in plan task 3)
// ---------------------------------------------------------------------------

export default {
  parse(sqlText, opts) {
    // input: strict string, non-blank (no silent coercion).
    if (typeof sqlText !== "string" || sqlText.trim().length === 0) {
      fail("input", "sql.parse: sqlText must be a non-empty string");
    }
    // dialect: normalized, no magic default, unknown -> structured rejection
    // carrying the supported list.
    const o = opts || {};
    if (o.dialect === undefined || o.dialect === null) {
      fail("dialect", "sql.parse: dialect is required", { supported: DIALECTS.slice() });
    }
    const raw = String(o.dialect);
    const dialect = DIALECT_ALIASES[raw] || raw;
    if (!DIALECTS.includes(dialect)) {
      fail(
        "dialect",
        "sql.parse: unsupported dialect " + JSON.stringify(raw) +
          " (supported: " + DIALECTS.join(", ") + ")",
        { dialect: raw, supported: DIALECTS.slice() },
      );
    }
    // budget: before parsing (a 1MiB+ input must not reach the parser).
    if (sqlText.length > MAX_INPUT_CHARS) {
      fail(
        "budget",
        "sql.parse: input exceeds " + MAX_INPUT_CHARS + " chars (" +
          sqlText.length + ")",
        { dialect, length: sqlText.length, max: MAX_INPUT_CHARS },
      );
    }
    let ast;
    try {
      ast = new nodeSqlParser.Parser().astify(sqlText, { dialect });
    } catch (e) {
      // Structured syntax error: parser message + location, never silent.
      const message = "sql.parse: " + (e && e.message ? e.message : "syntax error");
      const extra = { dialect };
      if (e && e.location && e.location.start) {
        extra.location = {
          start: {
            offset: e.location.start.offset,
            line: e.location.start.line,
            column: e.location.start.column,
          },
          end: e.location.end
            ? {
                offset: e.location.end.offset,
                line: e.location.end.line,
                column: e.location.end.column,
              }
            : null,
        };
      }
      fail("syntax", message, extra);
    }
    // Single-statement contract: astify hands back an ARRAY for multi-stmt.
    if (Array.isArray(ast)) {
      fail(
        "syntax",
        "sql.parse: multiple statements are not supported (one statement per call)",
        { dialect, statements: ast.length },
      );
    }
    const scope = collectScope(ast);
    const tables = scope.sources.map((s) => ({
      name: s.name,
      schema: s.schema,
      alias: s.alias,
    }));
    const columns = collectColumns(ast, scope);
    return {
      dialect,
      tables,
      columns,
      ctes: scope.ctes,
      lineage: [], // plan task 3: single-level CTE lineage walker
      ast,
    };
  },
};
