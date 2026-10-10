# JavaScript Is All You Need

> `only-js` (codename **oj**) is a low-code backend framework: a Rust binary with a JS/TS
> runtime (`deno_core` / V8) embedded inside. You write business logic as `api.ts` files in
> directories — the directory tree *is* the route table. Save a file and the change is live;
> when you're done, one build command produces a shippable artifact.

English | [简体中文](README_cn.md)

---

## Motivation

toB work often calls for low-code to deliver fast. Strip the packaging off any low-code
product and what remains is a highly configurable system — and of all forms of
configuration, **programmable configuration is the highest tier**. The traditional way to
get there is embedding a scripting engine (lua, js) inside a backend language; js is the
usual pick thanks to its huge developer base.

The catch: the complexity never goes away. You still maintain a java / golang / c# backend
stack, *and* you have to solve "how to embed and tame a js runtime inside it". On top of
that, frontend/backend separation adds its own communication and knowledge-transfer
overhead.

This project takes a different bet: **unify frontend and backend on one language — JS/TS** —
and let the Rust host absorb everything else.

### If Node.js is already good enough, why build another one?

Running JS was never the hard part. **Letting the business write only JS while the host
absorbs everything else is.** That is where oj's trade-offs differ from Node:

- **Delivery shape**: the core is a Rust binary. At runtime there is no `node_modules` and
  no toolchain to install. Business modules build into versioned output directories plus a
  deterministic `.tgz` for publishing.
- **Safety rails sink into Rust**: dynamic SQL identifiers (table/column names) can only
  come from the Rust-side `SchemaRegistry` allowlist, and values only go through bound
  parameters. A business-side mistake cannot assemble an injection. Multi-tenancy, JWT auth,
  OIDC (built-in OP + RP), certificate validation, and static path-traversal guards all live
  in the host — not in business discipline.
- **Zero-config routing**: directory mirroring *is* routing; not a single line of
  registration code (see below).
- **Pluggable capabilities**: database dialects, S3, Redis, ES, Kafka/RabbitMQ, SMTP, and
  LDAP are all **cdylib plugins**, loaded on demand. A capability you don't load adds no
  code and no dependency.
- **A contained execution environment**: `JsRuntime` pooling plus a watchdog (`KillSwitch`)
  means one runaway request cannot take down the process; a failed runtime is discarded,
  never reused.
- **dev / release dual mode**: dev runs `.ts` directly (on-demand transpile + hot reload);
  release runs pre-built `.js` (no transpile, aggregated by the build lock). Same source,
  mode auto-detected from directory contents.

---

## Quick Start

```bash
cargo xtask build            # build and stage bin/oj + bin/plugins/<triple>/ (release; pulls prebuilt V8 on first run)

# dev: run .ts sources directly (no manifests.yaml in the dir → auto dev/ts; edits are live)
./bin/oj serve -c sample/config.yaml --api-path sample/src

# release: build artifacts first, then serve dist/ (manifests.yaml present → auto release/js)
./bin/oj build  -d sample/src -o sample/dist
./bin/oj serve -c sample/config.yaml --api-path sample/dist
```

> Always run examples and business projects through the compiled **`bin/oj`** binary: it is
> produced in one shot by `cargo xtask build` (including all first-party plugin cdylibs),
> needs no cargo / Rust toolchain at runtime, and can be copied as-is between environments
> on the same platform — more portable and more consistent than `cargo run`. Command path
> arguments resolve against the **current working directory** (CWD); the examples below
> assume the repository root.

On startup the module list and route table are written to the log (**terminal is silent by
default**, file-only; add `--console-log` or set `server.console_log: true` to mirror to
the terminal):

```bash
tail -f sample/logs/server-*.log
```

```bash
curl 'http://localhost:9778/v1/api/user/account/?id=1'
# → {"code":0,"msg":"ok","data":[{"id":1,"name":"neo","role":"admin"}]}

curl http://localhost:9778/v1/api/plugins   # self-descriptions of loaded plugins (public endpoint)
```

> On restricted networks set `V8_FROM_SOURCE=0` to force the prebuilt V8 package — do
> **not** build V8 from source.

### About `bin/oj`: the single compiled entry point

`cargo xtask build` produces `bin/oj` (the main binary) and `bin/plugins/<host-triple>/`
(all first-party plugin cdylibs) in one shot. **All examples and business deployments go
through `bin/oj`**:

| Command | Purpose |
|---|---|
| `./bin/oj serve -c <config> --api-path <src\|dist>` | Start the server (dir with/without `manifests.yaml` auto-detects dev/ts vs release/js) |
| `./bin/oj build -d <src> -o <dist>` | Build modules: transpile TS → `dist/<module>-<version>/` + routes.js + manifests.yaml + .tgz |
| `./bin/oj test -c <config>` | Run `*.test.ts` cases in-process (no server needed) |
| `./bin/oj exec [-e code \| file \| --repl]` | Run a script with the full backend injected; pure computation needs **no config** |
| `./bin/oj migrate / test fixture / schema diff` | Migrations / fixture data / schema reconciliation |
| `./bin/oj openapi --check` | Generate OpenAPI 3.1 from the route table / CI drift gate |
| `./bin/oj secret keygen / seal / open` | Config credential sealing (the `ENC[...]` values in config) |
| `./bin/oj info` | phpinfo-style diagnostics (version/ABI/plugins/backend registry; values never printed) |

- **Portable**: `bin/oj` + `bin/plugins/<triple>/` is the self-contained release unit —
  the target machine needs no Rust / cargo / Node toolchain; copy the tree and run (or use
  `scripts/deploy.sh` to produce a release package). `cargo build --workspace` is for
  development; its artifacts are staged into `bin/` as well.
- **Consistent**: docs, CI, and production share one entry point — behavior does not drift
  with cargo invocation styles or feature resolution.
- **Path semantics**: CLI `--api-path` / `--app-path` resolve against the **current working
  directory** (CWD); `mounts:` api/web directories inside the config resolve against the
  config file's directory.
- **Admission gate (v0.1.58)**: startup is refused when the top-level `mounts:` table
  (config + CLI folding) is empty. `test` / `migrate` / `exec` / `openapi` accept
  `--site <prefix>` to select one api mount (that tree and its prefix only).

---

## What a handler looks like

`api.ts` default-exports a method table (`get`/`post`/`put`/`del`/`patch`/`head`/`options`).
Globals are injected by the host — no imports; exactly one of `json.ok` / `json.fail` must
be called to end the session.

```ts
function get(): void {
  const id = Number(http.param("id", 0));
  db.table("account").select(["id", "name", "role"]).where({ field: "id", op: "eq", value: id })
    .all()
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

Responses are the unified `{code, msg, data}` envelope. Injected globals:

| Global | Purpose |
|---|---|
| `json` | `ok` / `fail` / `header` / `redirect` / `stream` / `sse` — envelope, headers, 3xx, streaming |
| `http` | Read-only request context: `method` / `param()` / `query` / `headers` / `body` / `files` / `tenantId` / `user` |
| `db` / `DB(name)` | SQL access; `db === DB("default")`, named databases; `db.asSystem` / `db.asTenant` request-scoped identity |
| `kv` / `redis` | Key-value store (in-memory implementation when Redis is not configured) |
| `blob(name)` | Object storage (local / s3; large-file direct upload via `blob.uploadUrl`) |
| `bus` | Pub/sub (`publish` / `subscribe`), broadcast across instances |
| `es` | Elasticsearch (`search` / `index` / `del`) |
| `mail` | SMTP delivery (`send` / `enqueue`; allowlist enforced in the host) |
| `Kafka(name)` / `RabbitMQ(name)` | Named MQ clients (long-task context) |
| `tasks` | Long tasks (`src/tasks/` pooled tasks + crontab) |
| `ws` | WebSocket frame context (`join` / `broadcast` room primitives) |
| `cert` / `jwt` / `bcrypt` / `crypto` | Certificate / JWT sign-verify / password hashing / AES-GCM primitives |
| `oidc` / `ldap` / `ldap(name)` | OIDC primitives and LDAP directory |
| `fs` | Local files (jailed to `fs.root`) |
| `vars` | Deployment-time constants (config `vars:` section, fail-closed) |
| `plugins()` | Introspection of loaded plugins |
| `ojInfo()` | Runtime diagnostics (same source as `oj info`) |
| `finish()` | End the session without writing a response |

---

## Directory-mirror routing

The directory tree **is** the route table — no registration:

```
sample/src/
  user/
    manifest.yaml            # name / desc / version (source of artifact versions)
    account/api.ts           → /v1/api/user/account/
    profile/detail/api.ts    → /v1/api/user/profile/detail/
    item/api.ts              → /v1/api/user/item/{id}   (see below)
    _shared/validate.ts      # underscore prefix = private, no route
  news/
    api.ts                   → /v1/api/news
    ws.ts                    → /v1/api/news/ws          (WebSocket)
```

- **Path parameters**: attach `.route` to a handler to replace the mirror —
  `detail.route = "{id}"` makes `/v1/api/user/item/{id}` reachable (`/v1/api/user/item`
  becomes 404).
- **Import aliases**: no more counting `../` — `#_shared/validate` anchors at the
  **module root** (`user/_shared/validate.ts`), `#/user/_shared/validate` anchors at the
  **src root**. Anchors derive from the importing file's own location, so the same spelling
  works in dev and release; `oj build` materializes them as versioned relative paths
  (cross-module aliases require `deps` in `manifest.yaml`).
- **WebSocket**: `ws.ts` runs once per received text frame. After the first frame calls
  `bus.subscribe("news")`, any handler's `bus.publish("news", ...)` (on any instance)
  broadcasts to that connection.
- **Prefix (v0.1.58)**: `/v1/api` comes from the api mount's `prefix` — mounts carry their
  own full URL prefix; the URL is what you write. Multiple API trees / static sites are just
  more mount lines (longest-prefix match, no cross-mount fallback).

---

## Configuration overview (`config.yaml`)

A section present = enabled; absent = disabled — that is the governing principle (full
reference and the **legacy-key migration guide** live in `docs/user-manual.md` §3/§3.1).

```yaml
# Mount table (v0.1.58): one line = one URL prefix + one directory (api/web, exactly one).
# Legacy server.api_prefix / app_path / app_prefix / static_sites have been removed.
mounts:
  - prefix: "/v1/api"    # api mount: /v1/api/<module>/...
    api: "src"           #   runs src in dev, dist in release — auto-detected per mount
  - prefix: "/"          # web mount: static site (GET/HEAD only; other methods → 405)
    web: "dist"
    # spa: true          # SPA deep-link fallback (default false; opt-in)
server:
  host: "localhost"
  port: 9778
  timeout: "30s"        # per-request execution timeout (circuit-breaks → 408)
  pool_size: 4          # JS execution concurrency
db:
  default: "sqlite://db.sqlite"     # multiple databases: DB("name")
redis:  {}    # present = real connection (fail-fast at startup); commented out → in-memory KV
es:     {}    # present = enable es.*
blob:         # present = enable blob.* + the {mount-prefix}/blob/{key} download route
  driver: "local"       # local | s3
  root: "uploads"
tenant:       # multi-tenancy: requests must carry header_key; injected as http.tenantId
  enable: true
  header_key: "X-TENANT-ID"
auth:         # JWT: oj-auth plugin guard (Bearer/cookie) + auth business routes (sample/src/auth/)
  jwt_secret: "change-me"
  anonymous_paths: ["/health"]
```

---

## Architecture overview

```
only-js/
  src/                 core library: src/bridge/ (JS↔Rust bridge, backend axes) + src/config.rs
  oj/                  CLI binary: serve / build / test / exec / migrate / schema / secret
                       / openapi / info subcommands (orchestration entry)
  serve/               axum HTTP service: mount dispatch → run handler → write back Capture
  oj-plugin-ffi/       C-ABI contract shared by host and plugins (strict ABI_VERSION gate)
  plugins/             cdylib plugins: oj-es / oj-db-{mysql,postgres} / oj-blob-s3
                       / oj-bus-{kafka,rabbitmq} / oj-kv-redis / oj-auth
                       / oj-mail / oj-ldap
  tools/xtask/         plugin build / stage / preflight (artifacts staged into bin/)
  bin/                 build artifacts: bin/oj (main binary) + bin/plugins/<triple>/ (plugin cdylibs)
  sample/              runnable sample project (config.yaml + src/ + dist/)
  docs/                design docs and manuals
```

**Request path**: HTTP request → `serve` picks a mount by longest-prefix match (api mount:
built-in `/health`, `/plugins`, `/blob/{key}` → route table → pre-handler pipeline
(auth + tenant) → dev directory-mirror fallback; web mount: static serving) → check out a
`JsRuntime` from the `RuntimePool`, reset per-request state → run the matching method of
`api.ts` (transpiled first in dev mode) → capture the `{code,msg,data}` envelope → write
the response.

**JS↔Rust boundary**: `src/bridge/mod.rs` registers all `op_*` via `deno_core::extension!`;
`bootstrap.js` assembles those ops into the globals listed above. Each backend axis
(db / kv / blob / bus / es / fetch / http / ws / mail / ldap) lives in its own module.

**Plugins**: cdylibs are `dlopen`ed at startup; after ABI and identity checks the plugin
vtables are wrapped into core backends. A panic inside a plugin is contained by
`oj_plugin_entry!`'s `catch_unwind` into an error — the host never aborts. Business
endpoints (login/logout etc.) are ordinary business routes (see `sample/src/auth/`),
protected by the oj-auth plugin's Bearer/cookie guard.

---

## Common development commands

```bash
cargo build --workspace                  # build all members (release, staged into bin/)
cargo test --release                    # root-crate unit tests (release)
cargo test --release --workspace         # full test suite (incl. oj e2e, release)
cargo fmt --check                        # format gate
cargo clippy --release --all-targets -- -D warnings   # lint gate (release)
cargo bench                              # criterion benchmarks

./bin/oj test -c sample/config.yaml             # run *.test.ts in-process (no server)
cargo xtask bin                                 # build oj and stage into bin/oj
cargo xtask plugin <name>                       # build a plugin into bin/plugins/<triple>/
cargo xtask plugin <name> --check               # plugin preflight (ABI / identity / semver / symbols)
cargo xtask build                               # build oj + all plugins into bin/
cargo xtask smoke --bin bin/oj                  # release gate (minimal oj build with build-machine sources hidden)
```

Async tests must use `tokio::test(flavor = "current_thread")` — `JsRuntime` is `!Send`.
Do not use `deno test`: the globals handlers rely on exist only in this bridge.

---

## Design red lines

- **SQL**: dynamic identifiers come only from the `SchemaRegistry` allowlist; values only
  through bound parameters — never concatenated.
- **`JsRuntime` is `!Send`**: the pool and everything holding it stays on a
  `current_thread` runtime; inspector/WS use `spawn_local`.
- **`panic = "unwind"`**: keep it in every plugin profile, or cross-boundary panic
  containment stops working.
- **`bootstrap.js` must stay 7-bit ASCII** (non-ASCII triggers a deno_core error).
- **Config credentials**: passwords/keys in config are always sealed as `ENC[...]`
  (`oj secret seal`); private keys stay on the deploy machine and never enter the repo.
- A failed runtime is always discarded, never returned to the pool.

---

## Docs

| Doc | Contents |
|---|---|
| `docs/user-manual.md` | Full `oj` CLI and `config.yaml` reference (§3.1 legacy-key migration guide) |
| `docs/dev-guide.md` | Development manual (incl. how to add a new op) |
| `docs/devkit/api-manual.md` | Business development manual (global-object API, scenarios; ships as `devkit/`) |
| `docs/bridge.md` | JS globals ↔ modules map |
| `docs/plugins/plugin-development.md` | Plugin architecture and development |
| `docs/testing.md` | Testing conventions |
| `docs/ops-manual.md` | Operations |
| `docs/benchmarks.md` | Performance data |
| `sample/README.md` | Sample project guide |

> `docs/dev-guide.md` is the development manual (day-to-day work + internal walkthrough,
> merged); commands and structure defer to this file and the code.
