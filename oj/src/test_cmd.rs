//! `oj test` 子命令：进程内真实 deno_core 运行时跑 sample/tests/*.test.ts。
//!
//! 钉线程模型（修正 #9）：dedicated OS 线程 + `current_thread().enable_all()` tokio runtime；
//! `JsRuntime` 是 `!Send`，不跨线程 spawn。extensions = [bridge_ext（真实全局 + StableState），
//! oj_test_ext（client 全局 + 测试框架 + op_client_dispatch）]；OpState 注入
//! `Arc<dyn ClientTransport>`（即 App）。逐个加载 *.test.ts 注册用例 → `__runTests()`
//! 跑全部 → 读 `__testSummary` 打印 TAP/vitest 风格摘要。返回退出码（修正 #13：不在运行时内 exit）。

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use deno_core::{JsRuntime, ModuleLoader, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};
use mdm_base_rust::bridge::bridge_ext;
use mdm_base_rust::bridge::OjModuleLoader;
use tokio::runtime::Builder as TokioBuilder;

use crate::app::{App, ClientTransport};
use crate::args::TestArgs;
use crate::server_cmd::load_app_config;
use crate::test_ext::oj_test_ext;

/// Rust 侧测试结果汇总（serde_v8 从 JS `__testSummary` 反序列化）。
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct TestSummary {
    total: usize,
    passed: usize,
    failed: usize,
    tests: Vec<TestResult>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TestResult {
    suite: Option<String>,
    name: String,
    ok: bool,
    error: Option<String>,
}

/// 入口：解析配置 → 钉线程起 runtime → 跑用例 → 返回退出码。
pub fn run(a: TestArgs) -> Result<i32, String> {
    let (cfg, config_dir, dir, ts, base) =
        load_app_config(&a.config, a.dir.as_deref(), a.base.as_deref())?;

    // 测试用例目录：绝对路径原样；相对 → 相对 config_dir（项目根）。
    let tests_dir = a.tests.clone().unwrap_or_else(|| "tests".into());
    let tests_path = if Path::new(&tests_dir).is_absolute() {
        PathBuf::from(&tests_dir)
    } else {
        config_dir.join(&tests_dir)
    };
    // ModuleSpecifier::from_file_path 要求绝对路径；config_dir 可能是相对路径，故绝对化。
    let tests_path = std::fs::canonicalize(&tests_path)
        .map_err(|e| format!("tests dir not found: {} ({e})", tests_path.display()))?;

    // 收集 *.test.ts（按名排序，稳定顺序）。
    let mut files: Vec<PathBuf> = std::fs::read_dir(&tests_path)
        .map_err(|e| format!("read {}: {e}", tests_path.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().map(|x| x == "ts").unwrap_or(false)
                && p.file_name()
                    .map(|n| n.to_string_lossy().ends_with(".test.ts"))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no *.test.ts found in {}", tests_path.display()));
    }

    // 钉线程：JsRuntime 是 !Send，必须待在同一 OS 线程。
    let handle = std::thread::Builder::new()
        .name("oj-test".into())
        .spawn(move || {
            let rt = TokioBuilder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("test runtime: {e}"))?;
            rt.block_on(async move {
                let app = App::from_config(cfg, &config_dir, dir, base, ts).await?;
                run_on_runtime(app, &files).await
            })
        })
        .map_err(|e| format!("spawn test thread: {e}"))?;

    handle
        .join()
        .map_err(|_| "test thread panicked".to_string())?
}

/// 在专用 runtime 上加载并跑全部测试文件。
async fn run_on_runtime(app: App, files: &[PathBuf]) -> Result<i32, String> {
    let stable = app.stable();
    let loader = stable.loader.clone();
    let module_loader: Option<Rc<dyn ModuleLoader>> = loader.map(|inner| {
        Rc::new(OjModuleLoader { inner }) as Rc<dyn ModuleLoader>
    });

    let mut rt = JsRuntime::new(RuntimeOptions {
        extensions: vec![bridge_ext::init(stable.clone()), oj_test_ext::init()],
        module_loader,
        ..Default::default()
    });

    // 注入 ClientTransport（App 自身）。op_client_dispatch 经 OpState 取用。
    rt.op_state()
        .borrow_mut()
        .put(Arc::new(app) as Arc<dyn ClientTransport>);

    // 逐个加载 *.test.ts（side esm），执行其顶层 describe/it 完成用例注册。
    let mut seq: u64 = 0;
    for f in files {
        let spec = ModuleSpecifier::from_file_path(f)
            .map_err(|_| format!("bad test path: {}", f.display()))?;
        let code = format!("await import(\"{spec}\");\n");
        let driver_spec = ModuleSpecifier::parse(&format!("file:///oj/test/{}.js", seq))
            .map_err(|e| e.to_string())?;
        seq += 1;
        let id = rt
            .load_side_es_module_from_code(&driver_spec, code)
            .await
            .map_err(|e| format!("load test {}: {e}", f.display()))?;
        let eval = rt.mod_evaluate(id);
        rt.run_event_loop(PollEventLoopOptions::default())
            .await
            .map_err(|e| format!("run test {}: {e}", f.display()))?;
        eval.await
            .map_err(|e| format!("test {}: {e}", f.display()))?;
    }

    // 运行全部已注册用例（client 调用在此触发真实 oneshot 派发）。
    let run_spec =
        ModuleSpecifier::parse("file:///oj/test/__run.js").map_err(|e| e.to_string())?;
    let run_code = "await globalThis.__runTests();\n";
    let id = rt
        .load_side_es_module_from_code(&run_spec, run_code)
        .await
        .map_err(|e| format!("run tests: {e}"))?;
    let eval = rt.mod_evaluate(id);
    rt.run_event_loop(PollEventLoopOptions::default())
        .await
        .map_err(|e| format!("run tests event loop: {e}"))?;
    eval.await.map_err(|e| format!("run tests eval: {e}"))?;

    // 读 __testSummaryJson（JS 对象 → JSON 字符串 → serde_json 反序列化）。
    // serde_v8::from_v8 / to_rust_string_lossy 都要求 context-bound scope
    // （PinnedRef<HandleScope<Context>>），故按 deno_core scope! 模式构建 ContextScope
    // （修正 #14：避免在 contextless scope 上调用需要 context 的 API）。
    let global = rt
        .execute_script("read_summary", "globalThis.__testSummaryJson")
        .map_err(|e| format!("read summary: {e}"))?;
    let context_global = rt.main_context();
    let isolate = &mut *rt.v8_isolate();
    let mut handle_scope = v8::HandleScope::new(isolate);
    let mut handle_scope = {
        let p = unsafe { std::pin::Pin::new_unchecked(&mut handle_scope) };
        p.init()
    };
    let handle_scope = &mut handle_scope;
    let context = v8::Local::new(handle_scope, context_global);
    let context_scope = v8::ContextScope::new(handle_scope, context);
    let local = v8::Local::<v8::Value>::new(&context_scope, global);
    let json = local.to_rust_string_lossy(&context_scope);
    let summary: TestSummary =
        serde_json::from_str(&json).map_err(|e| format!("deserialize summary: {e}"))?;

    // 打印 TAP / vitest 风格摘要。
    println!("oj test: {} files, {} tests", files.len(), summary.total);
    for t in &summary.tests {
        let tag = if t.ok { "ok  " } else { "FAIL" };
        let suite = t.suite.as_deref().unwrap_or("");
        println!("  {tag}  {suite} > {}", t.name);
        if let Some(err) = &t.error {
            for line in err.lines() {
                println!("        {line}");
            }
        }
    }
    println!("result: {}/{} passed", summary.passed, summary.total);
    Ok(if summary.failed > 0 { 1 } else { 0 })
}
