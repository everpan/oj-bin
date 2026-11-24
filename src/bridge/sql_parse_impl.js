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
  const ref = item && item.expr ? normColumnRef(item.expr) : null;
  return ref ? ref.column : null;
}

/**
 * Normalize a column-reference-shaped AST node to {table, column}.
 * Two shapes exist in 5.4.0 (probe-verified): plain `column_ref`
 * (string fields; object-valued `column`/`table` for some quoted forms),
 * and -- postgresql double-quoted identifiers -- a bare
 * `double_quote_string` node in expression position (double quotes ARE
 * identifiers in pg; single quotes parse as single_quote_string literals).
 */
function normColumnRef(o) {
  if (!o || typeof o !== "object") return null;
  if (o.type === "column_ref") {
    const column =
      typeof o.column === "string"
        ? o.column
        : o.column && typeof o.column.value === "string"
          ? o.column.value
          : null;
    if (!column) return null;
    const table =
      typeof o.table === "string"
        ? o.table
        : o.table && typeof o.table.value === "string"
          ? o.table.value
          : null;
    return { table, column };
  }
  if (o.type === "double_quote_string" && typeof o.value === "string") {
    return { table: null, column: o.value };
  }
  return null;
}

/**
 * FROM-entry shape per probe: { db, table, as } -- string table + db key, no
 * `column` field. The `column` exclusion keeps three-part column refs
 * ({type:"column_ref", db:"a", table:"b", column:"c"}) out: they share the
 * shape and would otherwise register phantom tables.
 */
function isFromEntry(o) {
  return (
    o !== null && typeof o === "object" && !Array.isArray(o) &&
    typeof o.table === "string" && "db" in o && o.column === undefined
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
 * order, from-entries everywhere incl. nested subqueries) and CTE names.
 * CTE names come from the STATEMENT-level `with` only (root, or the known
 * `{ast}` wrapper one level down): a WITH declared inside a subquery must not
 * leak into the global scope nor mask a same-named base table.
 */
function collectScope(ast) {
  const sources = [];
  const ctes = [];
  const cteDefs = new Map();
  const seen = new Set();
  const cteSet = new Set();
  // Statement-level WITH only (see doc above); nested WITH is out of scope.
  const withNode =
    (Array.isArray(ast.with) && ast.with) ||
    (ast.ast && Array.isArray(ast.ast.with) && ast.ast.with) ||
    null;
  if (withNode) {
    for (const w of withNode) {
      if (w && w.name && typeof w.name.value === "string") {
        ctes.push(w.name.value);
        cteSet.add(w.name.value);
        if (w.stmt) cteDefs.set(w.name.value, w.stmt);
      }
    }
  }
  const addSource = (o) => {
    if (!isFromEntry(o)) return;
    if (cteSet.has(o.table)) return;
    // Alias is part of identity: FROM emp a JOIN emp b keeps BOTH entries
    // (self-join aliases survive; bare columns then honestly degrade to null).
    const key = (o.db || "") + "." + o.table + "/" + (o.as || "");
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
    const ref = normColumnRef(o);
    if (!ref) return;
    let table = null;
    if (ref.table) {
      table = aliasMap.get(ref.table) || (scope.cteSet.has(ref.table) ? ref.table : null);
    } else if (scope.sources.length === 1) {
      table = scope.sources[0].name;
    }
    const key = (table || "") + "." + ref.column;
    if (seen.has(key)) return;
    seen.add(key);
    out.push({ table, column: ref.column });
  });
  return out;
}

// ---------------------------------------------------------------------------
// Lineage walker (single-level CTE / derived-subquery pass-through)
//
// Frozen rules (plan doc, "lineage resolution rules" section):
//   - output.table names the SOURCE BEHIND the alias (CTE name for
//     CTE-backed refs; null for anonymous derived subqueries), never the
//     alias itself.
//   - alias.col resolves through ONE level: into a CTE / derived subquery's
//     own select list; a CTE referencing another CTE stops at that boundary
//     name (no second-level recursion).
//   - bare col: single-source scope resolves to it; multi-source joins
//     degrade honestly (no schema knowledge -> table:null, sources []).
//   - DDL and non-select statements: lineage [] (documented contract).
// ---------------------------------------------------------------------------

/** One scope entry: a FROM item as seen by resolution. */
function scopeEntry(entry) {
  if (!entry) return null;
  if (entry.kind === "derived") {
    return { kind: "derived", name: null, alias: entry.alias, def: entry.def };
  }
  return { kind: entry.kind, name: entry.name, alias: entry.alias, def: entry.def };
}

/** Build a resolution scope from a select node's from/with shapes. */
function buildScope(selectAst, parentCtes) {
  const entries = [];
  const aliasMap = new Map();
  if (Array.isArray(selectAst.from)) {
    for (const f of selectAst.from) {
      if (isFromEntry(f)) {
        const kind = parentCtes.cteSet.has(f.table) ? "cte" : "base";
        const e = { kind, name: f.table, alias: f.as || null };
        entries.push(e);
        if (e.alias) aliasMap.set(e.alias, e);
      } else if (f && f.expr && f.expr.ast && f.as) {
        const e = { kind: "derived", name: null, alias: f.as, def: f.expr.ast };
        entries.push(e);
        aliasMap.set(f.as, e);
      }
    }
  }
  return { entries, aliasMap, cteDefs: parentCtes.cteDefs, cteSet: parentCtes.cteSet };
}

/** Resolve a qualifier (or bare column) against a scope -> source list. */
function resolveCol(qualifier, column, scope, depth) {
  if (qualifier) {
    const entry =
      scope.aliasMap.get(qualifier) ||
      scope.entries.find((e) => e.name === qualifier && !e.alias) ||
      null;
    if (!entry) return [];
    return resolveThrough(entry, column, scope, depth);
  }
  // bare column: single-source scope only (honest degradation otherwise).
  if (scope.entries.length === 1) {
    return resolveThrough(scope.entries[0], column, scope, depth);
  }
  return [];
}

/** Resolve a column against one scope entry (CTE/derived pass-through). */
function resolveThrough(entry, column, scope, depth) {
  if (entry.kind === "base") return [{ table: entry.name, column }];
  // CTE / derived: one-level pass-through, then stop at the boundary.
  if (depth > 0) {
    return [{ table: entry.name, column }];
  }
  // CTE defs arrive as a wrapper {tableList, columnList, ast} -- unwrap .ast.
  const raw = entry.def || scope.cteDefs.get(entry.name);
  const def = raw && raw.ast ? raw.ast : raw;
  if (!def || def.type !== "select") {
    return [{ table: entry.name, column }];
  }
  const inner = buildScope(def, scope);
  for (const item of Array.isArray(def.columns) ? def.columns : []) {
    if (outputName(item) === column) {
      const sources = resolveExpr(item.expr, inner, depth + 1);
      return sources.length > 0 ? sources : [{ table: entry.name, column }];
    }
  }
  // Column not materialized in the inner select list (e.g. SELECT *): stop
  // at the boundary name.
  return [{ table: entry.name, column }];
}

/** Resolve every column reference inside an expression -> deduped sources. */
function resolveExpr(expr, scope, depth) {
  const out = [];
  const seen = new Set();
  const push = (s) => {
    const key = s.table + "." + s.column;
    if (seen.has(key)) return;
    seen.add(key);
    out.push(s);
  };
  walk(expr, (o) => {
    const ref = normColumnRef(o);
    if (!ref) return;
    for (const s of resolveCol(ref.table, ref.column, scope, depth)) push(s);
  });
  return out;
}

/** Top-level SELECT lineage: one entry per output column of the select list. */
function collectLineage(ast, ctes) {
  if (ast.type !== "select") return [];
  const scope = buildScope(ast, ctes);
  const out = [];
  for (const item of Array.isArray(ast.columns) ? ast.columns : []) {
    const expr = item && item.expr;
    if (!expr) continue;
    const ref = normColumnRef(expr);
    let outputTable = null;
    if (ref && ref.table) {
      const entry =
        scope.aliasMap.get(ref.table) ||
        scope.entries.find((e) => e.name === ref.table && !e.alias) ||
        null;
      // Named behind the alias: CTE keeps its name; derived sources are
      // anonymous -> null.
      outputTable = entry ? entry.name : null;
    } else if (ref && !ref.table && scope.entries.length === 1 && scope.entries[0].name !== null) {
      outputTable = scope.entries[0].name;
    }
    out.push({
      output: { table: outputTable, column: outputName(item) },
      sources: resolveExpr(expr, scope, 0),
    });
  }
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
    const raw = o.dialect;
    if (typeof raw !== "string") {
      fail("dialect", "sql.parse: dialect must be a string", {
        dialect: typeof raw,
        supported: DIALECTS.slice(),
      });
    }
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
      // Stack exhaustion (deep nesting) is an engine limit, not SQL syntax.
      if (e instanceof RangeError) {
        fail("engine", "sql.parse: " + (e.message || "parser resource limit"), {
          dialect,
        });
      }
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
    // Belt-and-braces: a contract build over a hostile-but-valid AST (walk
    // stack exhaustion) must still surface as a structured error.
    let scope;
    let tables;
    let columns;
    let lineage;
    try {
      scope = collectScope(ast);
      tables = scope.sources.map((s) => ({
        name: s.name,
        schema: s.schema,
        alias: s.alias,
      }));
      // DDL (create/alter/drop): frozen contract -- columns/lineage are empty
      // (create_definitions happen to contain column_ref-shaped nodes; they are
      // schema definitions, not references).
      const isDdl = ast.type === "create" || ast.type === "alter" || ast.type === "drop";
      columns = isDdl ? [] : collectColumns(ast, scope);
      lineage = isDdl ? [] : collectLineage(ast, scope);
    } catch (e) {
      if (e && e.code === "OJ_SQL_PARSE") throw e;
      fail("engine", "sql.parse: contract extraction failed (" + (e && e.name) + ")", {
        dialect,
      });
    }
    return {
      dialect,
      tables,
      columns,
      ctes: scope.ctes,
      lineage,
      ast,
    };
  },
};
