use super::*;
use only_js::bridge::plugin_loader::kv_backend_connect;

/// env 相关测试串行化（同进程并行测试会互踩环境变量）。
/// 使用者均为下方 `#[cfg(unix)]` 用例，Windows 编译下随门避免 dead_code。
#[cfg(unix)]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Tmp(PathBuf);
fn tmpdir(tag: &str) -> Tmp {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    use std::sync::atomic::Ordering;
    let d = std::env::temp_dir().join(format!(
        "oj-sc-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// 隔离插件扫描到空 `<triple>` 目录（同 tests/start_cert_expired_test.rs）：避免
/// workspace / CI 检出的 `bin/plugins` 含 ABI 不符的陈旧产物（如遗留 bus-kafka.dll），
/// 在目标校验前抢先报「plugins scan」错，偏离本测目标。Windows 主机三元组会命中
/// `bin/plugins/<triple>`，故测试须显式把扫描钉到空目录。
fn isolate_plugins(dir: &Path) -> PathBuf {
    let base = dir.join("plugins-isolated");
    std::fs::create_dir_all(base.join(host_triple())).unwrap();
    base
}

/// 证书必配门禁下，测试须自带有效证书：在 `dir`（随测试存活的临时目录）生成
/// 真实签名 JWS，返回配好两路径的 Config。有效期 [now-1h, now+1y] → 启动时 Valid。
fn cert_cfg(dir: &Path) -> Config {
    let mut cfg = Config::default();
    let n = serve::test_support::now_secs();
    serve::test_support::write_cert_into(
        &mut cfg.server,
        dir,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    // 隔离插件扫描（见 isolate_plugins）。
    cfg.plugins_dir = Some(isolate_plugins(dir));
    cfg
}

/// 同 `cert_cfg`，但证书已过期（宽限期 0）→ 启动应被证书门禁拒绝。
fn expired_cert_cfg(dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.server.grace_days = Some(0);
    let n = serve::test_support::now_secs();
    serve::test_support::write_cert_into(&mut cfg.server, dir, n - 2000, n - 1000);
    // 隔离插件扫描（见 isolate_plugins）。
    cfg.plugins_dir = Some(isolate_plugins(dir));
    cfg
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// re-exec 剥旗标：--daemon 精确移除，其余参数（含值）原样保留。
#[test]
fn strip_daemon_flag_drops_only_daemon() {
    use std::ffi::OsString;
    let out = strip_daemon_flag(
        ["serve", "-c", "c.yaml", "--daemon", "--api-path", "src"]
            .into_iter()
            .map(OsString::from),
    );
    assert_eq!(
        out,
        ["serve", "-c", "c.yaml", "--api-path", "src"]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
    );
    // 无 --daemon 时原样透传。
    let out = strip_daemon_flag(["serve", "-c", "c.yaml"].into_iter().map(OsString::from));
    assert_eq!(out.len(), 3);
}

/// v0.1.28 fix：自带 glibc 发行包 daemon 化后必须继续走打包 ld-linux（deploy.sh 启动器
/// 注入的 OJ_BUNDLED_LD/LIB 标记），否则 re-exec 落回系统 ld-linux 崩在低版本宿主。
#[cfg(unix)]
#[test]
fn bundled_reexec_cmd_prefers_bundled_ld() {
    let _g = ENV_LOCK.lock().unwrap();
    let exe = std::path::Path::new("/fake/oj.bin");
    // Safety: 单测独占进程内这两个 oj 私有 env 键，先后成对 remove/set/remove，无并发读写。
    unsafe {
        std::env::remove_var("OJ_BUNDLED_LD");
        std::env::remove_var("OJ_BUNDLED_LIB");
    }
    // 未注入（非打包形态）→ program 即自身。
    let c = super::bundled_reexec_cmd(exe);
    assert_eq!(c.get_program(), exe.as_os_str());

    // Safety: 同上，设值后立即断言并清理。
    unsafe {
        std::env::set_var("OJ_BUNDLED_LD", "/pkg/lib/ld-linux-x86-64.so.2");
        std::env::set_var("OJ_BUNDLED_LIB", "/pkg/lib");
    }
    let c = super::bundled_reexec_cmd(exe);
    let args: Vec<_> = c.get_args().collect();
    assert_eq!(c.get_program(), "/pkg/lib/ld-linux-x86-64.so.2");
    assert_eq!(args, ["--library-path", "/pkg/lib", "/fake/oj.bin"]);
    // Safety: 同上，收尾清理。
    unsafe {
        std::env::remove_var("OJ_BUNDLED_LD");
        std::env::remove_var("OJ_BUNDLED_LIB");
    }
}

/// 打包形态（OJ_BUNDLED_LD 指向 <bin>/lib/ld-linux-*）daemon re-exec 的目标是
/// **随包 ld-linux + <bin>/oj.bin**（direct exec）：进程经「exec 动态加载器」启动，
/// current_exe() 指向 ld-linux，把它当 program 传给 ld.so 会被当 ELF 加载必败；
/// 而走 <bin>/oj 启动器脚本会经 shebang 拉起系统 /bin/sh（+readlink/dirname），
/// 整段中间层是系统 glibc——宿主那段任何异常都让 daemon 子进程静默死亡。直接 exec
/// 后子进程从第一个 exec 起全程随包 glibc。
#[cfg(unix)]
#[test]
fn daemon_reexec_cmd_prefers_bundled_ld_with_ojbin() {
    let _g = ENV_LOCK.lock().unwrap();
    let t = tmpdir("daemon-reexec");
    let bin_dir = t.0.join("bin");
    let ld = bin_dir.join("lib").join("ld-linux-x86-64.so.2");
    std::fs::create_dir_all(ld.parent().unwrap()).unwrap();
    std::fs::write(&ld, b"").unwrap();
    let oj_bin = bin_dir.join("oj.bin");
    std::fs::write(&oj_bin, b"").unwrap();
    let launcher = bin_dir.join("oj");
    std::fs::write(&launcher, b"#!/bin/sh\n").unwrap();
    // Safety: 单测独占进程内 oj 私有 env 键，设值后立即断言并清理（ENV_LOCK 串行化）。
    unsafe {
        std::env::set_var("OJ_BUNDLED_LD", &ld);
        std::env::set_var("OJ_BUNDLED_LIB", bin_dir.join("lib"));
    }
    let c = super::daemon_reexec_cmd().unwrap();
    // Safety: 同上，收尾清理。
    unsafe {
        std::env::remove_var("OJ_BUNDLED_LD");
        std::env::remove_var("OJ_BUNDLED_LIB");
    }
    assert_eq!(c.get_program(), ld.as_os_str());
    let args: Vec<_> = c.get_args().collect();
    assert_eq!(
        args,
        [
            std::ffi::OsStr::new("--library-path"),
            bin_dir.join("lib").as_os_str(),
            oj_bin.as_os_str(),
        ],
        "direct exec：随包 ld + --library-path + oj.bin，不包启动器脚本"
    );
}

/// oj.bin 缺失（异常布局）→ 回退 <bin>/oj 启动器脚本（它自会重放 ld 链），
/// 且不得再被包一层 ld 参数（脚本不是 ELF，ld.so 无法加载）。
#[cfg(unix)]
#[test]
fn daemon_reexec_cmd_falls_back_to_launcher_when_ojbin_missing() {
    let _g = ENV_LOCK.lock().unwrap();
    let t = tmpdir("daemon-reexec-fb");
    let bin_dir = t.0.join("bin");
    let ld = bin_dir.join("lib").join("ld-linux-x86-64.so.2");
    std::fs::create_dir_all(ld.parent().unwrap()).unwrap();
    std::fs::write(&ld, b"").unwrap();
    let launcher = bin_dir.join("oj");
    std::fs::write(&launcher, b"#!/bin/sh\n").unwrap();
    // Safety: 单测独占进程内 oj 私有 env 键（ENV_LOCK 串行化）。
    unsafe {
        std::env::set_var("OJ_BUNDLED_LD", &ld);
        std::env::set_var("OJ_BUNDLED_LIB", bin_dir.join("lib"));
    }
    let c = super::daemon_reexec_cmd().unwrap();
    // Safety: 同上，收尾清理。
    unsafe {
        std::env::remove_var("OJ_BUNDLED_LD");
        std::env::remove_var("OJ_BUNDLED_LIB");
    }
    assert_eq!(c.get_program(), launcher.as_os_str());
    assert!(
        c.get_args().len() == 0,
        "launcher re-exec 不得再包 ld 参数: {:?}",
        c.get_args().collect::<Vec<_>>()
    );
}

/// 就绪管道裁决（进程内管道模拟子进程的四种结局）：
/// `ok` → Ok；EOF 前有字节 → Died(真因)；EOF 无字节（未及回传即死）→ Died("")；
/// 写端开着不写（活着但没就绪）→ 超时 Starting。
#[cfg(unix)]
fn ready_pipe() -> (libc::c_int, libc::c_int) {
    let mut fds = [0 as libc::c_int; 2];
    // Safety: 纯 fd 创建。
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    (fds[0], fds[1])
}

#[cfg(unix)]
#[test]
fn given_ok_when_wait_daemon_ready_then_ready_ok() {
    let (rx, tx) = ready_pipe();
    // Safety: 栈上字面量、长度匹配。
    unsafe {
        libc::write(tx, b"ok".as_ptr().cast(), 2);
        libc::close(tx);
    }
    assert!(matches!(
        super::wait_daemon_ready(rx, std::time::Duration::from_secs(5)),
        super::DaemonReady::Ok
    ));
}

#[cfg(unix)]
#[test]
fn given_error_then_eof_when_wait_daemon_ready_then_died_with_msg() {
    let (rx, tx) = ready_pipe();
    let msg = b"load config: bad yaml";
    // Safety: 栈上字面量、长度匹配。
    unsafe {
        libc::write(tx, msg.as_ptr().cast(), msg.len());
        libc::close(tx);
    }
    match super::wait_daemon_ready(rx, std::time::Duration::from_secs(5)) {
        super::DaemonReady::Died(m) => assert_eq!(m, "load config: bad yaml"),
        other => panic!("expected Died, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn given_silent_death_when_wait_daemon_ready_then_died_empty() {
    let (rx, tx) = ready_pipe();
    // Safety: tx 为刚建出的写端。
    unsafe {
        libc::close(tx);
    } // 子进程未及回传即死：EOF 无字节
    assert!(matches!(
        super::wait_daemon_ready(rx, std::time::Duration::from_secs(5)),
        super::DaemonReady::Died(m) if m.is_empty()
    ));
}

#[cfg(unix)]
#[test]
fn given_no_signal_when_wait_daemon_ready_then_starting() {
    let (rx, tx) = ready_pipe();
    // 写端开着不写 = 子进程还活着但未就绪 → 超时 Starting（不判死）。
    let r = super::wait_daemon_ready(rx, std::time::Duration::from_millis(50));
    // Safety: tx 为刚建出的写端。
    unsafe {
        libc::close(tx);
    }
    assert!(matches!(r, super::DaemonReady::Starting));
}

/// 回归钉：缺省服务目录自 config 同级起步逐级向上搜索（src 优先、dist 次之），
/// 而非 CWD——否则在仓库根（自带核心 crate 的 src/）跑 `-c sample/config.yaml`
/// 会把 src/bridge/ 当业务模块，报 "module 'bridge' missing manifest.yaml"。
#[test]
fn default_dir_searches_upward_from_config_dir() {
    // 同级命中：src 优先于 dist。
    let t = tmpdir("defdir");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    std::fs::create_dir_all(t.0.join("dist")).unwrap();
    std::fs::write(t.0.join("config.yaml"), "{}\n").unwrap();
    let cfg = t.0.join("config.yaml");
    let ac = load_app_config(Some(cfg.to_str().unwrap()), None, None, None, false, None).unwrap();
    assert_eq!(ac.dir, t.0.join("src"));
    assert!(ac.ts);

    // 同级只有 dist → dist。
    let t2 = tmpdir("defdir-dist");
    std::fs::create_dir_all(t2.0.join("dist")).unwrap();
    std::fs::write(t2.0.join("config.yaml"), "{}\n").unwrap();
    let cfg2 = t2.0.join("config.yaml");
    let ac = load_app_config(Some(cfg2.to_str().unwrap()), None, None, None, false, None).unwrap();
    assert_eq!(ac.dir, t2.0.join("dist"));

    // config 下钻一层（sub/config.yaml），src 在父级 → 向上搜索命中。
    let t3 = tmpdir("defdir-up");
    std::fs::create_dir_all(t3.0.join("src")).unwrap();
    std::fs::create_dir_all(t3.0.join("sub")).unwrap();
    std::fs::write(t3.0.join("sub/config.yaml"), "{}\n").unwrap();
    let cfg3 = t3.0.join("sub/config.yaml");
    let ac = load_app_config(Some(cfg3.to_str().unwrap()), None, None, None, false, None).unwrap();
    assert_eq!(ac.dir, t3.0.join("src"));
}

/// search_api_dir 只认「像业务树」的候选：非业务 src（如源码仓自己的 Rust 树，
/// 首层只有无 manifest 的子目录/文件）被跳过继续向上；全程无合格候选 → None
/// （调用方报 service dir not found），不再把 `module 'bridge' missing
/// manifest.yaml` 甩给裸跑 `oj exec` 的用户（2026-10-10 实测陷阱）。
#[test]
fn search_api_dir_requires_plausible_tree() {
    // 非业务 src（首层子目录无 manifest.yaml）→ 跳过；祖先也无合格候选 → None
    // （load_app_config 据此走 `.oj-no-api` 占位，exec 照常跑、test/migrate 明确报错）。
    let t = tmpdir("search-rust");
    std::fs::create_dir_all(t.0.join("src/bridge")).unwrap();
    std::fs::write(t.0.join("src/lib.rs"), "fn main() {}\n").unwrap();
    assert_eq!(search_api_dir(&t.0), None);

    // dev 模块树（首层 */manifest.yaml）→ 命中；同层 src 优先于 dist。
    let t2 = tmpdir("search-dev");
    std::fs::create_dir_all(t2.0.join("src/u")).unwrap();
    std::fs::write(t2.0.join("src/u/manifest.yaml"), "name: u\n").unwrap();
    std::fs::create_dir_all(t2.0.join("dist")).unwrap();
    assert_eq!(search_api_dir(&t2.0), Some(t2.0.join("src")));

    // release 产物锁（manifests.yaml，版本目录的 manifest 在二层）→ 命中。
    let t3 = tmpdir("search-rel");
    std::fs::create_dir_all(t3.0.join("dist/u-0.1.0")).unwrap();
    std::fs::write(t3.0.join("dist/manifests.yaml"), "u: 0.1.0\n").unwrap();
    assert_eq!(search_api_dir(&t3.0), Some(t3.0.join("dist")));

    // 空树（fresh 项目）→ 合法候选；子目录起向上命中合格树。
    let t4 = tmpdir("search-empty");
    std::fs::create_dir_all(t4.0.join("sub")).unwrap();
    std::fs::create_dir_all(t4.0.join("src")).unwrap();
    std::fs::write(t4.0.join("config.yaml"), "{}\n").unwrap();
    assert_eq!(search_api_dir(&t4.0), Some(t4.0.join("src")));
    assert_eq!(search_api_dir(&t4.0.join("sub")), Some(t4.0.join("src")));

    // 全链（回归 v0.1.58 exec e2e 用的形态）：config={}/src=空 → load_app_config
    // 必须解析出该 src（而非占位）。
    std::fs::write(t4.0.join("cfg2.yaml"), "{}\n").unwrap();
    let dir2 = load_app_config(
        Some(t4.0.join("cfg2.yaml").to_str().unwrap()),
        None,
        None,
        None,
        false,
        None,
    )
    .unwrap()
    .dir;
    assert_eq!(dir2, t4.0.join("src"), "空 src 须被全链解析命中");
}

/// cfg 回落链（spec「plugins: 统一语义」）：非空对象透传 → 轴适配器 → {}；
/// 空对象不抢占透传优先级（否则 auth: {} 会切断顶层段 cfg 流）。
#[test]
fn plugin_cfg_fallback_chain() {
    let auth_yaml = "jwt_secret: \"s\"\n";
    let mut cfg = Config {
        auth: Some(serde_yaml::from_str(auth_yaml).unwrap()),
        ..Default::default()
    };
    // 1) 非空对象：原样透传（宿主不改写、不增删字段；键序不判——serde_json 保序）
    cfg.plugins.insert(
        "auth".into(),
        serde_json::json!({ "jwt_secret": "override", "extra_field": 42 }),
    );
    let v: serde_json::Value = serde_json::from_str(
        &plugin_cfg(&cfg, &serde_json::Value::Null, "auth", None, None).unwrap(),
    )
    .unwrap();
    assert_eq!(v["jwt_secret"], "override");
    assert_eq!(v["extra_field"], 42);
    assert_eq!(v.as_object().map(|o| o.len()), Some(2));
    // 2) 无条目 → 轴适配器（auth 分支既有行为不变）；未知插件 → {}
    let mut cfg2 = Config {
        auth: Some(serde_yaml::from_str(auth_yaml).unwrap()),
        ..Default::default()
    };
    let v: serde_json::Value = serde_json::from_str(
        &plugin_cfg(&cfg2, &serde_json::Value::Null, "auth", None, None).unwrap(),
    )
    .unwrap();
    assert_eq!(v["jwt_secret"], "s");
    assert_eq!(
        plugin_cfg(&cfg2, &serde_json::Value::Null, "vendor-thing", None, None),
        Ok("{}".to_string())
    );
    // 3) 空对象：跳过透传 → 仍轴适配器
    cfg2.plugins.insert("auth".into(), serde_json::json!({}));
    let v: serde_json::Value = serde_json::from_str(
        &plugin_cfg(&cfg2, &serde_json::Value::Null, "auth", None, None).unwrap(),
    )
    .unwrap();
    assert_eq!(v["jwt_secret"], "s");
}

/// `es` 段多源选择（v0.1.34）：`--es <profile>` 解析对应命名 profile；未声明即 fail-fast。
#[test]
fn plugin_cfg_es_profile_selection() {
    let cfg = Config {
        es: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "default".into(),
                config::EsCfg {
                    endpoint: "http://es-default:9200".into(),
                },
            );
            m.insert(
                "archive".into(),
                config::EsCfg {
                    endpoint: "http://es-archive:9200".into(),
                },
            );
            m
        },
        ..Default::default()
    };
    // 缺省（None）→ 取 default profile。
    let v: serde_json::Value = serde_json::from_str(
        &plugin_cfg(&cfg, &serde_json::Value::Null, "es", None, None).unwrap(),
    )
    .unwrap();
    assert_eq!(v["endpoint"], "http://es-default:9200");
    // 显式选中 archive profile。
    let v: serde_json::Value = serde_json::from_str(
        &plugin_cfg(&cfg, &serde_json::Value::Null, "es", None, Some("archive")).unwrap(),
    )
    .unwrap();
    assert_eq!(v["endpoint"], "http://es-archive:9200");
    // 未声明的 profile → fail-fast，且不静默回落 default。
    let e = plugin_cfg(&cfg, &serde_json::Value::Null, "es", None, Some("missing"));
    assert!(e.is_err(), "missing es profile must fail-fast, got {e:?}");
    assert!(
        e.unwrap_err().contains("not declared"),
        "error should name the missing profile"
    );
}

/// mail 轴 cfg 走**适配器臂**（顶层 `smtp:` 段序列化），**不**要求用户用
/// `plugins:` 透传——`plugins:` 非空即切严格清单模式，用户会被迫列全所有插件。
/// 过线 JSON 与宿主的校验面同源（`MailConfig::from_value` 吃同一份）。
#[test]
fn plugin_cfg_mail_serializes_top_level_smtp_section() {
    // 段缺省 → 空 cfg（插件 init 得到零 profile；宿主不挂后端）。
    let mut cfg = Config::default();
    assert_eq!(
        plugin_cfg(&cfg, &serde_json::Value::Null, "mail", None, None),
        Ok("{}".to_string())
    );
    // 顶层 smtp: 段 → 插件 cfg（并发参数 + 每 profile 一个键 + 凭据 + 白名单）。
    cfg.smtp = Some(
        serde_yaml::from_str(
            "workers: 2\nqueue_capacity: 8\nmock:\n  host: localhost\n  port: 25\n  \
                 tls: none\n  allow_none_tls: true\n  mechanism: login\n  \
                 file_transport: /tmp/eml\n  allowed_from: [noreply@x.com]\n  \
                 allowed_recipients: [\"@x.com\"]\n",
        )
        .unwrap(),
    );
    let raw = plugin_cfg(&cfg, &serde_json::Value::Null, "mail", None, None).unwrap();
    assert_ne!(raw, "{}");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["workers"], 2);
    assert_eq!(v["queue_capacity"], 8);
    assert_eq!(v["mock"]["file_transport"], "/tmp/eml");
    assert_eq!(v["mock"]["tls"], "none");
    assert_eq!(v["mock"]["allow_none_tls"], true);
    assert_eq!(v["mock"]["allowed_from"][0], "noreply@x.com");
    assert_eq!(v["mock"]["allowed_recipients"][0], "@x.com");
    // 空字段省略（`null` 会让插件强类型反序列化报错）。
    assert!(v["mock"].get("user").is_none(), "{v}");
    // 非空对象透传优先（统一语义；宿主与插件仍吃同一份 → 不产生两边分叉）。
    cfg.plugins.insert(
        "mail".into(),
        serde_json::json!({ "mock": { "host": "override" } }),
    );
    let v: serde_json::Value = serde_json::from_str(
        &plugin_cfg(&cfg, &serde_json::Value::Null, "mail", None, None).unwrap(),
    )
    .unwrap();
    assert_eq!(v["mock"]["host"], "override");
    assert!(v.get("workers").is_none(), "{v}");
}

/// 适配器轴必须是宿主探测轴的子集（两表失步 = 适配器永远打空）。
/// TYPED_AXES 已降级 pub(crate)，此处经公共 `Registrations::provides` 判定「宿主认识该轴」。
#[test]
fn cfg_adapters_subset_of_probed_axes() {
    let r = only_js::bridge::plugin_loader::Registrations::default();
    for name in ADAPTER_AXES {
        assert!(r.provides(name).is_some(), "{name}");
    }
}

#[test]
fn plugin_cfg_config_key_reads_top_level_section() {
    let cfg = Config::default();
    let top = serde_json::json!({ "cache": { "ttl": 60 }, "plugins": {} });
    let s = plugin_cfg(&cfg, &top, "oj-cache", Some("cache"), None).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&s).unwrap()["ttl"],
        60
    );
}

#[test]
fn plugin_cfg_passthrough_still_wins() {
    let mut cfg = Config::default();
    cfg.plugins
        .insert("oj-cache".into(), serde_json::json!({ "ttl": 1 }));
    let top = serde_json::json!({ "cache": { "ttl": 60 } });
    let s = plugin_cfg(&cfg, &top, "oj-cache", Some("cache"), None).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&s).unwrap()["ttl"],
        1
    );
}

#[test]
fn plugin_cfg_legacy_es_arm_unchanged() {
    // 无 config_key → 走遗留臂（既有 es/auth/mail/ldap 测试族保持绿即可，此处只钉回退）：
    let cfg = Config::default();
    let top = serde_json::json!({});
    let s = plugin_cfg(&cfg, &top, "es", None, None).unwrap();
    assert_eq!(s, "{}");
}

/// 装配后未消费段诊断：撞名（config key 命中）/ 透传（plugins:<name> 非空）/ 正常未消费。
#[test]
fn unconsumed_sections_covers_collision_passthrough_and_normal() {
    fn loaded(name: &str, key: Option<&str>) -> LoadedPlugin {
        LoadedPlugin {
            descriptor: oj_plugin_ffi::PluginDescriptor {
                name: name.into(),
                semver: "0.1.0".into(),
                abi_version: oj_plugin_ffi::ABI_VERSION,
                fingerprint: "".into(),
                desc: "".into(),
            },
            registrations: Default::default(),
            generic_axes: Vec::new(),
            config_key: key.map(Into::into),
            caps: None,
        }
    }
    let cfg = Config::default();
    // top 为全量 mapping（含宿主已知段 "redis"）：已知段恒不算 unconsumed。
    let top = serde_json::json!({
        "cache": { "ttl": 60 },
        "vendor": { "url": "x" },
        "lonely": { "a": 1 },
        "redis": { "default": "redis://h:6379" },
    });
    // 撞名：插件自报 config key = "cache" → 段被消费，不告警（已知段 "redis" 也不列）。
    let loaded_v = vec![loaded("oj-cache", Some("cache"))];
    assert_eq!(
        unconsumed_sections(&top, &cfg, &loaded_v),
        ["vendor", "lonely"]
    );
    // 透传：plugins:<name> 非空 → 同名段被消费（plugins 本身是已知键）。
    let mut cfg2 = Config::default();
    cfg2.plugins
        .insert("vendor".into(), serde_json::json!({ "url": "y" }));
    let loaded_v = vec![loaded("vendor", None)];
    assert_eq!(
        unconsumed_sections(&top, &cfg2, &loaded_v),
        ["cache", "lonely"]
    );
    // 正常：无任何消费方 → 未知段全列出，宿主已知段（redis）仍不列。
    let loaded_v: Vec<LoadedPlugin> = Vec::new();
    assert_eq!(
        unconsumed_sections(&top, &cfg, &loaded_v),
        ["cache", "vendor", "lonely"]
    );
}

/// T13：启动横幅每插件一行，db 轴插件追加 caps 串，非 db 轴插件不追加。
#[test]
fn banner_line_shows_abi_fingerprint_and_db_caps() {
    use only_js::bridge::dialect_caps::{DialectCaps, DialectInfo, IdStrategy, PlaceholderStyle};

    fn plugin(name: &str, caps: Option<DialectInfo>) -> LoadedPlugin {
        LoadedPlugin {
            descriptor: oj_plugin_ffi::PluginDescriptor {
                name: name.into(),
                semver: "0.1.0".into(),
                abi_version: oj_plugin_ffi::ABI_VERSION,
                fingerprint: "fp-1".into(),
                desc: "".into(),
            },
            registrations: Default::default(),
            generic_axes: Vec::new(),
            config_key: None,
            caps,
        }
    }

    let db = plugin(
        "oj-db-postgres",
        Some(DialectInfo {
            name: "postgres".into(),
            caps: DialectCaps {
                placeholder: PlaceholderStyle::Dollar,
                id: IdStrategy::Returning,
                stream: true,
                cancel: true,
            },
        }),
    );
    let line = plugin_banner_line(&db);
    assert!(line.contains("oj-db-postgres"), "{line}");
    assert!(line.contains("fp=fp-1"), "{line}");
    assert!(
        line.contains("caps: placeholder=$ id=returning stream cancel"),
        "{line}"
    );

    // 非 db 轴插件：无 caps 段。
    let kv = plugin("oj-kv-redis", None);
    let line = plugin_banner_line(&kv);
    assert!(!line.contains("caps:"), "{line}");
}

/// spec Part 3：config key 查找面是全量顶层 Value——撞宿主已知段名（config: "redis"）
/// 是合法场景，必须取到段内容而不是静默 "{}"（评审 H2）。
#[test]
fn plugin_cfg_config_key_reads_host_known_section() {
    let cfg = Config::default();
    let top = serde_json::json!({ "redis": { "default": "redis://h:6379" } });
    let s = plugin_cfg(&cfg, &top, "oj-kv-redis", Some("redis"), None).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&s).unwrap()["default"],
        "redis://h:6379"
    );
}

/// A5：mail 双配置源闸门 —— `smtp:` 与非空 `plugins.mail` **皆非空** ⇒ 装配期 Err
/// （`plugin_cfg` 会让透传静默胜出，运维改 `smtp:` 会「改了不生效」）。
/// 其余四种组合都合法（含 `plugins: {mail: {}}` 空对象 = 回落适配器；`smtp: {}` 空段 = 未配置）。
#[test]
fn given_both_mail_cfg_sources_nonempty_when_check_then_errors() {
    use only_js::config::SmtpSection;

    /// 一个 profile（实质内容）的 `smtp:` 段。
    fn profiles() -> SmtpSection {
        serde_yaml::from_str("default:\n  host: h\n  allowed_from: [a@x.com]\n").unwrap()
    }
    /// 非空 `plugins.mail`（透传形态）。
    fn passthrough() -> serde_json::Value {
        serde_json::json!({ "default": { "host": "override" } })
    }
    /// 组装一个 Config；`None` = 该来源缺省。
    fn case(plugins_mail: Option<serde_json::Value>, smtp: Option<SmtpSection>) -> Config {
        let mut cfg = Config {
            smtp,
            ..Default::default()
        };
        if let Some(v) = plugins_mail {
            cfg.plugins.insert("mail".into(), v);
        }
        cfg
    }

    // 皆非空 → Err，文案须点名两条来源与「二选一」。
    let e = check_mail_cfg_sources(&case(Some(passthrough()), Some(profiles())))
        .expect_err("双源皆非空必须 Err");
    assert!(
        e.contains("smtp:") && e.contains("plugins.mail") && e.contains("pick one"),
        "文案须点明两条来源与二选一：{e}"
    );
    // 仅 `workers`（无 profile）也算「非空段」——它就是被静默忽略的那部分配置。
    let workers_only: SmtpSection = serde_yaml::from_str("workers: 4\n").unwrap();
    assert!(check_mail_cfg_sources(&case(Some(passthrough()), Some(workers_only))).is_err());

    // 合法的四种组合：
    // 1) 只有 `smtp:`（标准形态）。
    assert!(check_mail_cfg_sources(&case(None, Some(profiles()))).is_ok());
    // 2) 只有 `plugins.mail`（透传）。
    assert!(check_mail_cfg_sources(&case(Some(passthrough()), None)).is_ok());
    // 3) `smtp:` 非空 + `plugins: {mail: {}}`（空对象 = 回落适配器，非透传；e2e 夹具即此形态）。
    assert!(check_mail_cfg_sources(&case(Some(serde_json::json!({})), Some(profiles()))).is_ok());
    // 4) `plugins.mail` 非空 + `smtp: {}`（空段 = 未配置，不算冲突）。
    assert!(
        check_mail_cfg_sources(&case(Some(passthrough()), Some(SmtpSection::default()))).is_ok()
    );
}

/// A5-ldap：ldap 双配置源闸门（与 mail 同构）；并钉住报错文案——曾漏 `\` 续行，
/// 多行字面量被拍平成一行夹一长串空格。
#[test]
fn given_both_ldap_cfg_sources_nonempty_when_check_then_errors_with_clean_message() {
    fn section() -> serde_yaml::Value {
        serde_yaml::from_str("default:\n  url: ldap://h:389\n").unwrap()
    }
    fn case(plugins_ldap: Option<serde_json::Value>, ldap: Option<serde_yaml::Value>) -> Config {
        let mut cfg = Config {
            ldap,
            ..Default::default()
        };
        if let Some(v) = plugins_ldap {
            cfg.plugins.insert("ldap".into(), v);
        }
        cfg
    }

    // 皆非空 → Err，文案点名两条来源与「二选一」，且不夹拼接空格。
    let e = check_ldap_cfg_sources(&case(
        Some(serde_json::json!({ "default": { "url": "override" } })),
        Some(section()),
    ))
    .expect_err("双源皆非空必须 Err");
    assert!(
        e.contains("ldap:") && e.contains("plugins.ldap") && e.contains("pick one"),
        "文案须点明两条来源与二选一：{e}"
    );
    assert!(
        !e.contains("  "),
        "文案不得含连续空格（漏 \\ 续行症状）：{e:?}"
    );
    // 单一来源（只有 `ldap:` 段）合法。
    assert!(check_ldap_cfg_sources(&case(None, Some(section()))).is_ok());
}

#[test]
fn config_dir_never_empty() {
    // bare 文件名回落当前目录（project_root 钳制不因空路径失效）。
    assert_eq!(config_dir_of(Path::new("config.yaml")), PathBuf::from("."));
    assert_eq!(
        config_dir_of(Path::new("./config.yaml")),
        PathBuf::from(".")
    );
    assert_eq!(
        config_dir_of(Path::new("sub/config.yaml")),
        PathBuf::from("sub")
    );
    assert_eq!(
        config_dir_of(Path::new("/abs/config.yaml")),
        PathBuf::from("/abs")
    );
}

#[test]
fn cli_app_path_resolves_against_cwd() {
    // CLI --app-path：相对路径按 CWD（base）拼接；绝对路径原样透传。
    // 以 Path 语义断言——Windows 下 join 产出 `\` 分隔符，字符串比较会误报。
    let base = Path::new("/repo/root");
    assert_eq!(
        PathBuf::from(absolutize_cwd(base, "dist")),
        base.join("dist")
    );
    // join 不做归一化，`..` 原样保留（由 resolve_static_root 的 canonicalize 收敛）。
    assert_eq!(
        PathBuf::from(absolutize_cwd(base, "../site")),
        base.join("../site")
    );
    assert_eq!(
        PathBuf::from(absolutize_cwd(base, "/abs/dist")),
        PathBuf::from("/abs/dist")
    );
}

/// U-18 批 T1.4：route_timeouts 悬空 pattern → 真装配拒启（文案点名 pattern 与路由数）。
/// 反向：命中已注册路由的 pattern 正常起服务。
#[tokio::test]
async fn route_timeouts_dangling_pattern_rejects_boot() {
    let t = tmpdir("rt-dangling");
    let src = t.0.join("src");
    std::fs::create_dir_all(src.join("m")).unwrap();
    std::fs::write(
        src.join("m/manifest.yaml"),
        "name: m\ndesc: rt-test\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        src.join("m/api.ts"),
        "export default { get() { return json.ok(1); } };",
    )
    .unwrap();
    let mk = |pattern: &str| {
        let mut cfg = cert_cfg(&t.0);
        cfg.server.port = 0;
        cfg.db.insert("default".into(), "sqlite://:memory:".into());
        cfg.server.route_timeouts.push(config::RouteTimeoutConf {
            pattern: pattern.into(),
            timeout: "1s".into(),
        });
        cfg
    };
    let e = start(mk("/v1/api/ghost"), &t.0, src.clone(), "/v1/api".into())
        .await
        .unwrap_err();
    assert!(e.contains("/v1/api/ghost"), "{e}");
    assert!(e.contains("matches none of"), "{e}");
    // 反向：命中真实路由 → 起服务成功（句柄立即关闭）。
    let (_addr, h) = start(mk("/v1/api/m"), &t.0, src, "/v1/api".into())
        .await
        .unwrap();
    h.abort();
}

#[test]
fn admission_gate_mounts() {
    // 规则 5：mounts（config + CLI 折叠后）空 → Err；任一挂载 → Ok。
    // 目录存在性移交装配期 resolve_mounts（fail-fast，含 prefix/条目序号）。
    let e = admission_gate(&Config::default()).unwrap_err();
    assert!(e.contains("mounts"), "{e}");
    let mut c = Config::default();
    c.mounts.push(config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some("web".into()),
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    assert!(admission_gate(&c).is_ok());
}

#[test]
fn fold_cli_app_paths_rules() {
    // 裸 dir：upsert `/` web 挂载；至多一次。
    let mut c = Config::default();
    fold_cli_app_paths(&mut c, &["web".into()]).unwrap();
    assert_eq!(c.mounts.len(), 1);
    assert_eq!(c.mounts[0].prefix, "/");
    assert!(c.mounts[0].web.as_deref().unwrap().ends_with("web"));
    assert!(fold_cli_app_paths(&mut c, &["a".into(), "b".into()]).is_err());
    // prefix=dir：追加 + 同前缀 web 条目替换（规范化口径：config 带尾斜杠也命中；
    // headers/spa 保留）。
    let mut c = Config::default();
    c.mounts.push(config::MountConf {
        prefix: "/docs/".into(),
        api: None,
        web: Some("old".into()),
        spa: Some(true),
        headers: Default::default(),

        ..Default::default()
    });
    fold_cli_app_paths(&mut c, &["/docs=new".into(), "/app=dist/app".into()]).unwrap();
    assert_eq!(c.mounts.len(), 2);
    let docs = c.mounts.iter().find(|m| m.prefix == "/docs").unwrap();
    assert!(docs.web.as_deref().unwrap().ends_with("new"), "{docs:?}");
    assert_eq!(docs.spa, Some(true), "CLI 只覆盖目录，spa 保留");
    let app = c.mounts.iter().find(|m| m.prefix == "/app").unwrap();
    assert!(app.web.as_deref().unwrap().ends_with("dist/app"), "{app:?}");
    // 裸 + 带前缀混合；裸覆盖 config 的 `/` web 条目。
    let mut c = Config::default();
    c.mounts.push(config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some("old-root".into()),
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    fold_cli_app_paths(&mut c, &["web".into(), "/x=dx".into()]).unwrap();
    assert_eq!(c.mounts.len(), 2);
    assert!(c.mounts[0].web.as_deref().unwrap().ends_with("web"));
    // 同前缀 api 挂载不受 --app-path 影响（配对语义：追加而非替换）。
    let mut c = Config::default();
    c.mounts.push(config::MountConf {
        prefix: "/docs".into(),
        api: Some("src".into()),
        web: None,
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    fold_cli_app_paths(&mut c, &["/docs=d".into()]).unwrap();
    assert_eq!(c.mounts.len(), 2);
    assert!(c.mounts[0].api.is_some() && c.mounts[1].web.is_some());
    // 非法 prefix 报错。
    assert!(fold_cli_app_paths(&mut Config::default(), &["docs=dx".into()]).is_err());
}

#[test]
fn fold_cli_mounts_rules() {
    // --api-path upsert {prefix: B, api: d}：B = -b > 默认 /v1/api；同前缀 api 替换目录。
    let mut c = Config::default();
    c.mounts.push(config::MountConf {
        prefix: "/v1/api".into(),
        api: Some("dist".into()),
        web: None,
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    let a = ServeArgs {
        api_path: Some("src".into()),
        base: Some("/cli".into()),
        ..Default::default()
    };
    fold_cli_mounts(&mut c, &a).unwrap();
    // CLI 前缀 /cli ≠ config 的 /v1/api → 追加（挂载自带 prefix，无全局 base 可覆盖）。
    assert_eq!(c.mounts.len(), 2);
    assert_eq!(c.mounts[1].prefix, "/cli");
    assert!(c.mounts[1].api.as_deref().unwrap().ends_with("src"));
    // 同前缀 → 替换目录（CLI 优先）。
    let a = ServeArgs {
        api_path: Some("src".into()),
        base: Some("/cli".into()),
        ..Default::default()
    };
    fold_cli_mounts(&mut c, &a).unwrap();
    assert_eq!(c.mounts.len(), 2);
    assert!(c.mounts[1].api.as_deref().unwrap().ends_with("src"));
    // -b 无 --api-path → Err（裸 -b 无处附着）。
    let a = ServeArgs {
        base: Some("/x".into()),
        ..Default::default()
    };
    let e = fold_cli_mounts(&mut Config::default(), &a).unwrap_err();
    assert!(e.contains("--base"), "{e}");
    // 缺省 prefix = DEFAULT_API_PREFIX（零 URL 变化）。
    let mut c = Config::default();
    fold_cli_mounts(
        &mut c,
        &ServeArgs {
            api_path: Some("src".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(c.mounts[0].prefix, "/v1/api");
}

#[test]
fn app_prefix_normalized() {
    // 归一：尾斜杠剪除；剪完为空 → "/"；不以 / 开头 → Err。
    assert_eq!(resolve_app_prefix("/").unwrap(), "/");
    assert_eq!(resolve_app_prefix("/site").unwrap(), "/site");
    assert_eq!(resolve_app_prefix("/site/").unwrap(), "/site");
    assert_eq!(resolve_app_prefix("/a/b/").unwrap(), "/a/b");
    let e = resolve_app_prefix("site").unwrap_err();
    assert!(e.contains("must start with"), "{e}");
}

/// 证书必配：未配置证书路径 → 启动 fail-fast（任何方式都无法绕过）；配齐有效证书 →
/// 正常启动；证书过期 → 启动拒绝。
#[tokio::test]
async fn certificate_mandatory_gate() {
    let t = tmpdir("sc-cert");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    // a) 未配置任何证书路径 → 拒绝启动。
    let e = start(Config::default(), &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(
        e.contains("certificate is mandatory") && e.contains("public_key_path"),
        "{e}"
    );
    // b) 只配一个路径 → 仍拒绝（缺任一不算就绪）。
    let mut cfg = Config::default();
    cfg.server.public_key_path = "/nonexistent/key.pem".into();
    let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("certificate is mandatory"), "{e}");
    // c) 配齐有效证书 → 启动成功（随机端口）。
    let mut cfg = cert_cfg(&t.0);
    cfg.server.host = "127.0.0.1".into(); // 沙箱环境 localhost 解析受限，绑定回环更稳
    cfg.server.port = 0;
    let r = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .unwrap();
    assert!(r.0.port() != 0);
}

/// 证书过期且宽限期 0 → 启动期硬断言中止（oj/src/app.rs::load_cert_with_watcher 的
/// assert!），不再返回可被静默的 Err。panic 载荷含 "certificate"，故用 should_panic。
/// 原 certificate_mandatory_gate 的 (d) 步骤拆出独立：该用例 a/b/c 须正常返回，不能与
/// 本测的 panic 混在同一测试函数里。
#[tokio::test]
#[should_panic(expected = "certificate")]
async fn expired_cert_aborts_startup() {
    let t = tmpdir("sc-cert-exp");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    // 触发 load_cert_with_watcher 的 assert!（过期且宽限结束）→ panic 经 .await 上抛，
    // 由 #[should_panic] 捕获。返回值不会到达，故忽略。
    let _ = start(
        expired_cert_cfg(&t.0),
        &t.0,
        t.0.join("src"),
        "/v1/api".into(),
    )
    .await;
}

/// auth 配了但 jwt_secret 空 → 装配 fail-fast（不静默跳过鉴权）。
/// 插件目录隔离到临时夹具（就地编译 oj-auth）——不得依赖 workspace 的 bin/ 产物，
/// 否则 CI 检出（无 bin/plugins）会先撞「no auth plugin loaded」而非本测的目标错误。
#[tokio::test]
async fn empty_jwt_secret_fails_fast() {
    let t = tmpdir("sc-jwt");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(auth_plugin_artifact(), pdir.join(plugin_file("auth"))).unwrap();
    let mut cfg = cert_cfg(&t.0);
    cfg.plugins_dir = Some(t.0.clone());
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"\"\n").unwrap());
    let e = start(cfg, Path::new("/tmp"), t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("jwt_secret"), "{e}");
}

#[test]
fn mode_detected_by_lock_file() {
    let t = tmpdir("sc-mode");
    // 空目录 / 模块树（只有 manifest.yaml）→ dev
    assert!(!is_release(&t.0));
    std::fs::create_dir_all(t.0.join("user")).unwrap();
    std::fs::write(t.0.join("user/manifest.yaml"), "name: user\n").unwrap();
    assert!(!is_release(&t.0));
    // 构建锁存在 → release
    std::fs::write(t.0.join("manifests.yaml"), "user: 0.1.0\n").unwrap();
    assert!(is_release(&t.0));
}

#[tokio::test]
async fn rejects_unknown_dsn_scheme() {
    let t = tmpdir("sc-dsn2");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    let mut cfg = cert_cfg(&t.0);
    cfg.db
        .insert("default".into(), "oracle://u:p@localhost/test".into());
    let e = start(cfg, Path::new("/tmp"), t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("scheme"), "{e}");
}

#[tokio::test]
async fn registry_connect_dispatches_by_scheme() {
    let t = tmpdir("sc-dsn");
    let reg = only_js::bridge::DbBackendRegistry::builtin();
    // sqlite：相对路径归一为 config_dir 下绝对路径并建空库
    reg.connect("sqlite://db.sqlite", &t.0)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(t.0.join("db.sqlite").is_file());
    reg.connect("sqlite::memory:", &t.0)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    reg.connect("memory://m", &t.0)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    // Task 4.1：mysql/postgres 已迁插件，内置不再认领 → 缺装时明确 unknown db scheme
    // （快速失败不触网——原测试真连 127.0.0.1:1 每个 ~30s 超时，本版消除）。
    for unclaimed in ["mysql://u:p@127.0.0.1:1/app", "postgres://127.0.0.1:1/app"] {
        let e = reg
            .connect(unclaimed, &t.0)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("unknown db scheme"), "{e}");
    }
    // 未知 scheme 拒绝
    assert!(reg.connect("oracle://x", &t.0).await.is_err());
}

/// 下载路由数据源 = registry.default()：同 key 不同内容时字节与 default 一致（spec §2 裁决回归）。
#[tokio::test]
async fn download_route_source_is_default_backend_only() {
    let t = tmpdir("sc-blob-def");
    let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
    let r = assemble_blobs(&section, &t.0, "/v1/api", None, None)
        .await
        .unwrap();
    r.default().unwrap().put("k", b"DEF", None).await.unwrap();
    r.get("img").unwrap().put("k", b"IMG", None).await.unwrap();
    match r.default().unwrap().serve("k").await.unwrap() {
        only_js::bridge::BlobServed::Bytes(bytes, _) => assert_eq!(bytes, b"DEF"),
        _ => panic!("local must inline-serve"),
    }
}

/// `--blob <profile>`（v0.1.34）：把命名 profile 烘焙为字面 "default" 的解析目标。
#[tokio::test]
async fn assemble_blobs_profile_selects_default_alias() {
    let t = tmpdir("sc-blob-alias");
    let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
    // 选中 img → 字面 default 现在指向 img 后端。
    let r = assemble_blobs(&section, &t.0, "/v1/api", None, Some("img"))
        .await
        .unwrap();
    r.default().unwrap().put("k", b"IMG", None).await.unwrap();
    match r.default().unwrap().serve("k").await.unwrap() {
        only_js::bridge::BlobServed::Bytes(bytes, _) => assert_eq!(bytes, b"IMG"),
        _ => panic!("default must now resolve to the img backend"),
    }
    // 同时 img 仍以原名可取。
    assert!(r.get("img").is_some());
}

/// `--blob <missing>`（v0.1.34）：选错 profile 直接 fail-fast，不静默回落 default。
#[tokio::test]
async fn assemble_blobs_missing_profile_fails_fast() {
    let t = tmpdir("sc-blob-miss");
    let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
    let e = assemble_blobs(&section, &t.0, "/v1/api", None, Some("ghost"))
        .await
        .err()
        .unwrap_or_default();
    assert!(
        e.contains("blob profile 'ghost' not declared"),
        "expected fail-fast on missing profile, got: {e}"
    );
}

#[tokio::test]
async fn assemble_blobs_multi_local_and_unknown_driver() {
    let t = tmpdir("sc-blob");
    let section: config::BlobSection =
            serde_yaml::from_str("backends:\n  default:\n    driver: local\n    root: a\n  img:\n    driver: local\n    root: b\n")
                .unwrap();
    let r = assemble_blobs(&section, &t.0, "/v1/api", None, None)
        .await
        .unwrap();
    assert!(r.default().is_some() && r.get("img").is_some());
    // local root 相对 config_dir 绝对化并创建
    assert!(t.0.join("a").is_dir() && t.0.join("b").is_dir());
    // 未知 driver：错误带后端名
    let bad: config::BlobSection =
        serde_yaml::from_str("backends:\n  x:\n    driver: ghost\n").unwrap();
    let e = assemble_blobs(&bad, &t.0, "/v1/api", None, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("'x'") && e.contains("ghost"), "{e}");
}

/// s3 驱动但无 blob 插件 → fail fast（driver != local 且无插件闸门，Task 4.2）。
#[tokio::test]
async fn s3_without_blob_plugin_fails_fast() {
    let t = tmpdir("sc-blob-s3");
    let section: config::BlobSection = serde_yaml::from_str(
        "backends:\n  default:\n    driver: s3\n    bucket: b\n    region: r\n",
    )
    .unwrap();
    let e = assemble_blobs(&section, &t.0, "/v1/api", None, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("requires the oj-blob-s3 plugin"), "{e}");
}

#[tokio::test]
async fn connect_dbs_two_engines_and_unknown_named() {
    let t = tmpdir("sc-cdb");
    let reg = only_js::bridge::DbBackendRegistry::builtin();
    let mut m = HashMap::new();
    m.insert("default".to_string(), "sqlite::memory:".to_string());
    m.insert("aux".to_string(), "memory://aux".to_string());
    let dbs = connect_dbs(&m, &reg, &t.0).await.unwrap();
    assert!(dbs.contains_key("default") && dbs.contains_key("aux"));
    // 未知 scheme：库名出现在错误里
    let mut bad = HashMap::new();
    bad.insert("mydb".to_string(), "oracle://x".to_string());
    let e = connect_dbs(&bad, &reg, &t.0)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("mydb"), "{e}");
    assert!(e.contains("unknown db scheme"), "{e}");
}

#[tokio::test]
async fn manifest_mismatch_blocks_startup() {
    let t = tmpdir("sc-md");
    std::fs::create_dir_all(t.0.join("src/user")).unwrap();
    std::fs::write(
        t.0.join("src/user/manifest.yaml"),
        "name: x\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    let e = start(cert_cfg(&t.0), &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("name"), "{e}");
}

/// 夹具：手摆 release dist（file 名任意合法即可）。
fn rel_fixture(files: &[(&str, &str)]) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    use std::sync::atomic::Ordering;
    let t = std::env::temp_dir().join(format!(
        "oj-sc-rel-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    for (rel, c) in files {
        let p = t.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }
    t
}

const MANI: &str = "name: user\ndesc: d\nversion: 0.1.0\n";

#[tokio::test]
async fn release_aggregates_modules_via_lock() {
    let t = rel_fixture(&[
        ("dist/manifests.yaml", "user: 0.1.0\n"),
        ("dist/user-0.1.0/manifest.yaml", MANI),
        (
            "dist/user-0.1.0/routes.js",
            "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-x.js\" } ];\n",
        ),
        (
            "dist/user-0.1.0/item/api-x.js",
            "export default { get() { json.ok({ v: 1 }); } };\n",
        ),
    ]);
    let mut cfg = cert_cfg(&t);
    cfg.server.port = 0; // 随机端口（默认 778 并行测试会撞）
    let (addr, _h) = start(cfg, &t, t.join("dist"), "/v1/api".into())
        .await
        .unwrap();
    let r = reqwest::get(format!("http://{addr}/v1/api/user/item/7"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200); // pattern 无 base → 聚合拼 /v1/api/user/item/{id}
}

/// P1 迁移门禁（§4.6）：dev 默认 auto（启动即 apply）；release 默认 verify
/// （账本落后 M004 拒启 + 指引命令；apply 后放行）；off 逃生门；非法值 fail-fast。
#[tokio::test]
async fn migrate_gate_auto_verify_off() {
    let t = tmpdir("sc-gate");
    std::fs::create_dir_all(t.0.join("src/u/migrations")).unwrap();
    std::fs::write(
        t.0.join("src/u/manifest.yaml"),
        "name: u\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.0.join("src/u/migrations/0001__init.sql"),
        "CREATE TABLE g (x);",
    )
    .unwrap();
    let db = format!("sqlite://{}/db.sqlite", t.0.display());
    // a) dev auto：启动即迁移（表已建）。
    let mut cfg = cert_cfg(&t.0);
    cfg.server.port = 0;
    cfg.db.insert("default".into(), db.clone());
    let _ = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let acc = only_js::bridge::DbBackendRegistry::builtin()
        .connect(&db, &t.0)
        .await
        .unwrap();
    let rows = acc
        .query("select name from sqlite_master where type='table' and name='g'")
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "dev auto gate must apply migrations");
    // b) release verify：空库 + 产物带迁移 → M004 拒启（文案含命令）。
    const UMANI: &str = "name: u\ndesc: d\nversion: 0.1.0\n";
    let dist = rel_fixture(&[
        ("dist/manifests.yaml", "u: 0.1.0\n"),
        ("dist/u-0.1.0/manifest.yaml", UMANI),
        ("dist/u-0.1.0/routes.js", "export default [];\n"),
        (
            "dist/u-0.1.0/migrations/0001__init.sql",
            "CREATE TABLE g (x);",
        ),
    ]);
    let db2 = format!("sqlite://{}/rel.sqlite", t.0.display());
    let dist = dist.join("dist"); // rel_fixture 返回父目录
    let mut cfg = cert_cfg(&t.0);
    cfg.server.port = 0;
    cfg.db.insert("default".into(), db2.clone());
    let e = start(cfg, &t.0, dist.clone(), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("M004") && e.contains("oj migrate"), "{e}");
    // c) 显式 apply（oj migrate 等价）→ verify 放行。
    let acc = only_js::bridge::DbBackendRegistry::builtin()
        .connect(&db2, &t.0)
        .await
        .unwrap();
    oj_migrate_apply(&dist, &acc).await;
    let mut cfg = cert_cfg(&t.0);
    cfg.server.port = 0;
    cfg.db.insert("default".into(), db2);
    let _ = start(cfg, &t.0, dist, "/v1/api".into()).await.unwrap();
    // d) off 逃生门：空库 + 迁移不执行也不拒启。
    let dist2 = rel_fixture(&[
        ("dist/manifests.yaml", "u: 0.1.0\n"),
        ("dist/u-0.1.0/manifest.yaml", UMANI),
        ("dist/u-0.1.0/routes.js", "export default [];\n"),
        (
            "dist/u-0.1.0/migrations/0001__init.sql",
            "CREATE TABLE g (x);",
        ),
    ])
    .join("dist");
    let mut cfg = cert_cfg(&t.0);
    cfg.server.port = 0;
    cfg.db.insert(
        "default".into(),
        format!("sqlite://{}/off.sqlite", t.0.display()),
    );
    cfg.server.migrate_on_start = Some("off".into());
    let _ = start(cfg, &t.0, dist2, "/v1/api".into()).await.unwrap();
    // e) 非法值 fail-fast。
    let mut cfg = cert_cfg(&t.0);
    cfg.server.migrate_on_start = Some("nope".into());
    let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("illegal value"), "{e}");
}

/// 测试辅助：release 布局 apply_all（等价 `oj migrate -d dist`）。
async fn oj_migrate_apply(dist: &Path, acc: &Arc<dyn DataAccessor>) {
    crate::migrate::apply_all(Some(acc), dist, false, false, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn release_fail_fast_paths() {
    // a) 无 manifests.yaml：自动判定（目录即模式）下这是 dev 树——产物版目录名
    // user-0.1.0 与 manifest name 不符，dev 内省报 manifest 错（release 缺锁的
    // 报错面由下方 b 用例覆盖：有锁才进 release 腿）。
    let t = rel_fixture(&[("dist/user-0.1.0/manifest.yaml", MANI)]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("manifest"), "{e}");
    // b) 锁指向不存在版本
    let t = rel_fixture(&[
        ("dist/manifests.yaml", "user: 9.9.9\n"),
        ("dist/user-0.1.0/manifest.yaml", MANI),
    ]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("9.9.9"), "{e}");
    // c) version 注入
    let t = rel_fixture(&[("dist/manifests.yaml", "user: ../../etc\n")]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("version") || e.contains("illegal"), "{e}");
    // d) manifest name 不符
    let t = rel_fixture(&[
        ("dist/manifests.yaml", "user: 0.1.0\n"),
        (
            "dist/user-0.1.0/manifest.yaml",
            "name: other\ndesc: d\nversion: 0.1.0\n",
        ),
    ]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("name"), "{e}");
}

#[tokio::test]
async fn release_routes_js_syntax_error_fails_fast() {
    // routes.js 语法错 → reader 解析失败 → 启动 Err（spec §4）
    let t = rel_fixture(&[
        ("dist/manifests.yaml", "user: 0.1.0\n"),
        ("dist/user-0.1.0/manifest.yaml", MANI),
        ("dist/user-0.1.0/routes.js", "export default [ "),
    ]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("routes.js"), "{e}");
}

#[tokio::test]
async fn release_conflicting_patterns_fail_fast() {
    // 两模块同 pattern 同 method → from_entries failures → 启动 Err（spec §4）
    let t = rel_fixture(&[
        ("dist/manifests.yaml", "user: 0.1.0\nother: 0.9.0\n"),
        ("dist/user-0.1.0/manifest.yaml", MANI),
        (
            "dist/user-0.1.0/routes.js",
            "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-x.js\" } ];\n",
        ),
        (
            "dist/user-0.1.0/item/api-x.js",
            "export default { get() { json.ok({}); } };\n",
        ),
        (
            "dist/other-0.9.0/manifest.yaml",
            "name: other\ndesc: d\nversion: 0.9.0\n",
        ),
        (
            "dist/other-0.9.0/routes.js",
            "export default [ { method: \"get\", pattern: \"user/item/{id}\", file: \"item/api-y.js\" } ];\n",
        ),
        (
            "dist/other-0.9.0/item/api-y.js",
            "export default { get() { json.ok({}); } };\n",
        ),
    ]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("conflict"), "{e}");
}

#[tokio::test]
async fn release_keeps_real_error_from_corrupt_lock() {
    // M-2：锁解析错保留真实错误（不与缺失混为一句 not found or invalid）
    let t = rel_fixture(&[
        ("dist/manifests.yaml", "user: [unclosed\n"),
        ("dist/user-0.1.0/manifest.yaml", MANI),
    ]);
    let e = start(cert_cfg(&t), &t, t.join("dist"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("unclosed") || e.contains("parse"), "{e}");
}

#[tokio::test]
async fn server_app_path_serves_static_relative_to_config_dir() {
    let t = tmpdir("sc-root");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    std::fs::write(t.0.join("index.html"), "<h1>site</h1>").unwrap();
    let mut cfg = cert_cfg(&t.0);
    cfg.server.port = 0; // 随机端口
    // web 挂载相对 config_dir（v0.1.58：原 server.app_path: "."）。
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some(".".into()),
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    let (addr, _h) = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .unwrap();
    let r = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("site"));
}

#[tokio::test]
async fn server_app_path_missing_dir_fails_fast() {
    // dir 指向不存在的子目录（相对 tmpdir），使模块扫描为空、顺利走到 server.app_path
    // 校验；此前用绝对 "src" 会 canonicalize 到 oj/src（含无 manifest 的 test_ext），
    // 在抵达 server.app_path 检查前就于模块扫描阶段报错，掩盖了本测试真正要验证的逻辑。
    let t = tmpdir("sc-root-missing");
    let mut cfg = cert_cfg(&t.0);
    cfg.mounts.push(only_js::config::MountConf {
        prefix: "/".into(),
        api: None,
        web: Some("no-such-dir".into()),
        spa: None,
        headers: Default::default(),

        ..Default::default()
    });
    let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("mounts[") && e.contains("no-such-dir"), "{e}");
}

/// P0：模块种子重放——src/u/seed.sql 建表插数，handler 查得到。
#[tokio::test]
async fn module_seeds_replayed_and_served() {
    let t = tmpdir("sc-mseed");
    std::fs::create_dir_all(t.0.join("src/u")).unwrap();
    std::fs::write(
        t.0.join("src/u/manifest.yaml"),
        "name: u\ndesc: d\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        t.0.join("src/u/seed.sql"),
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);\n\
             INSERT OR IGNORE INTO t (id, v) VALUES (1, 'mod');\n",
    )
    .unwrap();
    let mut cfg = cert_cfg(&t.0);
    cfg.server.port = 0;
    cfg.db.insert(
        "default".into(),
        format!("sqlite://{}/db.sqlite", t.0.display()),
    );
    let (addr, _h) = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .unwrap();
    std::fs::create_dir_all(t.0.join("src/u/f")).unwrap();
    std::fs::write(
            t.0.join("src/u/f/api.ts"),
            "export default { get() { db.query(\"select v from t where id = ?\", [1]).then(r => json.ok(r)); } };\n",
        )
        .unwrap();
    let resp = reqwest::get(format!("http://{addr}/v1/api/u/f/"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["data"][0]["v"], "mod", "{v}");
}

// ----- 插件装配（spec §5）：全部经 cfg.plugins_dir 注入（每测试独立 Config，
// 无全局 env 竞争；OAT_PLUGINS_DIR 只留给真实部署路径）。 -----

use std::sync::OnceLock;

fn host_triple() -> String {
    let out = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .unwrap()
        .to_string()
}

/// 插件存放文件名（= loader plugin_file_name）。
fn plugin_file(name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

/// 编译 oj-es 产物路径（全进程一次，oj-es 已有 debug 构建，命中缓存）。
fn es_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-es"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-es");
        assert!(status.success(), "oj-es build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_es.{ext}"))
    })
    .clone()
}

fn es_cfg(endpoint: &str) -> Config {
    let mut es = std::collections::HashMap::new();
    es.insert(
        "default".to_string(),
        config::EsCfg {
            endpoint: endpoint.to_string(),
        },
    );
    Config {
        es,
        ..Default::default()
    }
}

/// plugins 键声明但文件缺失 → fail fast。
#[tokio::test(flavor = "current_thread")]
async fn manifest_missing_file_fails_fast() {
    let t = tmpdir("sc-man");
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
    let mut cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    cfg.plugins.insert("ghost".into(), serde_json::json!({}));
    let mut r = Registries::default();
    let e = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("plugin file missing"), "{e}");
}

/// 缺省扫描空目录 → 零插件、仅内置后端（es 未配置，不触发 §2 闸门）。
#[tokio::test(flavor = "current_thread")]
async fn scan_empty_dir_yields_only_builtin() {
    let t = tmpdir("sc-empty");
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert!(plugins.is_empty());
    assert!(r.es.is_none());
}

/// 扫描到损坏插件 → fail fast（不静默跳过）。
#[tokio::test(flavor = "current_thread")]
async fn scan_bad_plugin_fails_fast() {
    let t = tmpdir("sc-bad");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(pdir.join(plugin_file("broken")), b"not a real dylib").unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    let mut r = Registries::default();
    let e = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("plugins scan"), "{e}");
}

/// 配置声明 [es] 但插件未装 → 启动期报错（§2 闸门）。
#[tokio::test(flavor = "current_thread")]
async fn es_declared_without_plugin_fails_startup() {
    let t = tmpdir("sc-esgate");
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap();
    let mut cfg = es_cfg("http://127.0.0.1:1");
    cfg.plugins_dir = Some(t.0.clone());
    let mut r = Registries::default();
    let e = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("no es plugin loaded"), "{e}");
}

/// 全链路：真实 oj-es 插件装配 → es 后端经 FfiEsBackend 接线（handle 0）+ 自省信息。
#[tokio::test(flavor = "current_thread")]
async fn es_plugin_wires_backend() {
    let t = tmpdir("sc-esplug");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
    let mut cfg = es_cfg("http://127.0.0.1:1");
    cfg.plugins_dir = Some(t.0.clone());
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "es");
    assert!(r.es.is_some(), "es backend must be wired from the plugin");
}

/// plugins: 一段三用（spec「plugins: 统一语义」）：map 键即严格清单——键 ∈ 装配
/// 结果（空对象值只影响 cfg 透传、不影响加载）；map 外插件不装配；空 map = 扫描模式。
#[tokio::test(flavor = "current_thread")]
async fn plugins_map_keys_drive_strict_load() {
    let t = tmpdir("sc-mapkeys");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
    // 1) 键 = 清单：{"es": {}} → 只装配 es，es 后端照常接线。
    let mut cfg = es_cfg("http://127.0.0.1:1");
    cfg.plugins_dir = Some(t.0.clone());
    cfg.plugins.insert("es".into(), serde_json::json!({}));
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "es");
    assert!(r.es.is_some(), "es backend must be wired from the plugin");
    // 2) map 外插件不装配：es 在盘上但键只有 ghost → 严格模式缺 ghost fail fast
    //    （扫描模式此时会装配 es， fail 即证明走的是清单路径）。
    let mut cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    cfg.plugins.insert("ghost".into(), serde_json::json!({}));
    let mut r = Registries::default();
    let e = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("plugin file missing"), "{e}");
    // 3) 空 map = 扫描模式：同一目录照旧扫描装配。
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "es");
}

/// 严格清单装配顺序确定：HashMap 键序逐进程漂移，entries 排序后按名字序装配，
/// Vec<PluginInfo> 输出跨进程稳定（GET {base}/plugins 可比对）。插入序故意逆字母序。
#[tokio::test(flavor = "current_thread")]
async fn strict_manifest_loads_in_name_order() {
    let t = tmpdir("sc-manorder");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(es_plugin_artifact(), pdir.join(plugin_file("es"))).unwrap();
    std::fs::copy(auth_plugin_artifact(), pdir.join(plugin_file("auth"))).unwrap();
    let mut cfg = es_cfg("http://127.0.0.1:1");
    cfg.plugins_dir = Some(t.0.clone());
    cfg.plugins.insert("es".into(), serde_json::json!({}));
    cfg.plugins
        .insert("auth".into(), serde_json::json!({"jwt_secret": "x"}));
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["auth", "es"]);
}

/// 编译 oj-db-mysql 产物路径（全进程一次；sqlx 首次编译慢，OnceLock 缓存）。
fn db_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-db-mysql"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-db-mysql");
        assert!(status.success(), "oj-db-mysql build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_db_mysql.{ext}"))
    })
    .clone()
}

/// 真实 oj-db-mysql 插件装配 → db 工厂经 FfiDbBackend 注册进 DbBackendRegistry
/// （scheme 认领；连接转发由 ffi.rs 适配器测试覆盖，此处不断网连接）。
#[tokio::test(flavor = "current_thread")]
async fn db_plugin_wires_backend() {
    let t = tmpdir("sc-dbplug");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(db_plugin_artifact(), pdir.join(plugin_file("db-mysql"))).unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        plugins: HashMap::from([("db-mysql".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "db-mysql");
    let names = r.dbs.backend_names();
    assert!(
        names.contains(&"db-mysql"),
        "factory not registered: {names:?}"
    );
    // 未认领 scheme 仍 unknown（插件没声明 oracle）→ 快速失败
    let e = r
        .dbs
        .connect("oracle://x", &t.0)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("unknown db scheme"), "{e}");
}

/// mysql DSN 但插件未装 → 明确 "unknown db scheme"（不触网，快速失败）。
#[tokio::test(flavor = "current_thread")]
async fn db_declared_without_plugin_unknown_scheme() {
    let t = tmpdir("sc-dbplug-none");
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    let mut r = Registries::default();
    assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    let mut m = std::collections::HashMap::new();
    m.insert(
        "mydb".to_string(),
        "mysql://u:p@127.0.0.1:1/app".to_string(),
    );
    let e = connect_dbs(&m, &r.dbs, &t.0)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("mydb"), "{e}");
    assert!(e.contains("unknown db scheme"), "{e}");
}

/// 编译 oj-blob-s3 产物路径（全进程一次；object_store aws 首次编译慢，OnceLock 缓存）。
fn blob_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-blob-s3"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-blob-s3");
        assert!(status.success(), "oj-blob-s3 build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_blob_s3.{ext}"))
    })
    .clone()
}

/// 真实 oj-blob-s3 插件装配 → Registries.blob 槽就位；经它 connect 建后端
/// （cfg 校验离线路径：bucket/region 必填在插件侧 fail-fast，不触网）。
#[tokio::test(flavor = "current_thread")]
async fn blob_plugin_wires_vtable_and_connect_gate() {
    let t = tmpdir("sc-blobplug");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(blob_plugin_artifact(), pdir.join(plugin_file("blob-s3"))).unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        plugins: HashMap::from([("blob-s3".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "blob-s3");
    assert!(r.blob.is_some(), "blob vtable slot not registered");
    // 经 vtable connect：配置缺 bucket → 插件侧 fail-fast（快速失败不触网）
    let cfg_json = serde_json::json!({ "driver": "s3", "region": "us-east-1" }).to_string();
    let e = blob_backend_connect(r.blob.unwrap(), "img", &cfg_json)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("bucket required"), "{e}");
}

/// 编译 oj-bus-kafka 产物路径（全进程一次；rdkafka 首次编译慢，OnceLock 缓存）。
fn bus_kafka_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-bus-kafka"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-bus-kafka");
        assert!(status.success(), "oj-bus-kafka build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_bus_kafka.{ext}"))
    })
    .clone()
}

/// 编译 oj-bus-rabbitmq 产物路径（全进程一次；lapin 首次编译慢，OnceLock 缓存）。
fn bus_rabbitmq_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-bus-rabbitmq"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-bus-rabbitmq");
        assert!(status.success(), "oj-bus-rabbitmq build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_bus_rabbitmq.{ext}"))
    })
    .clone()
}

/// 真实 oj-bus-kafka 插件装配 → Registries.bus 注册 kind "kafka"（kind 由插件名
/// 去 "bus-" 前缀推断）。connect 需真 broker，此处只验证注册与 kind 键选。
#[tokio::test(flavor = "current_thread")]
async fn bus_plugin_wires_kind() {
    let t = tmpdir("sc-busplug");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(
        bus_kafka_plugin_artifact(),
        pdir.join(plugin_file("bus-kafka")),
    )
    .unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        plugins: HashMap::from([("bus-kafka".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "bus-kafka");
    let kinds = r.bus.kinds();
    assert!(
        kinds.iter().any(|k| k == "kafka"),
        "kind not registered: {kinds:?}"
    );
    // 本地 kind 仍内置
    assert!(kinds.iter().any(|k| k == "local"), "{kinds:?}");
}

/// broker.kind=kafka 但插件未装 → "unknown broker kind"（列出已知 kind，快速失败）。
#[tokio::test(flavor = "current_thread")]
async fn kafka_declared_without_plugin_unknown_kind() {
    let t = tmpdir("sc-busplug-none");
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    let mut r = Registries::default();
    assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    let broker_cfg = only_js::config::BrokerCfg {
        kind: "kafka".into(),
        ..Default::default()
    };
    let e = r
        .bus
        .connect(&Some(broker_cfg))
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(e.contains("unknown broker kind 'kafka'"), "{e}");
}

/// 编译 oj-kv-redis 产物路径（全进程一次）。
fn kv_redis_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-kv-redis"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-kv-redis");
        assert!(status.success(), "oj-kv-redis build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_kv_redis.{ext}"))
    })
    .clone()
}

/// 真实 oj-kv-redis 插件装配 → Registries.kv 槽就位；经 vtable connect 建 KV
/// （连接探活需真 redis——此处只验证槽位 + connect 到无监听端口 fail-fast，不触网）。
#[tokio::test(flavor = "current_thread")]
async fn kv_plugin_wires_vtable_and_connect_gate() {
    let t = tmpdir("sc-kvplug");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(
        kv_redis_plugin_artifact(),
        pdir.join(plugin_file("kv-redis")),
    )
    .unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        plugins: HashMap::from([("kv-redis".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "kv-redis");
    assert!(r.kv.is_some(), "kv vtable slot not registered");
    // 经 vtable connect：无监听端口 → 插件侧探活 fail-fast（不挂启动）。
    let e = kv_backend_connect(r.kv.unwrap(), "redis://127.0.0.1:1/")
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("redis connect"), "{e}");
}

/// redis.default 声明但无 kv 插件 → 启动 fail-fast（§2 闸门，不退化静默）。
#[tokio::test(flavor = "current_thread")]
async fn redis_declared_without_kv_plugin_fails_fast() {
    let t = tmpdir("sc-kvplug-none");
    std::fs::create_dir_all(t.0.join("src")).unwrap();
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
    let mut cfg = cert_cfg(&t.0);
    cfg.plugins_dir = Some(t.0.clone());
    cfg.redis
        .insert("default".into(), "redis://127.0.0.1:1/".into());
    let mut r = Registries::default();
    assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert!(r.kv.is_none());
    let e = start(cfg, &t.0, t.0.join("src"), "/v1/api".into())
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("no kv plugin loaded"), "{e}");
    assert!(e.contains("cargo xtask plugin kv-redis"), "{e}");
}

/// 编译 oj-auth 产物路径（全进程一次）。
fn auth_plugin_artifact() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let status = std::process::Command::new("cargo")
            .args(["build", "-p", "oj-auth"])
            .current_dir(&root)
            .status()
            .expect("invoke cargo build for oj-auth");
        assert!(status.success(), "oj-auth build failed");
        let (prefix, ext) = if cfg!(target_os = "windows") {
            ("", "dll")
        } else if cfg!(target_os = "macos") {
            ("lib", "dylib")
        } else {
            ("lib", "so")
        };
        root.join("target/debug")
            .join(format!("{prefix}oj_auth.{ext}"))
    })
    .clone()
}

/// auth 声明但无 auth 插件 → fail-fast（§2 闸门，不静默放行）。
#[tokio::test(flavor = "current_thread")]
async fn auth_plugin_required_when_configured() {
    let t = tmpdir("sc-authgate");
    std::fs::create_dir_all(t.0.join(host_triple())).unwrap(); // 空插件目录 → 零插件
    let mut cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"x\"\n").unwrap());
    let mut r = Registries::default();
    let e = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .err()
        .unwrap_or_default();
    assert!(e.contains("no auth plugin loaded"), "{e}");
}

/// 全链路：真实 oj-auth 插件装配 → Registries.auth 槽就位；经 FfiAuthGuard
/// 验签测试签发的 JWT（sign 用 jsonwebtoken）：匿名路径放行、有效 token 出 user、
/// 坏 token / 缺 token 拒绝。
#[tokio::test(flavor = "current_thread")]
async fn auth_plugin_wires_guard_and_verifies_jwt() {
    let t = tmpdir("sc-authplug");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(auth_plugin_artifact(), pdir.join(plugin_file("auth"))).unwrap();
    let mut cfg = Config {
        plugins_dir: Some(t.0.clone()),
        ..Default::default()
    };
    cfg.auth = Some(
        serde_yaml::from_str("jwt_secret: \"oj-test-secret\"\nanonymous_paths:\n  - /health\n")
            .unwrap(),
    );
    let mut r = Registries::default();
    assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    let vt = r
        .auth
        .expect("auth vtable slot must be wired from the plugin");
    let guard = only_js::bridge::plugin_loader::auth_guard_from_vtable(vt);
    // 匿名路径 → None（放行，无 token 也行）
    assert!(
        guard
            .verify("/health", "GET", None, None)
            .unwrap()
            .is_none()
    );
    // 有效 token → user（sub → id）
    let now = serve::test_support::now_secs();
    let claims = serde_json::json!({
        "sub": "u1", "roles": ["admin"], "iat": now, "exp": now + 3600
    });
    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(b"oj-test-secret"),
    )
    .unwrap();
    let user = guard
        .verify("/items", "GET", Some(&format!("Bearer {token}")), None)
        .unwrap()
        .expect("valid token must yield user");
    assert_eq!(user["id"], "u1");
    // 坏 token / 缺 token → Err（401 语义）
    assert!(
        guard
            .verify("/items", "GET", Some("Bearer nope"), None)
            .is_err()
    );
    assert!(guard.verify("/items", "GET", None, None).is_err());
}

/// JSON publish → 订阅通道必收 text 信封帧（bus Bytes 载荷才会走 Binary）。
fn env_text(f: only_js::bridge::WsSend) -> String {
    match f {
        only_js::bridge::WsSend::Text(s) => s,
        other => panic!("json publish must deliver text frame, got {other:?}"),
    }
}

/// 硬验收（Task 6.1 Step 5）：真 kafka 插件 broker 下 Task 0.5 共享语义回归
/// （env-gated，`OJ_TEST_KAFKA_BROKERS` 给逗号分隔 bootstrap servers；未设置 → 跳过）。
/// 同一 broker 实例（一个 FfiEventBroker，单消费循环/每 topic）上两个订阅通道
/// （模拟跨 actor 池与全部 WS 连接）共享同一 topic：一次 publish → 插件消费循环
/// 经 host.deliver → 全局 DELIVER_TARGETS 扇出 → 两通道都收到。
#[tokio::test(flavor = "multi_thread")]
async fn kafka_plugin_broker_shared_across_channels() {
    let brokers = match std::env::var("OJ_TEST_KAFKA_BROKERS") {
        Ok(b) if !b.is_empty() => b,
        _ => {
            eprintln!("skip: OJ_TEST_KAFKA_BROKERS unset");
            return;
        }
    };
    let t = tmpdir("sc-busshare-k");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(
        bus_kafka_plugin_artifact(),
        pdir.join(plugin_file("bus-kafka")),
    )
    .unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        plugins: HashMap::from([("bus-kafka".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    let broker_cfg = only_js::config::BrokerCfg {
        kind: "kafka".into(),
        brokers: brokers.split(',').map(|s| s.trim().to_string()).collect(),
        group: Some("oj-shared".into()),
        topic_prefix: Some(format!("ojshare-k-{}", std::process::id())),
        ..Default::default()
    };
    let broker = r.bus.connect(&Some(broker_cfg)).await.unwrap();
    let topic = format!("shared.{}", std::process::id());
    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe(&topic, tx1).await.unwrap();
    broker.subscribe(&topic, tx2).await.unwrap(); // 同 topic 第二通道（不新起消费）
    tokio::time::sleep(std::time::Duration::from_millis(500)).await; // 等消费就绪
    broker
        .publish(
            &topic,
            &only_js::bridge::BusPayload::Json(serde_json::json!({ "v": 9 })),
        )
        .await
        .unwrap();
    let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), rx1.recv())
        .await
        .expect("shared receive 1 timeout")
        .expect("channel 1 closed");
    let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), rx2.recv())
        .await
        .expect("shared receive 2 timeout")
        .expect("channel 2 closed");
    let t1 = env_text(f1);
    let v1: serde_json::Value = serde_json::from_str(&t1).unwrap();
    assert_eq!(v1["data"]["v"], 9, "{t1}");
    let t2 = env_text(f2);
    let v2: serde_json::Value = serde_json::from_str(&t2).unwrap();
    assert_eq!(v2["data"]["v"], 9, "{t2}");
}

/// 硬验收（Task 6.1 Step 5）：真 rabbitmq 插件 broker 下 Task 0.5 共享语义回归
/// （env-gated，`OJ_TEST_RABBITMQ_URL` 给 amqp URL；未设置 → 跳过）。语义同 kafka 测试。
#[tokio::test(flavor = "multi_thread")]
async fn rabbitmq_plugin_broker_shared_across_channels() {
    let url = match std::env::var("OJ_TEST_RABBITMQ_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skip: OJ_TEST_RABBITMQ_URL unset");
            return;
        }
    };
    let t = tmpdir("sc-busshare-r");
    let pdir = t.0.join(host_triple());
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::copy(
        bus_rabbitmq_plugin_artifact(),
        pdir.join(plugin_file("bus-rabbitmq")),
    )
    .unwrap();
    let cfg = Config {
        plugins_dir: Some(t.0.clone()),
        plugins: HashMap::from([("bus-rabbitmq".into(), serde_json::json!({}))]),
        ..Default::default()
    };
    let mut r = Registries::default();
    let (plugins, _loaded) = assemble_plugins(&cfg, &serde_json::Value::Null, &t.0, &mut r, None)
        .await
        .unwrap();
    assert_eq!(plugins.len(), 1);
    let broker_cfg = only_js::config::BrokerCfg {
        kind: "rabbitmq".into(),
        url: Some(url),
        topic_prefix: Some(format!("ojshare-r-{}", std::process::id())),
        ..Default::default()
    };
    let broker = r.bus.connect(&Some(broker_cfg)).await.unwrap();
    let topic = format!("shared.{}", std::process::id());
    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    broker.subscribe(&topic, tx1).await.unwrap();
    broker.subscribe(&topic, tx2).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    broker
        .publish(
            &topic,
            &only_js::bridge::BusPayload::Json(serde_json::json!({ "v": 11 })),
        )
        .await
        .unwrap();
    let f1 = tokio::time::timeout(std::time::Duration::from_secs(10), rx1.recv())
        .await
        .expect("shared receive 1 timeout")
        .expect("channel 1 closed");
    let f2 = tokio::time::timeout(std::time::Duration::from_secs(10), rx2.recv())
        .await
        .expect("shared receive 2 timeout")
        .expect("channel 2 closed");
    let t1 = env_text(f1);
    let v1: serde_json::Value = serde_json::from_str(&t1).unwrap();
    assert_eq!(v1["data"]["v"], 11, "{t1}");
    let t2 = env_text(f2);
    let v2: serde_json::Value = serde_json::from_str(&t2).unwrap();
    assert_eq!(v2["data"]["v"], 11, "{t2}");
}
