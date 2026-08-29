# JavaScript Is All You Need

> `only-js` (codename **oj**) —— a low-code backend framework that embeds a JS/TS
> runtime (`deno_core` / V8) into Rust. Write `api.ts` organized by directory; the directory
> tree *is* the route table. Changes take effect immediately, and the build output is shippable.

---

## Motivation

In toB engagements you often need `low-code` for fast delivery. Low-code offerings on the
market vary wildly, but strip away the packaging and they are all fundamentally highly
configurable systems — and among all forms of configuration, **programmable configuration is
the highest tier**. The traditional approach is to embed a scripting engine (lua, js) inside a
backend language; js is the most popular choice thanks to its huge developer base.

The problem is that, whatever backend language you pick, the complexity never goes away. You
have to maintain a java / golang / c# backend stack *and* solve "how to embed and tame a js
runtime inside it"; meanwhile frontend/backend separation itself brings communication overhead
and knowledge-transfer friction.

This project aims to take the best of all worlds and **unify frontend and backend onto a
single language: JS/TS**.

### If Node.js is already good enough, why build another one?

Because "being able to run JS" was never the hard part — **"let the business write only JS,
and have the host absorb everything else" is**. The trade-offs here differ markedly from Node:

- **Delivery shape**: the core is a Rust binary. At runtime there is no `node_modules` and no
  toolchain to install; business modules build into versioned output directories plus a
  deterministic `.tgz` for publishing.
- **Safety rails sink into Rust**: dynamic SQL identifiers (table/column names) can only come
  from the Rust-side `SchemaRegistry` allowlist, and values only go through bound parameters —
  even a business-side mistake can't assemble an injection. Multi-tenancy, JWT auth, certificate
  validation, and static path-traversal guards all live in the host, not in business discipline.
- **Zero-config routing**: the directory mirror *is* the route — no registration code to write
  (see below).
- **Capability is pluggable**: DB dialects, S3, Redis, ES, Kafka/RabbitMQ are all **cdylib
  plugins**, loaded on demand; capabilities you don't install never enter the binary or its
  dependencies.
- **Controlled execution environment**: `JsRuntime`s are pooled and reused with a timeout
  watchdog (`KillSwitch`) — a single runaway request won't take down the process; failed
  runtimes are dropped rather than reused.
- **dev / release dual mode**: dev runs `.ts` directly (transpile on demand + hot reload),
  release runs prebuilt `.js` (no transpile, lock-aggregated). The same source auto-switches
  between the two modes.

---

## Quick Start

```bash
cargo build                                                   # first build pulls the prebuilt V8

# dev: run .ts sources directly (no manifests.yaml in dir → auto dev/ts, file changes apply live)
cargo run -p oj -- server -c sample/config.yaml --api-path sample/src

# release: build the artifacts first, apply migrations, then run dist/
# (manifests.yaml present → auto release/js; migrate is required by the verify gate)
cargo run -p oj -- build   -d sample/src -o sample/dist
cargo run -p oj -- migrate -c sample/config.yaml -d sample/dist
cargo run -p oj -- server  -c sample/config.yaml --api-path sample/dist
```

Modules own their data layer: a per-module `schema.yaml` (declarative tables, the source of
truth), `migrations/*.sql` (hand-written DDL evolution with a per-module ledger), and
`seed.sql`/`fixtures/`. `oj build --check` runs the structural checks (table ownership,
cross-module deps, seed discipline) without writing artifacts, and `oj schema diff` reports
drift between declarations and the live database.

On startup it prints the module list and route table, then:

```bash
curl 'http://localhost:9778/v1/api/user/account/?id=1'
# → {"code":0,"msg":"ok","data":[{"id":1,"name":"neo","role":"admin"}]}
```

> Under network restrictions set `V8_FROM_SOURCE=0` to force the prebuilt package — **do not**
> compile V8 from source.

---

## What a handler looks like

`api.ts` default-exports a method table (`get`/`post`/`put`/`del`/`patch`/`head`/`options`).
The globals are injected by the host — no import needed; `json.ok` / `json.fail` must be called
exactly once to end the session.

```ts
function get(): void {
  const id = Number(http.param("id", 0));
  db.query("select id, name, role from account where id = ?", [id])
    .then((r) => (r.length ? json.ok(r[0]) : json.fail(404, "no such account")))
    .catch((e) => json.fail(500, String(e)));
}

function post(): void {
  const b = http.body as { name?: string };
  if (!b.name) { json.fail(400, "name required"); return; }
  db.exec("insert into account (name) values (?)", [b.name])
    .then(() => json.ok({ created: true }))
    .catch((e) => json.fail(500, String(e)));
}

export default { get, post };
```

Responses are uniformly the `{code, msg, data}` envelope. Injected globals:

| Global | Purpose |
|---|---|
| `json` | `ok` / `fail` / `header` —— unified envelope and response headers |
| `http` | read-only request context: `method` / `param()` / `query` / `headers` / `body` / `tenantId` |
| `db` / `DB(name)` | SQL access; `db === DB("default")`, multiple DBs addressed by name |
| `kv` / `redis` | key-value store (in-memory implementation when Redis is unconfigured) |
| `blob(name)` | object storage (local / s3) |
| `bus` | pub/sub (`publish` / `subscribe`), broadcast across instances |
| `es` | Elasticsearch (`search` / `index` / `del`) |
| `ws` | WebSocket frame context |
| `fetch` | browser-compatible Fetch |
| `log` | structured logging (tracing) |
| `plugins()` | introspection of loaded plugins |
| `finish()` | end the session without writing a response |

---

## Directory-mirrored routing

The directory tree *is* the route table — no registration required:

```
sample/src/
  user/
    manifest.yaml            # name / desc / version (source of build artifact version)
    account/api.ts           → /v1/api/user/account/
    profile/detail/api.ts    → /v1/api/user/profile/detail/
    item/api.ts              → /v1/api/user/item/{id}   (see below)
    _shared/validate.ts      # leading underscore = private, not a route
  news/
    api.ts                   → /v1/api/news
    WS.ts                    → /v1/api/news/ws          (WebSocket)
```

- **Path params**: attach `.route` to a handler to override the directory mirror —
  `detail.route = "{id}"` makes `/v1/api/user/item/{id}` reachable (and `/v1/api/user/item`
  returns 404 in that case).
- **WebSocket**: `WS.ts` is executed once per received text frame. After the first frame does
  `bus.subscribe("news")`, any handler's `bus.publish("news", ...)` — including from other
  instances — broadcasts to that connection.
- **Prefix**: `/v1/api` comes from config `server.base`, overridable with `-b`.

---

## Configuration overview (`config.yaml`)

A block's presence enables it, its absence disables it — that is the governing principle of
configuration (full reference in `docs/user-manual.md`).

```yaml
server:
  host: "localhost"
  port: 9778
  base: "/v1/api"      # API prefix
  root: "dist"          # static site root (omitted = no static serving)
  timeout: "30s"        # per-request execution timeout (blown → 408)
  pool_size: 4          # JS execution concurrency
db:
  default: "sqlite://db.sqlite"     # multi-DB mixing: addressed by name via DB("name")
redis:  {}    # present → connect for real (fail-fast at startup); commented out → in-memory KV
es:     {}    # present → enables es.*
blob:         # present → enables blob.* + {base}/blob/{key} download route
  driver: "local"       # local | s3
  root: "uploads"
tenant:       # multi-tenancy: request must carry header_key, value injected as http.tenantId
  enable: true
  header_key: "X-TENANT-ID"
auth:         # JWT: built-in /v1/api/auth/{login,refresh,logout} + Bearer guard
  jwt_secret: "change-me"
  anonymous_paths: ["/health"]
```

---

## Architecture overview

```
only-js/
  src/                  core library: src/bridge/ (JS↔Rust bridge, backend axes) + src/config.rs
  oj/                  CLI binary: server / build / test subcommands (orchestration entry)
  server/              axum HTTP service: route lookup → run handler → write back Capture
  oj-plugin-ffi/       C-ABI contract shared by host and plugins (strict ABI_VERSION gate)
  plugins/             cdylib plugins: oj-es / oj-db-{mysql,postgres} / oj-blob-s3
                       / oj-bus-{kafka,rabbitmq} / oj-kv-redis
  tools/xtask/         plugin build / copy / preflight tooling (outputs to bin/)
  bin/                 build output: bin/oj (main) + bin/plugins/<triple>/ (plugin cdylibs)
  sample/              runnable example project (config.yaml + src/ + dist/)
  docs/                design and manuals
```

**Request path**: HTTP request → `server` catch-all routing (incl. built-in `/auth/*`,
`/blob/{key}`) → `RouteTable.lookup` → check out a `JsRuntime` from `RuntimePool` and reset
per-request state → run the matching method of `api.ts` (transpiled first in dev mode) →
capture the `{code,msg,data}` envelope → write back the response.

**JS↔Rust boundary**: `src/bridge/mod.rs` registers all `op_*` via `deno_core::extension!`,
and `bootstrap.js` assembles those ops into the globals table above. Each backend axis (db /
kv / blob / bus / es / fetch / http / ws) is its own module.

**Plugins**: at startup `dlopen` loads the cdylib, validates the ABI version and identity, then
wraps the plugin vtable as a core backend; a panic inside a plugin is contained by
`oj_plugin_entry!`'s `catch_unwind` into an error instead of aborting the host.

---

## Common development commands

```bash
cargo build --workspace                  # build all members (release; output to bin/)
cargo test                               # root crate unit tests
cargo test --workspace                   # full test run (incl. oj e2e)
cargo fmt --check                        # formatting gate
cargo clippy --all-targets -D warnings   # lint gate
cargo bench                              # criterion benchmarks

cargo run -p oj -- test -c sample/config.yaml    # run *.test.ts in-process (no server needed)
cargo xtask bin                                 # build oj and copy into bin/oj
cargo xtask plugin <name>                       # build plugin and copy into bin/plugins/<triple>/
cargo xtask plugin <name> --check               # plugin preflight (ABI / identity / semver / symbols)
cargo xtask build                               # build oj + all plugins into bin/
```

For async tests use `tokio::test(flavor = "current_thread")` — `JsRuntime` is `!Send`.
Do not use `deno test`: the globals a handler depends on exist only inside this bridge.

---

## Design red lines

- **SQL**: dynamic identifiers come only from the `SchemaRegistry` allowlist; values go only
  through bound parameters, never string concatenation.
- **`JsRuntime` is `!Send`**: the pool and its holder live on the same `current_thread`
  runtime; inspector/WS use `spawn_local`.
- **`panic = "unwind"`**: must hold across all plugin profiles, otherwise cross-boundary panic
  containment breaks.
- **`bootstrap.js` must stay 7-bit ASCII** (non-ASCII triggers a deno_core error).
- Failed runtimes are always dropped, never returned to the pool.

---

## Documentation

| Doc | Content |
|---|---|
| `docs/user-manual.md` | full `oj` CLI and `config.yaml` reference |
| `docs/dev-guide.md` / `docs/dev-manual.md` | developer manuals (incl. adding a new op) |
| `docs/bridge.md` | JS globals and module cross-reference |
| `docs/plugin-architecture.md` / `docs/plugin-development.md` | plugin architecture and development |
| `docs/route-params-design.md` | path-param routing design |
| `docs/testing.md` | testing conventions |
| `docs/migration.md` | migration runbook (schema.yaml / migrations / ledger / guards) |
| `docs/ops-manual.md` | operations |
| `docs/benchmarks.md` | performance data |
| `sample/README.md` | example project notes |

> Parts of `docs/dev-guide.md` describe an earlier in-process `Bridge` API and the initial
> plugin plan; for commands and structure this file and the code are authoritative.
