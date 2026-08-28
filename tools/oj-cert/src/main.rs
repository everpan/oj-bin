//! oj-cert：oj 的 JWS 证书生成/重签工具（tools/ 独立工具，不随 oj 发行）。
//!
//!   oj-cert gen   -o <dir> [--days 365] [--bits 2048] [--nbf <unix>] [--exp <unix>]
//!   oj-cert renew -k <private.pem> [-o <dir>] [--days 365] [--exp <unix>]
//!
//! `--days` 为**有效天数**（exp = now + days*86400），缺省 365 天。

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
        /// 有效天数（exp = now + days*86400；--nbf/--exp 显式值优先）
        #[arg(long, default_value_t = 365)]
        days: u64,
        /// RSA 位数（下限 2048）
        #[arg(long, default_value_t = 2048)]
        bits: u32,
        /// 生效时间（Unix 秒；缺省 now）
        #[arg(long)]
        nbf: Option<u64>,
        /// 过期时间（Unix 秒；缺省 now + days 天）
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
        /// 有效天数（exp = now + days*86400；--exp 显式值优先）
        #[arg(long, default_value_t = 365)]
        days: u64,
        /// 过期时间（Unix 秒；缺省 now + days 天）
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
        } => oj_cert::r#gen(&GenOpts {
            out_dir: out,
            bits,
            nbf: nbf.unwrap_or(now),
            exp: exp.unwrap_or(oj_cert::days_to_expiry(now, days)),
        }),
        Cmd::Renew {
            key,
            out,
            days,
            exp,
        } => oj_cert::renew(&RenewOpts {
            key_path: key,
            out_dir: out,
            // renew 无 --nbf：避免误缩已生效窗口（spec 决策）。
            nbf: now,
            exp: exp.unwrap_or(oj_cert::days_to_expiry(now, days)),
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
