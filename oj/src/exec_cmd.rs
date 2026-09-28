//! `oj exec` 子命令：直接执行 ts/js 脚本（spec §3.2 流程）。
//!
//! 钉线程模式同 `oj test`（JsRuntime !Send）。扩展顺序钉死：
//! ws_client_extensions → bridge_ext_init(stable) → exec_ext_init(options)
//! （ws 在前：bootstrap.js 静态 import 依赖；exec_ext 最后：覆盖 log 依赖
//! bridge bootstrap 先跑）。

use std::path::{Path, PathBuf};
use std::rc::Rc;

use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions};
use only_js::bridge::OjModuleLoader;
use only_js::bridge::{
    boot_runtime, bridge_ext_init, patch_fs_loaded_sources, ws_client_extensions,
};
use tokio::runtime::Builder as TokioBuilder;

use crate::app::{Backend, assemble_backend};
use crate::args::ExecArgs;
use crate::exec_ext::{ExecOptions, oj_exec_ext_init};
use crate::server_cmd::load_app_config;

/// 入口：解析校验 → 钉线程 → 装配后端 → 执行脚本 → 进程退出码。
pub fn run(a: ExecArgs) -> Result<i32, String> {
    let script = PathBuf::from(&a.file);
    match script.extension().and_then(|e| e.to_str()) {
        Some("ts") | Some("js") => {}
        _ => return Err(format!("exec: 仅支持 .ts/.js: {}", script.display())),
    }
    let (cfg, config_dir, dir, _ts, base) = load_app_config(&a.config, a.dir.as_deref(), None)?;
    // exec 恒 dev 语义（spec §3.4）：脚本没有 release 形态；dir 仅作 schema 白名单来源。
    let db_override = a.db.clone();
    // --log-file 打开失败仅告警（spec §4），终端照出。
    let log_file = match &a.log_file {
        Some(p) => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
        {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("warn: exec: open --log-file {p}: {e}（终端输出继续）");
                None
            }
        },
        None => None,
    };
    let args = a.args;
    let handle = std::thread::Builder::new()
        .name("oj-exec".into())
        .spawn(move || {
            let rt = TokioBuilder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("exec runtime: {e}"))?;
            rt.block_on(async move {
                let backend =
                    assemble_backend(&cfg, &config_dir, &dir, &base, true, db_override.as_deref())
                        .await?;
                // 迁移门禁（spec §3.1）：exec 缺省全跳过（与 server dev 缺省 auto 相反，
                // 有意不对称）；仅 config 显式写 migrate_on_start 时执行对应项，
                // reconcile 跟随 auto。非法值 fail-fast（与 server 文案一致）。
                if let Some(gate) = cfg.server.migrate_on_start.as_deref() {
                    let stable = backend.stable();
                    let db_key = db_override.as_deref().unwrap_or("default");
                    match gate {
                        "auto" => {
                            crate::migrate::apply_all(stable.dbs.get(db_key), &dir, true, false)
                                .await?;
                            for l in crate::schema::reconcile_all(
                                stable.dbs.get(db_key).map(|a| a.as_ref()),
                                &dir,
                                true,
                                db_key,
                            )
                            .await?
                            {
                                eprintln!("schema: {l}");
                            }
                        }
                        "verify" => {
                            crate::migrate::verify_all(stable.dbs.get(db_key), &dir, true).await?
                        }
                        "off" => {}
                        other => {
                            return Err(format!(
                                "server.migrate_on_start: illegal value {other:?} (auto|verify|off)"
                            ));
                        }
                    }
                }
                run_script(&backend, &script, ExecOptions { args, log_file }).await
            })
        })
        .map_err(|e| format!("spawn exec thread: {e}"))?;
    handle
        .join()
        .map_err(|_| "exec thread panicked".to_string())?
}

/// 装配 JsRuntime 并执行脚本。Ok(0) = settle 无异常；Err = 加载/求值异常（含 V8 堆栈）。
pub(crate) async fn run_script(
    backend: &Backend,
    script: &Path,
    options: ExecOptions,
) -> Result<i32, String> {
    let stable = backend.stable();
    let loader = stable.loader.clone();
    let module_loader: Option<Rc<dyn deno_core::ModuleLoader>> =
        loader.map(|inner| Rc::new(OjModuleLoader { inner }) as Rc<dyn deno_core::ModuleLoader>);

    let mut extensions = ws_client_extensions();
    extensions.push(bridge_ext_init(stable.clone()));
    extensions.push(oj_exec_ext_init(options));
    // deno_* 扩展以构建机绝对路径声明 JS；进 runtime 前统一换为内嵌源码。
    patch_fs_loaded_sources(&mut extensions);

    let mut rt = JsRuntime::new(RuntimeOptions {
        extensions,
        module_loader,
        ..Default::default()
    });

    // ext_boot：`oj exec` 不走 RuntimePool（直接建 JsRuntime），故在此补跑一次。
    if let Some(spec) = stable.boot.as_deref() {
        boot_runtime(&mut rt, spec)
            .await
            .map_err(|e| format!("ext_boot: {e}"))?;
    }

    // 入口不经 module loader（同任务驱动 run_task 的做法）：looks_cjs 会把无
    // import/export 的脚本误包成 CJS 绞杀 TLA——直接以转译源 + versioned URL 走
    // side-module（TLA 保真）；脚本内相对 import 由 OjModuleLoader 照常解析。
    let src = only_js::bridge::transpile::cached_transpile(script)
        .map_err(|e| format!("exec: compile {}: {e}", script.display()))?;
    // 一次性 runtime 无需 ?v=mtime 版本化（桥内 versioned_specifier 未导出；exec 无热重载）。
    let abs = std::fs::canonicalize(script)
        .map_err(|e| format!("exec: canonicalize {}: {e}", script.display()))?;
    let spec = ModuleSpecifier::from_file_path(&abs)
        .map_err(|_| format!("exec: bad script path: {}", script.display()))?;
    let id = rt
        .load_side_es_module_from_code(&spec, format!("{src}\n"))
        .await
        .map_err(|e| format!("exec: load {}: {e}", script.display()))?;
    let eval = rt.mod_evaluate(id);
    rt.run_event_loop(PollEventLoopOptions::default())
        .await
        .map_err(|e| format!("exec: run {}: {e}", script.display()))?;
    eval.await
        .map_err(|e| format!("exec: {}: {e}", script.display()))?;
    Ok(0)
}

/// 测试薄封装：显式 args/log_file（`run()` 的进程内同款路径）。
#[cfg(test)]
pub(crate) async fn run_script_ext(
    backend: &Backend,
    script: &Path,
    args: Vec<String>,
    log_file: Option<std::fs::File>,
) -> Result<i32, String> {
    run_script(backend, script, ExecOptions { args, log_file }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Backend, assemble_backend};
    use crate::args::ExecArgs;
    use only_js::config::Config;

    async fn backend_fixture(tmp: &Path) -> Backend {
        let cfg = Config::default();
        assemble_backend(&cfg, tmp, tmp, "/v1/api", true, None)
            .await
            .unwrap()
    }

    fn write_script(dir: &Path, name: &str, code: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, code).unwrap();
        p
    }

    /// ① console 到 sink（经 --log-file 文件断言）：终端 + JSONL 双写。
    #[tokio::test(flavor = "current_thread")]
    async fn given_console_log_when_run_then_jsonl_contains_msg() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t6a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let script = write_script(&tmp, "m.ts", r#"console.log("hello-exec", 1, {x:2});"#);
        let log = tmp.join("out.jsonl");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let backend = backend_fixture(&tmp).await;
        let code = run_script_ext(&backend, &script, vec![], Some(f))
            .await
            .unwrap();
        assert_eq!(code, 0);
        let line = std::fs::read_to_string(&log).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(v["msg"], r#"hello-exec 1 {"x":2}"#);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// ② args 注入：必须非空 argv（空数组在时序 bug 下也成立，钉死评审 L2）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_dashdash_args_when_run_then_args_reachable() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t6b-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = write_script(
            &tmp,
            "a.ts",
            r#"throw new Error("ARGS=" + args.join("|"));"#,
        );
        let backend = backend_fixture(&tmp).await;
        let e = run_script_ext(&backend, &script, vec!["-x".into(), "foo bar".into()], None)
            .await
            .unwrap_err();
        assert!(e.contains("ARGS=-x|foo bar"), "{e}");
    }

    /// ③ 顶层异常 → Err 携带 V8 消息；④ 相对 import（显式扩展名，项目根内）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_relative_import_when_run_then_module_chain_resolves() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t6c-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        write_script(&tmp, "util.ts", r#"export const tag = "util-ok";"#);
        let script = write_script(
            &tmp,
            "main.ts",
            r#"import { tag } from "./util.ts"; throw new Error("TAG=" + tag);"#,
        );
        let backend = backend_fixture(&tmp).await;
        let e = run_script_ext(&backend, &script, vec![], None)
            .await
            .unwrap_err();
        assert!(e.contains("TAG=util-ok"), "{e}");
    }

    /// ⑤ 顶层 await 跑完 event loop（microtask settle 后无异常 → 0）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_top_level_await_when_run_then_settles_ok() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t6d-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = write_script(
            &tmp,
            "tla.ts",
            r#"await Promise.resolve(); globalThis.__x = 1;"#,
        );
        let backend = backend_fixture(&tmp).await;
        assert_eq!(
            run_script_ext(&backend, &script, vec![], None)
                .await
                .unwrap(),
            0
        );
    }

    /// ⑥ 装配 fail-fast 透传（spec §4 行 2）：strict 清单列了不存在的插件 → Err。
    #[test]
    fn given_strict_manifest_missing_plugin_when_run_then_err_mentions_plugins() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("config.yaml"), "plugins:\n  nope-plugin: {}\n").unwrap();
        let script = tmp.join("s.ts");
        std::fs::write(&script, "console.log(1);").unwrap();
        let e = run(ExecArgs {
            file: script.to_string_lossy().into(),
            config: tmp.join("config.yaml").to_string_lossy().into(),
            dir: Some(tmp.to_string_lossy().into()),
            db: None,
            log_file: None,
            args: vec![],
        })
        .unwrap_err();
        assert!(e.contains("plugin"), "{e}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// ⑦ 迁移门禁（spec §3.1，终审 I-1）：缺省全跳过 → 脚本查不到表；显式
    /// `migrate_on_start: auto` → apply 建表。两分支钉死 exec 与 server dev
    /// （缺省 auto）相反的缺省。
    #[test]
    fn given_migrate_gate_when_run_then_default_skips_and_explicit_auto_applies() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t7gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src/t7m/migrations")).unwrap();
        std::fs::write(
            tmp.join("src/t7m/manifest.yaml"),
            "name: t7m\ndesc: d\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("src/t7m/migrations/0001__init.sql"),
            "create table t7gate (id integer);",
        )
        .unwrap();
        let script = tmp.join("probe.ts");
        std::fs::write(
            &script,
            r#"const r = await db.query("select count(*) as c from sqlite_master where type = 'table' and name = 't7gate'", []); if (r[0].c !== 1) throw new Error("GATE=missing");"#,
        )
        .unwrap();
        let dsn = oj_plugin_ffi::path_util::sqlite_file_dsn(&tmp.join("t7.db"));
        let mk = |gate: Option<&str>| {
            let yaml = match gate {
                Some(g) => {
                    format!("db:\n  default: \"{dsn}\"\nserver:\n  migrate_on_start: {g}\n")
                }
                None => format!("db:\n  default: \"{dsn}\"\n"),
            };
            let p = tmp.join(format!(
                "cfg-{}.yaml",
                if gate.is_some() { "on" } else { "off" }
            ));
            std::fs::write(&p, yaml).unwrap();
            ExecArgs {
                file: script.to_string_lossy().into(),
                config: p.to_string_lossy().into(),
                dir: Some(tmp.join("src").to_string_lossy().into()),
                db: None,
                log_file: None,
                args: vec![],
            }
        };
        // 缺省（不写 migrate_on_start）= 全跳过 → 表不存在 → 脚本 throw。
        let e = run(mk(None)).unwrap_err();
        assert!(e.contains("GATE=missing"), "{e}");
        // 显式 auto → apply（含 reconcile 跟随）→ 表存在 → settle 0。
        assert_eq!(run(mk(Some("auto"))).unwrap(), 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
