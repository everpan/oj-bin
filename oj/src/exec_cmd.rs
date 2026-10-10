//! `oj exec` 子命令：直接执行 ts/js 脚本（spec §3.2 流程）。
//!
//! 钉线程模式同 `oj test`（JsRuntime !Send）。扩展顺序钉死：
//! ws_client_extensions → bridge_ext_init(stable) → exec_ext_init(options)
//! （ws 在前：bootstrap.js 静态 import 依赖；exec_ext 最后：覆盖 log 依赖
//! bridge bootstrap 先跑）。
//!
//! 三种入口（互斥）：`file`（执行文件）/ `-e,--code`（内联代码）/ `--repl`（交互式
//! REPL）；三者缺省（裸 `oj exec`，v0.1.55）= 进 REPL。

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions};
use only_js::bridge::OjModuleLoader;
use only_js::bridge::{
    boot_runtime, bridge_ext_init, patch_fs_loaded_sources, ws_client_extensions,
};
use tokio::runtime::Builder as TokioBuilder;

use crate::app::{Backend, ResourceProfiles, assemble_backend};
use crate::args::ExecArgs;
use crate::exec_ext::{ExecOptions, oj_exec_ext_init};
use crate::serve_cmd::load_app_config;

/// exec 执行目标：文件 / 内联代码 / 交互式 REPL（三者互斥；缺省 = REPL）。
pub(crate) enum ExecTarget {
    File(PathBuf),
    Code(String),
    Repl,
}

/// 入口：解析校验 → 钉线程 → 装配后端 → 执行 → 进程退出码。
pub fn run(a: ExecArgs) -> Result<i32, String> {
    let target = match (&a.file, &a.code, a.repl) {
        (Some(_), Some(_), _) => return Err("exec: <file> 与 --code 互斥，二选一".into()),
        (_, _, true) => {
            if a.file.is_some() || a.code.is_some() {
                return Err("exec: --repl 与 <file>/--code 互斥".into());
            }
            ExecTarget::Repl
        }
        (Some(file), None, false) => {
            let p = PathBuf::from(file);
            match p.extension().and_then(|e| e.to_str()) {
                Some("ts") | Some("js") => ExecTarget::File(p),
                _ => return Err(format!("exec: 仅支持 .ts/.js: {}", p.display())),
            }
        }
        (None, Some(code), false) => ExecTarget::Code(code.clone()),
        // 三者缺省（裸 `oj exec`）= REPL（v0.1.55，替代原报错）。
        (None, None, false) => ExecTarget::Repl,
    };
    let (cfg, top, config_dir, dir, _ts, base) = load_app_config(
        a.config.as_deref(),
        a.site.as_deref(),
        a.dir.as_deref(),
        None,
    )?;
    // exec 恒 dev 语义（spec §3.4）：脚本没有 release 形态；dir 仅作 schema 白名单来源。
    // 各资源根 key 的默认 profile 选择（--db/--redis/--blob/--es/--broker/--kafka/--rabbit），
    // 缺省 default；未声明 fail-fast（装配层统一校验）。
    let profiles = ResourceProfiles {
        db: a.db.clone(),
        redis: a.redis.clone(),
        blob: a.blob.clone(),
        es: a.es.clone(),
        broker: a.broker.clone(),
        kafka: a.kafka.clone(),
        rabbit: a.rabbit.clone(),
    };
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
                let backend = assemble_backend(
                    &cfg,
                    &top,
                    &config_dir,
                    &[(dir.clone(), true)],
                    &base,
                    true,
                    &profiles,
                )
                .await?;
                // 迁移门禁（spec §3.1）：exec 缺省全跳过（与 server dev 缺省 auto 相反，
                // 有意不对称）；仅 config 显式写 migrate_on_start 时执行对应项，
                // reconcile 跟随 auto。非法值 fail-fast（与 server 文案一致）。
                if let Some(gate) = cfg.server.migrate_on_start.as_deref() {
                    let stable = backend.stable();
                    let db_key = profiles.db.as_deref().unwrap_or("default");
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
                match target {
                    ExecTarget::Repl => {
                        if std::io::stdin().is_terminal() {
                            // 真终端：rustyline 接管原始终端（方向键/历史/编辑可用）。
                            let editor = rustyline::DefaultEditor::new()
                                .map_err(|e| format!("exec: init repl: {e}"))?;
                            let mut src = RustylineSource { editor };
                            run_repl(&backend, ExecOptions { args, log_file }, &mut src).await
                        } else {
                            // 管道 / 重定向（CI、测试、文件回放）：走普通 BufRead。
                            let stdin = std::io::stdin();
                            run_repl(&backend, ExecOptions { args, log_file }, &mut stdin.lock())
                                .await
                        }
                    }
                    ExecTarget::File(script) => {
                        run_script(&backend, &script, ExecOptions { args, log_file }).await
                    }
                    ExecTarget::Code(code) => {
                        run_code(&backend, &code, ExecOptions { args, log_file }).await
                    }
                }
            })
        })
        .map_err(|e| format!("spawn exec thread: {e}"))?;
    handle
        .join()
        .map_err(|_| "exec thread panicked".to_string())?
}

/// 装配一次性 JsRuntime（与 serve 同源的扩展 / 堆限额）。ext_boot 由各执行路径在
/// 持有 rt 后用 `boot_if_set` 补跑（exec 不走 RuntimePool）。
fn build_runtime(backend: &Backend, options: ExecOptions) -> JsRuntime {
    let stable = backend.stable();
    let loader = stable.loader.clone();
    let module_loader: Option<Rc<dyn deno_core::ModuleLoader>> =
        loader.map(|inner| Rc::new(OjModuleLoader { inner }) as Rc<dyn deno_core::ModuleLoader>);

    let mut extensions = ws_client_extensions();
    extensions.push(bridge_ext_init(stable.clone()));
    extensions.push(oj_exec_ext_init(options));
    // deno_* 扩展以构建机绝对路径声明 JS；进 runtime 前统一换为内嵌源码。
    patch_fs_loaded_sources(&mut extensions);

    // PR-4：与生产 runtime 同源的单 isolate 堆限额（超限 terminate，exec 失败带 OOM 文案）。
    let create_params = stable
        .js_heap_limit
        .map(only_js::bridge::heap_create_params);
    let mut rt = JsRuntime::new(RuntimeOptions {
        extensions,
        module_loader,
        create_params,
        ..Default::default()
    });
    if let Some(limit) = stable.js_heap_limit {
        only_js::bridge::install_heap_limit_callback(&mut rt, limit);
    }
    rt
}

/// 在已装配的 runtime 上加载并执行一段源码（file:// 或合成 specifier）。
async fn eval_module(
    rt: &mut JsRuntime,
    spec: ModuleSpecifier,
    src: String,
    label: &str,
) -> Result<i32, String> {
    let id = rt
        .load_side_es_module_from_code(&spec, src)
        .await
        .map_err(|e| format!("exec: load {label}: {e}"))?;
    let eval = rt.mod_evaluate(id);
    rt.run_event_loop(PollEventLoopOptions::default())
        .await
        .map_err(|e| format!("exec: run {label}: {e}"))?;
    eval.await.map_err(|e| format!("exec: {label}: {e}"))?;
    Ok(0)
}

/// 执行文件：transpile + 版本化 URL 走 side-module（TLA 保真）。
pub(crate) async fn run_script(
    backend: &Backend,
    script: &Path,
    options: ExecOptions,
) -> Result<i32, String> {
    let mut rt = build_runtime(backend, options);
    // ext_boot（持有 rt 后补跑一次；exec 不走 RuntimePool）。
    boot_if_set(&mut rt, backend).await?;
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
    eval_module(
        &mut rt,
        spec,
        format!("{src}\n"),
        &script.display().to_string(),
    )
    .await
}

/// 执行内联代码（-e/--code）：以 TypeScript 语法转译，合成 specifier 走 side-module。
pub(crate) async fn run_code(
    backend: &Backend,
    code: &str,
    options: ExecOptions,
) -> Result<i32, String> {
    let mut rt = build_runtime(backend, options);
    boot_if_set(&mut rt, backend).await?;
    let src = only_js::bridge::transpile::transpile_src(Path::new("oj-eval.ts"), code)
        .map_err(|e| format!("exec: compile --code: {e}"))?;
    let spec = ModuleSpecifier::parse("file:///oj-eval.ts")
        .map_err(|_| "exec: bad eval specifier".to_string())?;
    eval_module(&mut rt, spec, format!("{src}\n"), "--code").await
}

/// REPL 行读取抽象：交互式（tty → rustyline，方向键/历史可用）与管道（BufRead，CI/测试）
/// 共用同一求值循环。`read_line` 自带 prompt 语义——管道实现忽略 prompt，rustyline 实现
/// 显示并接管原始终端（raw mode），方向键/编辑不再回显乱串。
pub(crate) trait LineSource {
    /// 读取一行：Ok(Some(s)) = 行内容；Ok(None) = EOF / Ctrl-C（退出）；Err = I/O 错误。
    fn read_line(&mut self, prompt: &str) -> std::io::Result<Option<String>>;
}

/// 管道 / 测试用的简单实现：直接走 `BufRead`（无行编辑，仅非交互回放）。
impl<R: BufRead> LineSource for R {
    fn read_line(&mut self, _prompt: &str) -> std::io::Result<Option<String>> {
        let mut s = String::new();
        if BufRead::read_line(self, &mut s)? == 0 {
            Ok(None)
        } else {
            Ok(Some(s))
        }
    }
}

/// 交互式实现：rustyline 接管原始终端（raw mode），方向键 / 历史 / 行内编辑正常，
/// 不再把 `\x1b[A` 等转义序列原样回显成乱串。
struct RustylineSource {
    editor: rustyline::DefaultEditor,
}

impl LineSource for RustylineSource {
    fn read_line(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        match self.editor.readline(prompt) {
            Ok(line) => {
                // 记入内存历史（↑/↓ 跨行回溯）；忽略写入失败（如空行 / 容量）。
                let _ = self.editor.add_history_entry(&line);
                Ok(Some(line))
            }
            // Ctrl-D（Eof）或 Ctrl-C（Interrupted）均退出 REPL。
            Err(rustyline::error::ReadlineError::Eof)
            | Err(rustyline::error::ReadlineError::Interrupted) => Ok(None),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

/// 交互式 REPL：逐行求值（同一 isolate，后端全局可用）。变量跨行不自动持久
/// （模块顶层绑定作用域隔离）；需跨行共享时显式挂到 `globalThis`。
pub(crate) async fn run_repl(
    backend: &Backend,
    options: ExecOptions,
    src: &mut dyn LineSource,
) -> Result<i32, String> {
    let mut rt = build_runtime(backend, options);
    boot_if_set(&mut rt, backend).await?;
    println!(
        "oj REPL —— 后端全局（db/kv/blob/.../console/log）已就绪；Ctrl-D / Ctrl-C 退出；↑/↓ 翻历史。"
    );
    let mut n: usize = 0;
    loop {
        let Some(line) = src.read_line("oj> ").map_err(|e| format!("repl: {e}"))? else {
            // EOF（Ctrl-D）/ Ctrl-C。
            println!();
            break;
        };
        let code = line.trim_end_matches(['\n', '\r']);
        if code.trim().is_empty() {
            continue;
        }
        let ts = match only_js::bridge::transpile::transpile_src(Path::new("oj-repl.ts"), code) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("repl: transpile: {e}");
                continue;
            }
        };
        let spec = ModuleSpecifier::parse(&format!("file:///oj-repl/{n}.ts"))
            .map_err(|_| "repl: bad specifier".to_string())?;
        n += 1;
        if let Err(e) = eval_module(&mut rt, spec, format!("{ts}\n"), "repl").await {
            eprintln!("{e}");
        }
    }
    Ok(0)
}

/// ext_boot：exec 直接建 JsRuntime，须在持有 rt 后补跑一次（与 serve 走 RuntimePool 不同）。
async fn boot_if_set(rt: &mut JsRuntime, backend: &Backend) -> Result<(), String> {
    let stable = backend.stable();
    if let Some(spec) = stable.boot.as_deref() {
        boot_runtime(rt, spec)
            .await
            .map_err(|e| format!("ext_boot: {e}"))?;
    }
    Ok(())
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

/// 测试薄封装：内联代码路径（同 `run_script_ext`）。
#[cfg(test)]
pub(crate) async fn run_code_ext(
    backend: &Backend,
    code: &str,
    args: Vec<String>,
    log_file: Option<std::fs::File>,
) -> Result<i32, String> {
    run_code(backend, code, ExecOptions { args, log_file }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Backend, ResourceProfiles, assemble_backend};
    use crate::args::ExecArgs;
    use only_js::config::Config;

    async fn backend_fixture(tmp: &Path) -> Backend {
        let cfg = Config::default();
        assemble_backend(
            &cfg,
            &serde_json::Value::Null,
            tmp,
            &[(tmp.to_path_buf(), true)],
            "/v1/api",
            true,
            &ResourceProfiles::default(),
        )
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
            site: None,
            file: Some(script.to_string_lossy().into()),
            code: None,
            repl: false,
            config: Some(tmp.join("config.yaml").to_string_lossy().into()),
            dir: Some(tmp.to_string_lossy().into()),
            db: None,
            redis: None,
            blob: None,
            es: None,
            broker: None,
            kafka: None,
            rabbit: None,
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
                site: None,
                file: Some(script.to_string_lossy().into()),
                code: None,
                repl: false,
                config: Some(p.to_string_lossy().into()),
                dir: Some(tmp.join("src").to_string_lossy().into()),
                db: None,
                redis: None,
                blob: None,
                es: None,
                broker: None,
                kafka: None,
                rabbit: None,
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

    /// ⑧ 内联代码（-e/--code）：args 注入 + TLA 保真；与文件同款求值路径。
    #[tokio::test(flavor = "current_thread")]
    async fn given_inline_code_when_run_then_args_and_tla_reachable() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t8-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let backend = backend_fixture(&tmp).await;
        // args 注入（与文件同款 globalThis.args）。
        let e = run_code_ext(
            &backend,
            r#"throw new Error("CODEARGS=" + args.join("|"));"#,
            vec!["a".into(), "b c".into()],
            None,
        )
        .await
        .unwrap_err();
        assert!(e.contains("CODEARGS=a|b c"), "{e}");
        // TLA 跑完 event loop → settle 0。
        assert_eq!(
            run_code_ext(
                &backend,
                r#"await Promise.resolve(); globalThis.__c = 1;"#,
                vec![],
                None
            )
            .await
            .unwrap(),
            0
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// ⑨ REPL：管道喂入两行，逐行求值无异常（Ctrl-D / EOF 退出 0）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_piped_lines_when_repl_then_each_line_evaluated() {
        let tmp = std::env::temp_dir().join(format!("oj-exec-t9-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let backend = backend_fixture(&tmp).await;
        let mut input =
            std::io::Cursor::new("console.log('repl-ok-1');\nconsole.log('repl-ok-2');\n");
        assert_eq!(
            run_repl(
                &backend,
                ExecOptions {
                    args: vec![],
                    log_file: None
                },
                &mut input
            )
            .await
            .unwrap(),
            0
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
