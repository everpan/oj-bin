# 移除 --grace-days + tools/oj-cert 证书工具 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除 `oj server --grace-days` CLI 参数（宽限天数仅由 config 提供）；新建独立工具 crate `tools/oj-cert`（RSA 密钥对 + JWS 证书生成与重签续期）。

**Architecture:** Part 1 纯删除（args/server_cmd/main.rs 测试/文档）。Part 2 新 bin crate `oj-cert`（`src/lib.rs` 持有 gen/renew 逻辑供集成测试直测，`src/main.rs` 为 clap 薄壳）；证书格式与 `server/src/certificate.rs` 契约一致（JWS 三段式，header `{"alg":"RS256","typ":"JWT"}`，payload `{nbf,exp}`，RS256）。

**Tech Stack:** clap 4.5 (derive)、rsa 0.9（features=["pem"]；ring 不能生成 RSA 密钥）、base64 0.23、serde_json 1。

**Spec:** `docs/superpowers/specs/2026-08-28-grace-days-cli-removal-and-oj-cert-design.md`

## Global Constraints

- 禁止 debug 构建：一律 `cargo build --release` / `cargo test`（测试不受限）。
- 门禁：`cargo fmt --check` + `cargo clippy --all-targets -D warnings` 每个任务结束前必须过。
- `oj-cert` **不进 `bin/`、不随 oj 发行**：`tools/xtask` 一律不动（用户明确否决）。
- `src/config.rs` 的 `server.grace_days` 字段与默认值 `Some(30)` **不动**。
- 证书契约对齐 `server/src/certificate.rs`：只认 RS256；loader 只读 payload 的 `nbf`/`exp`；公钥 PEM 接受 SPKI/PKCS#1（工具产出 SPKI）。
- 新代码注释与文档用中文，风格对齐仓库现有文件（模块顶部 `//!` 说明 + 行内注释）。
- commit message 中文、conventional 前缀（fix/feat/refactor/docs），结尾附空行 + `unix@vip.qq.com ai`。

---

### Task 1: 移除 `--grace-days` CLI 参数

**Files:**
- Modify: `oj/src/args.rs`（ServerArgs 字段、clap 定义、to_command、测试）
- Modify: `oj/src/server_cmd.rs:30-33`（覆盖合并块）
- Modify: `oj/src/main.rs:105`（测试字面量）
- Modify: `docs/user-manual.md:30,44`、`docs/dev-manual.md:247`

**Interfaces:**
- Consumes: 无。
- Produces: `ServerArgs` 不再含 `grace_days` 字段（`Command::Server(ServerArgs)` 构造点全部同步）；`cfg.server.grace_days` 仅来自 config。

- [ ] **Step 1: 写失败断言（回归护栏）**

在 `oj/src/args.rs` 测试 `bad_usage_is_clap_error` 中、`--dev 已删` 断言之后加：

```rust
        // --grace-days 已删：宽限天数仅由 config 的 server.grace_days 提供
        assert!(cli(&["server", "--grace-days", "30"]).is_err());
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p oj --lib args`
Expected: FAIL —— `ServerArgs`/`Commands::Server` 仍含 `grace_days`，但 clap 定义删除前 CLI 仍接受该旗标，断言不成立。

- [ ] **Step 3: 删除代码触点**

1. `oj/src/args.rs:18-19` 删 `ServerArgs.grace_days` 字段及文档注释。
2. `oj/src/args.rs:81-83` 删 `Commands::Server` 的 `grace_days` clap 参数及文档注释。
3. `oj/src/args.rs` `to_command` 中 `Commands::Server` 分支：解构与构造两处各删 `grace_days,`。
4. `oj/src/server_cmd.rs`：注释 `// CLI 覆盖：证书路径与宽限天数（若有）。` 改为 `// CLI 覆盖：证书路径（若有）。`，并删除：

```rust
    if let Some(d) = a.grace_days {
        cfg.server.grace_days = Some(d);
    }
```

5. `oj/src/main.rs:105` 删测试字面量中的 `grace_days: None,`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p oj`
Expected: PASS（含新护栏断言与 `server_missing_config_returns_one`）。

- [ ] **Step 5: 同步文档**

1. `docs/user-manual.md:30`：行尾删 ` [--grace-days <n>]`，改为：
   `oj server [-c config.yaml] [-b /v1/api] [-d <src|dist>] [--cert-path <jws>] [--key-path <pem>]`
2. `docs/user-manual.md:44`：删整行表格行 `| \`--grace-days\` | \`server.grace_days\`（默认 30） | （server）证书过期后宽限天数，覆盖 \`server.grace_days\` |`
3. `docs/dev-manual.md:247`：行尾删 ` --grace-days 15`。

- [ ] **Step 6: 门禁 + 提交**

```bash
cargo fmt --check && cargo clippy --all-targets -D warnings
git add -A && git commit -m "refactor(oj): 移除 --grace-days CLI 参数，宽限天数仅由 config 提供

宽限期属运维策略，随配置走，不随启动命令漂移。server.grace_days
（默认 30）字段保留。

unix@vip.qq.com ai"
```

---

### Task 2: `tools/oj-cert` crate 骨架 + gen 子命令

**Files:**
- Modify: `Cargo.toml:2`（members 加 `"tools/oj-cert"`）
- Create: `tools/oj-cert/Cargo.toml`
- Create: `tools/oj-cert/src/lib.rs`
- Create: `tools/oj-cert/src/main.rs`
- Test: `tools/oj-cert/tests/cert_gen.rs`

**Interfaces:**
- Consumes: 无（独立 crate，不依赖 only-js）。
- Produces（Task 3 依赖，签名精确如下）：
  - `pub const MIN_BITS: u32 = 2048;`
  - `pub fn now_secs() -> u64`
  - `pub struct GenOpts { pub out_dir: PathBuf, pub bits: u32, pub nbf: u64, pub exp: u64 }`
  - `pub struct RenewOpts { pub key_path: PathBuf, pub out_dir: Option<PathBuf>, pub nbf: u64, pub exp: u64 }`
  - `pub fn gen(opts: &GenOpts) -> Result<PathBuf, String>`（写 private.pem/public.pem/cert.jws，返回 cert.jws 路径）
  - `pub fn renew(opts: &RenewOpts) -> Result<PathBuf, String>`（Task 3 实现；写 cert.jws，返回其路径）

- [ ] **Step 1: 建 crate 骨架**

`Cargo.toml`（根）members 行尾部 `"tests/plugins/mini"` 前插入 `"tools/oj-cert"`：

```toml
members = ["server", "oj", "oj-plugin-ffi", "plugins/oj-es", "plugins/oj-db-mysql", "plugins/oj-db-postgres", "plugins/oj-blob-s3", "plugins/oj-bus-kafka", "plugins/oj-bus-rabbitmq", "plugins/oj-kv-redis", "tools/xtask", "tools/oj-cert", "tests/plugins/mini"]
```

`tools/oj-cert/Cargo.toml`：

```toml
[package]
name = "oj-cert"
version = "0.1.0"
edition = "2024"
description = "证书工具：RSA 密钥对 + JWS 证书生成（gen）与重签续期（renew）；tools/ 独立工具，不随 oj 发行"

[dependencies]
# rsa 需要 pem feature：to_pkcs8_pem / from_pkcs8_pem 等 PEM 编解码在其后。
# ring 只能验签/签名不能生成 RSA 密钥，keygen 用纯 Rust 的 rsa。
rsa = { version = "0.9", features = ["pem"] }
base64 = "0.23.1"
clap = { version = "4.5", features = ["derive"] }
serde_json = "1"
```

`tools/oj-cert/src/lib.rs` 暂为 `//! 占位` 一行；`src/main.rs` 暂为 `fn main() {}`。

Run: `cargo build -p oj-cert`
Expected: 编译通过（拉取 rsa 依赖）。

- [ ] **Step 2: 写失败测试**

`tools/oj-cert/tests/cert_gen.rs`：

```rust
//! 集成测试：gen/renew 产物用 rsa 独立解码 + 验签（与生成路径不对称，可捕格式错误）。

use base64::Engine;
use oj_cert::{GenOpts, MIN_BITS, gen};
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::Sha256;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use std::path::PathBuf;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("oj-cert-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// 独立验签：解析 SPKI PEM + JWS 三段，重放 RS256 verify。
fn verify_jws(public_pem: &str, jws: &str, nbf: u64, exp: u64) -> Result<(), String> {
    let parts: Vec<&str> = jws.trim().split('.').collect();
    assert_eq!(parts.len(), 3, "jws must have 3 parts");
    let dec = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    let header: serde_json::Value =
        serde_json::from_slice(&dec(parts[0])).map_err(|e| e.to_string())?;
    assert_eq!(header["alg"], "RS256");
    assert_eq!(header["typ"], "JWT");
    let payload: serde_json::Value =
        serde_json::from_slice(&dec(parts[1])).map_err(|e| e.to_string())?;
    assert_eq!(payload["nbf"], nbf);
    assert_eq!(payload["exp"], exp);
    let pub_key = RsaPublicKey::from_public_key_pem(public_pem).map_err(|e| e.to_string())?;
    let sig = Signature::try_from(dec(parts[2]).as_slice()).map_err(|e| e.to_string())?;
    VerifyingKey::<Sha256>::new(pub_key)
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .map_err(|_| "signature verify failed".to_string())
}

#[test]
fn gen_writes_and_verifies() {
    let dir = tmpdir("gen");
    let (nbf, exp) = (1_000_000_u64, 2_000_000_u64);
    gen(&GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf,
        exp,
    })
    .unwrap();
    let jws = std::fs::read_to_string(dir.join("cert.jws")).unwrap();
    let pub_pem = std::fs::read_to_string(dir.join("public.pem")).unwrap();
    let priv_pem = std::fs::read_to_string(dir.join("private.pem")).unwrap();
    assert!(priv_pem.starts_with("-----BEGIN PRIVATE KEY-----"), "{priv_pem}");
    assert!(pub_pem.starts_with("-----BEGIN PUBLIC KEY-----"), "{pub_pem}");
    verify_jws(&pub_pem, &jws, nbf, exp).unwrap();
    // unix 下私钥落盘即 600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("private.pem")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_bad_params() {
    let dir = tmpdir("bad");
    let e = gen(&GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf: 100,
        exp: 100,
    })
    .unwrap_err();
    assert!(e.contains("exp must be greater"), "{e}");
    let e = gen(&GenOpts {
        out_dir: dir.clone(),
        bits: 1024,
        nbf: 1,
        exp: 2,
    })
    .unwrap_err();
    assert!(e.contains("bits must be"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test -p oj-cert`
Expected: FAIL —— `oj_cert::gen`/`GenOpts`/`MIN_BITS` 未定义（编译错误即失败）。

- [ ] **Step 4: 实现 lib.rs**

`tools/oj-cert/src/lib.rs` 整体替换：

```rust
//! oj-cert 核心：JWS/RS256 证书生成与重签。
//!
//! 证书格式与 server/src/certificate.rs 契约一致：header `{"alg":"RS256","typ":"JWT"}`
//! + payload `{nbf, exp}`（Unix 秒）+ RS256 签名；公钥 PEM 为 SPKI（loader 兼容）。

use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::{RsaPrivateKey, RsaPublicKey};
use std::path::{Path, PathBuf};

/// ring `RSA_PKCS1_2048_8192_SHA256` 验签下限。
pub const MIN_BITS: u32 = 2048;

/// 生成参数（gen）。
#[derive(Debug, Clone)]
pub struct GenOpts {
    /// 输出目录（private.pem / public.pem / cert.jws）。
    pub out_dir: PathBuf,
    /// RSA 位数（下限 MIN_BITS）。
    pub bits: u32,
    /// 生效时间（Unix 秒）。
    pub nbf: u64,
    /// 过期时间（Unix 秒，须 > nbf）。
    pub exp: u64,
}

/// 重签参数（renew）。
#[derive(Debug, Clone)]
pub struct RenewOpts {
    /// 现有私钥（PKCS#8 PEM）。
    pub key_path: PathBuf,
    /// cert.jws 输出目录；None = 私钥所在目录。
    pub out_dir: Option<PathBuf>,
    /// 生效时间（Unix 秒）。
    pub nbf: u64,
    /// 过期时间（Unix 秒，须 > nbf）。
    pub exp: u64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

/// JWS 三段拼接（b64url no-pad，与 server 加载端 split('.') 对齐）。
fn jws(signing_key: &SigningKey<Sha256>, nbf: u64, exp: u64) -> String {
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" }).to_string();
    let payload = serde_json::json!({ "nbf": nbf, "exp": exp }).to_string();
    let h = b64(header.as_bytes());
    let p = b64(payload.as_bytes());
    let sig = signing_key.sign(format!("{h}.{p}").as_bytes()).to_vec();
    format!("{h}.{p}.{}", b64(&sig))
}

/// 私钥落盘：PKCS#8 PEM；unix 下 chmod 600。
fn write_private(path: &Path, key: &RsaPrivateKey) -> Result<(), String> {
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| format!("encode pkcs8: {e}"))?;
    std::fs::write(path, pem.to_string()).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

/// gen：生成密钥对 + JWS，写 private.pem / public.pem / cert.jws，返回 cert.jws 路径。
pub fn gen(opts: &GenOpts) -> Result<PathBuf, String> {
    if opts.bits < MIN_BITS {
        return Err(format!("bits must be >= {MIN_BITS}"));
    }
    if opts.exp <= opts.nbf {
        return Err("exp must be greater than nbf".into());
    }
    std::fs::create_dir_all(&opts.out_dir)
        .map_err(|e| format!("mkdir {}: {e}", opts.out_dir.display()))?;
    let key = RsaPrivateKey::new(&mut OsRng, opts.bits).map_err(|e| format!("keygen: {e}"))?;
    let signing = SigningKey::<Sha256>::new(key.clone());
    let out = |name: &str| opts.out_dir.join(name);
    write_private(&out("private.pem"), &key)?;
    let pub_pem = key
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .map_err(|e| format!("encode spki: {e}"))?;
    std::fs::write(out("public.pem"), pub_pem).map_err(|e| format!("write public.pem: {e}"))?;
    std::fs::write(out("cert.jws"), jws(&signing, opts.nbf, opts.exp))
        .map_err(|e| format!("write cert.jws: {e}"))?;
    Ok(out("cert.jws"))
}

/// renew：读现有私钥重签，只写新 cert.jws，返回其路径。
///
/// 公钥不变 → config 不改，配合 server 证书热重载免重启续期。
pub fn renew(opts: &RenewOpts) -> Result<PathBuf, String> {
    if opts.exp <= opts.nbf {
        return Err("exp must be greater than nbf".into());
    }
    let pem = std::fs::read_to_string(&opts.key_path)
        .map_err(|e| format!("read key {}: {e}", opts.key_path.display()))?;
    let key = RsaPrivateKey::from_pkcs8_pem(&pem)
        .map_err(|e| format!("parse private key (PKCS#8 PEM): {e}"))?;
    let dir = match &opts.out_dir {
        Some(d) => d.clone(),
        None => opts
            .key_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    };
    let path = dir.join("cert.jws");
    std::fs::write(&path, jws(&SigningKey::<Sha256>::new(key), opts.nbf, opts.exp))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}
```

`tools/oj-cert/src/main.rs` 整体替换（clap 薄壳；`Renew` 分支在此一并写入，Task 3 只做其测试与文档）：

```rust
//! oj-cert：oj 的 JWS 证书生成/重签工具（tools/ 独立工具，不随 oj 发行）。
//!
//!   oj-cert gen   -o <dir> [--days 365] [--bits 2048] [--nbf <unix>] [--exp <unix>]
//!   oj-cert renew -k <private.pem> [-o <dir>] [--days 365] [--exp <unix>]

use clap::{Parser, Subcommand};
use oj_cert::{GenOpts, RenewOpts};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "oj-cert", version, about = "oj JWS 证书生成工具")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 生成 RSA 密钥对 + JWS 证书（private.pem / public.pem / cert.jws）
    Gen {
        /// 输出目录
        #[arg(short, long)]
        out: PathBuf,
        /// 有效天数（exp = now + days；--nbf/--exp 显式值优先）
        #[arg(long, default_value_t = 365)]
        days: u64,
        /// RSA 位数（下限 2048）
        #[arg(long, default_value_t = 2048)]
        bits: u32,
        /// 生效时间（Unix 秒；缺省 now）
        #[arg(long)]
        nbf: Option<u64>,
        /// 过期时间（Unix 秒；缺省 now + days）
        #[arg(long)]
        exp: Option<u64>,
    },
    /// 用现有私钥重签续期（只写新 cert.jws；公钥不变，config 无需改动）
    Renew {
        /// 现有私钥（PKCS#8 PEM）
        #[arg(short, long)]
        key: PathBuf,
        /// cert.jws 输出目录（缺省 = 私钥所在目录）
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// 有效天数（exp = now + days；--exp 显式值优先）
        #[arg(long, default_value_t = 365)]
        days: u64,
        /// 过期时间（Unix 秒；缺省 now + days）
        #[arg(long)]
        exp: Option<u64>,
    },
}

fn main() {
    let cli = Cli::parse();
    let now = oj_cert::now_secs();
    let result = match cli.cmd {
        Cmd::Gen {
            out,
            days,
            bits,
            nbf,
            exp,
        } => oj_cert::gen(&GenOpts {
            out_dir: out,
            bits,
            nbf: nbf.unwrap_or(now),
            exp: exp.unwrap_or(now + days),
        }),
        Cmd::Renew { key, out, days, exp } => oj_cert::renew(&RenewOpts {
            key_path: key,
            out_dir: out,
            // renew 无 --nbf：避免误缩已生效窗口（spec 决策）。
            nbf: now,
            exp: exp.unwrap_or(now + days),
        }),
    };
    match result {
        Ok(path) => println!("written: {}", path.display()),
        Err(e) => {
            eprintln!("oj-cert: {e}");
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test -p oj-cert`
Expected: PASS（2 个测试）。gen 测试含 RSA 2048 keygen，首次运行约数秒属正常。

- [ ] **Step 6: 门禁 + 提交**

```bash
cargo run -p oj-cert -- gen -o /tmp/oj-cert-smoke && ls -la /tmp/oj-cert-smoke && rm -rf /tmp/oj-cert-smoke
cargo fmt --check && cargo clippy --all-targets -D warnings
git add -A && git commit -m "feat(tools): 新增 oj-cert 证书工具（gen：RSA 密钥对 + JWS 证书生成）

JWS/RS256 三段式与 server/src/certificate.rs 契约一致；私钥 PKCS#8
（unix 600）、公钥 SPKI、payload 仅 nbf/exp。独立 crate 不依赖
only-js，不进 bin/、不随 oj 发行（spec 决策）。

unix@vip.qq.com ai"
```

---

### Task 3: renew 测试 + 文档收口

**Files:**
- Test: `tools/oj-cert/tests/cert_gen.rs`（追加 renew 测试）
- Modify: `docs/ops-manual.md:146`（故障恢复行指向 oj-cert renew）

**Interfaces:**
- Consumes: Task 2 的 `renew(&RenewOpts) -> Result<PathBuf, String>`、`verify_jws` 测试辅助。
- Produces: 无。

- [ ] **Step 1: 写失败测试**

`tools/oj-cert/tests/cert_gen.rs` 顶部 use 行改为 `use oj_cert::{GenOpts, MIN_BITS, RenewOpts, gen, renew};`（补上 renew 所需导入），文件末尾追加：

```rust
#[test]
fn renews_with_moved_exp_and_same_key() {
    let dir = tmpdir("renew");
    gen(&GenOpts {
        out_dir: dir.clone(),
        bits: MIN_BITS,
        nbf: 1_000_000,
        exp: 1_000_100,
    })
    .unwrap();
    let new_exp = 3_000_000_u64;
    let out = renew(&RenewOpts {
        key_path: dir.join("private.pem"),
        out_dir: Some(dir.clone()),
        nbf: 1_000_000,
        exp: new_exp,
    })
    .unwrap();
    assert_eq!(out, dir.join("cert.jws"));
    let pub_pem = std::fs::read_to_string(dir.join("public.pem")).unwrap();
    verify_jws(&pub_pem, &std::fs::read_to_string(&out).unwrap(), 1_000_000, new_exp).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn renew_rejects_bad_exp_and_missing_key() {
    let dir = tmpdir("renew-bad");
    let e = renew(&RenewOpts {
        key_path: dir.join("no.pem"),
        out_dir: None,
        nbf: 100,
        exp: 100,
    })
    .unwrap_err();
    assert!(e.contains("exp must be greater"), "{e}");
    let e = renew(&RenewOpts {
        key_path: dir.join("no.pem"),
        out_dir: None,
        nbf: 100,
        exp: 200,
    })
    .unwrap_err();
    assert!(e.contains("read key"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: 跑测试确认通过**

Run: `cargo test -p oj-cert`
Expected: PASS —— `renew` 已在 Task 2 实现，此处补测试即应通过（若 FAIL 则修复 `renew` 实现后重跑）。

- [ ] **Step 3: 文档收口**

`docs/ops-manual.md:146` 表格行整行替换为：

```markdown
| 启动报 `certificate expired` / `certificate has expired and grace period elapsed` | 证书已过期且宽限期结束（`exp` + `grace_days` 仍早于现在） | 重签续期：构建 `cargo build -p oj-cert --release`（工具在 `tools/oj-cert`，不随发行包）后 `oj-cert renew -k private.pem` 使 `exp` 晚于现在，替换证书文件后重启（运行中替换则热重载即时生效）；调大 `grace_days` 仅延长宽限、不改 `exp` |
```

- [ ] **Step 4: 全量门禁 + 提交**

```bash
cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test --workspace
git add -A && git commit -m "feat(tools): oj-cert renew 重签续期 + ops 手册指向工具

renew 用现有私钥重签（公钥不变，config 无需改动），配合证书热重载
免重启续期；CLI 无 --nbf 防误缩已生效窗口。

unix@vip.qq.com ai"
```

---

## 完成定义

- [ ] `cargo test -p oj`：`--grace-days` 拒绝断言通过，存量测试全绿。
- [ ] `cargo test -p oj-cert`：gen/renew/非法参数 4 测试全绿。
- [ ] `cargo fmt --check && cargo clippy --all-targets -D warnings && cargo test --workspace` 全绿。
- [ ] `tools/xtask` 与 `src/config.rs` 零改动（`git diff --stat` 核对）。
