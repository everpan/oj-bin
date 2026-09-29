//! `oj secret`：config 凭据密封工具（keygen / seal / open）。
//!
//! 用法见 `docs/devkit/SKILL.md`；安全模型见 `only_js::secret` 模块头。
//! 要点：**明文优先从 stdin 读**——命令行参数会进 shell history 与 `ps` 输出，
//! 等于把刚加密的密码又明文存了一份。

use std::io::Read;
use std::path::{Path, PathBuf};

use only_js::secret;

use crate::args::{SecretKeygenArgs, SecretOpenArgs, SecretSealArgs};

/// 私钥文件名（`oj secret keygen` 产出）。
pub const PRIVATE_FILE: &str = "secrets-private.pem";
/// 公钥文件名。
pub const PUBLIC_FILE: &str = "secrets-public.pem";

/// `oj secret keygen`：生成密钥对；已存在则拒绝（除非 --force）。
pub fn run_keygen(a: &SecretKeygenArgs) -> Result<(), String> {
    let dir = PathBuf::from(&a.out_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let priv_path = dir.join(PRIVATE_FILE);
    let pub_path = dir.join(PUBLIC_FILE);
    for p in [&priv_path, &pub_path] {
        if p.exists() && !a.force {
            return Err(format!(
                "{} already exists (refusing to overwrite a key that may be in use; pass --force)",
                p.display()
            ));
        }
    }
    let (priv_pem, pub_pem) = secret::keygen(a.bits)?;
    write_pem(&priv_path, &priv_pem)?;
    write_pem(&pub_path, &pub_pem)?;
    println!("wrote {}", priv_path.display());
    println!("wrote {}", pub_path.display());
    println!();
    println!("  {}  留在部署机（chmod 600，别进 git）", PRIVATE_FILE);
    println!("  {}  可随仓库走（只用于加密）", PUBLIC_FILE);
    Ok(())
}

/// `oj secret seal`：明文 → `ENC[...]`。
pub fn run_seal(a: &SecretSealArgs) -> Result<(), String> {
    let pub_path = match &a.key {
        Some(p) => PathBuf::from(p),
        None => {
            let Some(c) = &a.config else {
                return Err("need a public key: pass -k <pub.pem> or -c <config.yaml> \
                            (with secrets.public_key_path)"
                    .into());
            };
            let (cfg_dir, path) = cfg_path_of(c)?;
            PathBuf::from(secrets_key_path(&path, "public_key_path", &cfg_dir)?)
        }
    };
    let key = secret::load_public_key(&pub_path)?;
    let plaintext = input_of(a.value.as_deref(), "plaintext")?;
    println!("{}", secret::seal_pub(&key, &plaintext)?);
    Ok(())
}

/// `oj secret open`：`ENC[...]` → 明文（排障：确认密文与私钥对得上）。
pub fn run_open(a: &SecretOpenArgs) -> Result<(), String> {
    let token = input_of(a.value.as_deref(), "ENC[...]")?;
    // `-k` 直通；否则走与运行时**同一条**私钥通道（env > env file > config 段）——
    // 排障必须和启动用的是同一份真相，否则「open 得出来但 serve 起不来」无从解释。
    let out = match &a.key {
        Some(p) => {
            let pem = std::fs::read_to_string(p).map_err(|e| format!("read {p}: {e}"))?;
            secret::open(&pem, &token)?
        }
        None => {
            let (cfg_dir, path) = match &a.config {
                Some(c) => cfg_path_of(c)?,
                None => (PathBuf::from("."), PathBuf::from("config.yaml")),
            };
            let key = secret::load_private_key(
                secrets_key_path_opt(&path, "private_key_path").as_deref(),
                &cfg_dir,
            )?;
            secret::open_pem(&key, &token)?
        }
    };
    println!("{out}");
    Ok(())
}

/// 明文/密文输入：positional 给了就用（会进 history），否则读 stdin（推荐）。
/// stdin 剥一个尾换行（`echo secret | oj secret seal` 不该把 `\n` 加密进去）。
fn input_of(value: Option<&str>, what: &str) -> Result<String, String> {
    match value {
        Some(v) => Ok(v.to_string()),
        None => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("read {what} from stdin: {e}"))?;
            Ok(strip_trailing_newline(&s))
        }
    }
}

/// 剥掉 stdin 末尾的**一个**换行（`echo secret | oj secret seal` 不该把 `\n` 加密进去）。
/// 先剥 CRLF 再剥 LF——反序会把 `"abc\r\n"` 剥成 `"abc\r"`，把 `\r` 加密进密码
/// （运行期认证失败且极难定位）。
fn strip_trailing_newline(s: &str) -> String {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .map(str::to_string)
        .unwrap_or_else(|| s.to_string())
}

/// `-c` → (config 目录, config 路径)；缺文件报错（不静默回落默认值）。
fn cfg_path_of(config: &str) -> Result<(PathBuf, PathBuf), String> {
    let p = PathBuf::from(config);
    if !p.is_file() {
        return Err(format!("config file not found: {}", p.display()));
    }
    let dir = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    Ok((dir, p))
}

/// 只取 config 里的 `secrets.<key>`（不整份 load_from：那会连带要求解密私钥，
/// 而这里正是要拿公钥去加密，鸡生蛋）。
fn secrets_key_path_opt(config: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config).ok()?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let s = v.get("secrets")?.get(key)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn secrets_key_path(config: &Path, key: &str, cfg_dir: &Path) -> Result<String, String> {
    secrets_key_path_opt(config, key).ok_or_else(|| {
        format!(
            "{}: secrets.{key} not set (or empty); it must point at the PEM file \
             (relative to the config directory {})",
            config.display(),
            cfg_dir.display()
        )
    })
}

fn write_pem(path: &Path, pem: &str) -> Result<(), String> {
    // 私钥以 0o600 **创建**（先写后 chmod 会在宽 umask 下留一个可被同机他账号读的窗口）。
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if path.file_name().is_some_and(|n| n == PRIVATE_FILE) {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| format!("write {}: {e}", path.display()))?;
            return f
                .write_all(pem.as_bytes())
                .map_err(|e| format!("write {}: {e}", path.display()));
        }
    }
    std::fs::write(path, pem).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_writes_keypair_and_refuses_overwrite() {
        let dir = std::env::temp_dir().join(format!("oj-sec-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = SecretKeygenArgs {
            bits: 2048,
            out_dir: dir.display().to_string(),
            force: false,
        };
        run_keygen(&a).unwrap();
        assert!(dir.join(PRIVATE_FILE).is_file() && dir.join(PUBLIC_FILE).is_file());
        // 私钥 600（unix）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.join(PRIVATE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o600);
        }
        // 二次生成拒绝覆盖（防误删在用私钥）。
        assert!(run_keygen(&a).unwrap_err().contains("--force"));
        run_keygen(&SecretKeygenArgs { force: true, ..a }).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// seal → open 端到端（含「config 里取公钥」通道）。
    #[test]
    fn seal_then_open_roundtrip_via_config_paths() {
        let dir = std::env::temp_dir().join(format!("oj-sec-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run_keygen(&SecretKeygenArgs {
            bits: 2048,
            out_dir: dir.display().to_string(),
            force: false,
        })
        .unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!(
                "secrets:\n  public_key_path: {PUBLIC_FILE}\n  private_key_path: {PRIVATE_FILE}\n"
            ),
        )
        .unwrap();
        let cfg = dir.join("config.yaml").display().to_string();
        // seal 走 -c（公钥来自 config 段）。
        let token = secret::seal_pub(
            &secret::load_public_key(&dir.join(PUBLIC_FILE)).unwrap(),
            "hunter2",
        )
        .unwrap();
        assert!(secret::is_sealed(&token));
        // open 走 -c（私钥来自 config 段，与运行时同一通道）。
        let a = SecretOpenArgs {
            key: None,
            config: Some(cfg),
            value: Some(token),
        };
        // 直接断言解密结果（run_open 打到 stdout，这里验的是同一条私钥通道）。
        let (cfg_dir, path) = cfg_path_of(a.config.as_ref().unwrap()).unwrap();
        let key = secret::load_private_key(
            secrets_key_path_opt(&path, "private_key_path").as_deref(),
            &cfg_dir,
        )
        .unwrap();
        assert_eq!(
            secret::open_pem(&key, a.value.as_ref().unwrap()).unwrap(),
            "hunter2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// stdin 的尾换行剥离：CRLF 必须整对剥掉——反序会留下 `\r` 并被加密进密码
    /// （运行期认证失败且极难定位）。只剥一个，不剥多个。
    #[test]
    fn strip_trailing_newline_handles_crlf_and_lf() {
        assert_eq!(strip_trailing_newline("secret"), "secret");
        assert_eq!(strip_trailing_newline("secret\n"), "secret");
        assert_eq!(strip_trailing_newline("secret\r\n"), "secret");
        assert_eq!(strip_trailing_newline("secret\n\n"), "secret\n");
        // 只有 \r 不是行尾，原样保留（数据本身，非换行）。
        assert_eq!(strip_trailing_newline("secret\r"), "secret\r");
    }

    /// 缺公钥（既无 -k 也无 -c）→ 明确报错，不是空手跑。
    #[test]
    fn seal_without_any_key_source_fails_loud() {
        let e = secret::load_public_key(Path::new("no-such.pem")).unwrap_err();
        assert!(e.contains("no-such.pem"));
    }
}
