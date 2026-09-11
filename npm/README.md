# @oj-bin/oj

Prebuilt binaries for **only-js** (`oj`) — a low-code backend framework that embeds a
JavaScript/TypeScript runtime (V8, via `deno_core`) into Rust. Write business logic as
JS/TS handlers; Rust serves them over HTTP with injected globals (`db`, `kv`, `blob`,
`bus`, `es`, `fetch`, `WebSocket`, …).

## Install

```bash
npm i @oj-bin/oj
```

A postinstall script copies the binaries for your platform into **`./bin/`** of the
directory where you ran `npm i`:

```
bin/oj                     # main CLI (oj.exe on Windows)
bin/plugins/<triple>/      # backend plugin cdylibs (db/kv/blob/bus/es/auth)
bin/devkit/                # API manual + global.d.ts
```

```bash
./bin/oj server -c config.yaml --api-path src
```

## Supported platforms

| platform | triple |
|---|---|
| linux x64 (glibc) | `x86_64-unknown-linux-gnu` |
| macOS arm64 | `aarch64-apple-darwin` |
| windows x64 (msvc) | `x86_64-pc-windows-msvc` |

Other platforms: download from
[GitHub Releases](https://github.com/everpan/only-js/releases) or build from source.

## Notes

- **pnpm ≥ 10** does not run dependency lifecycle scripts by default; add to
  `pnpm-workspace.yaml`:
  ```yaml
  onlyBuiltDependencies: ["@oj-bin/oj"]
  ```
- If you install with `--ignore-scripts`, run the installer manually:
  `node node_modules/@oj-bin/oj/postinstall.js`
- Global install (`npm i -g`) is **not** supported (binaries land in `./bin/` of the
  current project). Use a project-local install or the GitHub Release archives.
- China mirrors: npmmirror syncs this package automatically —
  `npm i @oj-bin/oj --registry=https://registry.npmmirror.com`

Repo & docs: <https://github.com/everpan/only-js>
