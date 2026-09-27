//! TS→JS 转译（deno_ast strip types）+ 全局转译缓存。
//! 缓存按 (path, mtime) 单槽条目：改文件即失效替换，容量天然有界。
//! ponytail: 进程级全局（跨 Bridge/actor 共享）；测试临时目录路径各异，条目随进程消亡。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// 实际发生转译的次数（UC-14 缓存断言用）。
static TRANSPILE_COUNT: OnceLock<std::sync::atomic::AtomicUsize> = OnceLock::new();

fn count() -> &'static std::sync::atomic::AtomicUsize {
    TRANSPILE_COUNT.get_or_init(|| std::sync::atomic::AtomicUsize::new(0))
}

#[doc(hidden)]
pub fn transpile_hits() -> usize {
    count().load(std::sync::atomic::Ordering::Relaxed)
}

type Cache = Mutex<HashMap<PathBuf, (SystemTime, String)>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 读盘 + mtime 缓存 + 转译（.ts）或原文（.js 直读不转译）。
pub fn cached_transpile(path: &Path) -> Result<String, String> {
    let mtime = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if let Some((t, src)) = cache().lock().unwrap().get(path)
        && *t == mtime
    {
        return Ok(src.clone());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let out = if path.extension().is_some_and(|e| e == "ts") {
        transpile_src(path, &raw)?
    } else {
        raw
    };
    cache()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (mtime, out.clone()));
    Ok(out)
}

/// 纯转译：deno_ast 解析 TypeScript → transpile（strip types）。
/// deno_ast 0.53：ParseParams.specifier 为 Url、text 为 Arc<str>，
/// transpile 收三组 options，返回 TranspileResult（into_source().text）。
pub fn transpile_src(path: &Path, src: &str) -> Result<String, String> {
    count().fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // 相对路径造不出 file URL；诊断文本由前缀 path.display() 保证带文件名，回退占位 specifier。
    let specifier = deno_ast::ModuleSpecifier::from_file_path(path)
        .unwrap_or_else(|_| deno_ast::ModuleSpecifier::parse("file:///transpile.ts").unwrap());
    let parsed = deno_ast::parse_module(deno_ast::ParseParams {
        specifier,
        text: src.into(),
        media_type: deno_ast::MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| format!("{}: {e}", path.display()))?;
    let out = parsed
        .transpile(
            &deno_ast::TranspileOptions::default(),
            &deno_ast::TranspileModuleOptions::default(),
            &deno_ast::EmitOptions::default(),
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(out.into_source().text)
}

/// minify：swc_ecma_minifier 全量压缩（DCE + 表达式压缩 + mangle 局部变量名）。
/// 比 codegen 级 minify 压缩率更高，且函数内局部绑定被重命名（基础混淆）；
/// mangle top_level=false 保住顶层/导出名（跨文件 import 与 routes.js file 字段不受影响），
/// `json`/`db`/`http` 等注入全局是属性访问、不是绑定，天然不被碰。确定性：同输入同输出。
/// ponytail: 字符串数组/控制流平坦化等强混淆不做（javascript-obfuscator 量级），
/// 需要 `--obfuscate` 档时再议（Node 子进程或自写 swc pass）。
pub fn minify_js(path: &Path, src: &str) -> Result<String, String> {
    use deno_ast::swc::common::{FileName, GLOBALS, Globals, Mark, SourceMap, sync::Lrc};
    use deno_ast::swc::ecma_visit::VisitMutWith;
    use deno_ast::swc::parser::{Parser, StringInput, Syntax, lexer::Lexer};
    use swc_ecma_minifier::option::{CompressOptions, ExtraOptions, MangleOptions, MinifyOptions};

    // Mark/SyntaxContext 依赖线程局部 GLOBALS（swc 记号分配）；按 swc 惯例包一层。
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let cm: Lrc<SourceMap> = Lrc::new(SourceMap::default());
        let fm = cm.new_source_file(
            FileName::Real(path.to_path_buf()).into(),
            // 相对路径造不出 file URL 之类的顾虑此处无关；src 原样进 SourceMap（mangle
            // 的字符频率分析读它，确定性来源之一）。
            String::from(src),
        );
        let comments = deno_ast::swc::common::comments::SingleThreadedComments::default();
        let lexer = Lexer::new(
            Syntax::Es(Default::default()),
            deno_ast::swc::ast::EsVersion::Es2022,
            StringInput::from(&*fm),
            Some(&comments),
        );
        let mut parser = Parser::new_from(lexer);
        let mut program = parser
            .parse_program()
            .map_err(|e| format!("{}: {:?}", path.display(), e))?;

        // resolver 打作用域记号：区分「本文件顶层绑定」与「外部未解析引用」——
        // mangle 只动前者，导入导出与全局引用的名字由此保住。
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        program.visit_mut_with(&mut deno_ast::swc::transforms::resolver(
            unresolved_mark,
            top_level_mark,
            false,
        ));

        let program = swc_ecma_minifier::optimize(
            program,
            cm,
            Some(&comments),
            None,
            &MinifyOptions {
                compress: Some(CompressOptions::default()),
                mangle: Some(MangleOptions {
                    top_level: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &ExtraOptions {
                unresolved_mark,
                top_level_mark,
                mangle_name_cache: None,
            },
        );

        // codegen minify 重排成单行、剥全部注释（含内联 sourcemap——顺带消掉
        // 绝对路径与整份源码副本）。optimize 已改写 AST，这里只做最终落字。
        // 位置仅在记 srcmap 时回查（此处不记），空 SourceMap 即可。
        let cm = Lrc::new(SourceMap::default());
        let mut buf = vec![];
        {
            use deno_ast::swc::codegen::Node;
            let mut emitter = deno_ast::swc::codegen::Emitter {
                cfg: deno_ast::swc::codegen::Config::default().with_minify(true),
                comments: None,
                cm: cm.clone(),
                wr: Box::new(deno_ast::swc::codegen::text_writer::JsWriter::new(
                    cm, "\n", &mut buf, None,
                )),
            };
            match program {
                deno_ast::swc::ast::Program::Module(m) => m.emit_with(&mut emitter),
                deno_ast::swc::ast::Program::Script(s) => s.emit_with(&mut emitter),
            }
            .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        String::from_utf8(buf).map_err(|e| format!("{}: {e}", path.display()))
    })
}

/// 计数器为进程全局，测试并行跑会互相污染 delta 断言——
/// 所有会触发 .ts 转译的测试（本组 + bridge 模块测试的 run_module 路径）共用此锁串行。
/// （不能复用 cache 锁：cached_transpile 内部已持有，重入会死锁。）
#[cfg(test)]
pub(crate) static TRANSPILE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_type_annotations() {
        let _g = TRANSPILE_TEST_LOCK.lock().unwrap();
        let out = transpile_src(Path::new("a.ts"),
            "const x: number = 1;\nfunction f(a: string): string { return a; }\nexport default 1;\n").unwrap();
        assert!(out.contains("const x = 1;"), "{out}");
        assert!(out.contains("return a;"), "{out}");
        assert!(!out.contains(": number"), "{out}");
    }

    #[test]
    fn syntax_error_has_position() {
        let _g = TRANSPILE_TEST_LOCK.lock().unwrap();
        let e = transpile_src(Path::new("bad.ts"), "function {{{{").unwrap_err();
        assert!(e.contains("bad.ts"), "{e}");
    }

    #[test]
    fn minify_is_single_line_and_strips_comments() {
        let src = "// 注释\nimport { v } from \"./a.js\";\nfunction get() { json.ok({ v }); }\nget.route = \"{id}\";\nexport default { get };\n//# sourceMappingURL=data:application/json;base64,AAA\n";
        let out = minify_js(Path::new("m.js"), src).unwrap();
        assert!(!out.trim_end().contains('\n'), "{out}"); // 单行（允尾换行）
        assert!(!out.contains("//"), "{out}"); // 注释全剥（含 sourcemap 行）
        assert!(out.contains("from\"./a.js\""), "{out}"); // 语句/空白压缩
        assert!(
            out.contains("json.ok({v:") || out.contains("json.ok({ v:"),
            "{out}"
        ); // 局部导入绑定 v 被 mangle 成短名（属性名 v 保留）
        // 同输入同输出（构建确定性依赖）
        assert_eq!(minify_js(Path::new("m.js"), src).unwrap(), out);
    }

    #[test]
    fn cache_hits_on_second_call_same_mtime() {
        let _g = TRANSPILE_TEST_LOCK.lock().unwrap();
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "oj-tr-{}-{}.ts",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&p, "const a: number = 1;\n").unwrap();
        let before = transpile_hits();
        let s1 = cached_transpile(&p).unwrap();
        let s2 = cached_transpile(&p).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(transpile_hits(), before + 1, "second call must hit cache");
        // 内容变更 → mtime 变 → 重转译。
        // 不用 sleep 干等 mtime 自己跳一跳：NTFS 的 LastWriteTime 约 15.6ms 一跳，
        // 固定 sleep 落在边界上就是 flaky。改成写完后显式把 mtime 推后，让失效
        // 判定的输入是确定的（set_modified 必须在 write 之后，否则被 write 覆盖）。
        std::fs::write(&p, "const b: number = 2;\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(1))
            .unwrap();
        let s3 = cached_transpile(&p).unwrap();
        assert!(s3.contains("const b"), "{s3}");
        assert_eq!(transpile_hits(), before + 2);
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod tla_probe_tests {
    /// TS 转译对顶层 await 的保真性（任务驱动依赖，评审 F3）。
    #[test]
    fn ts_transpile_preserves_top_level_await() {
        // 本测试也触发 .ts 转译 → 持 TRANSPILE_TEST_LOCK（模块注释：所有转译路径共用
        // 此锁串行）。此前漏掉：并发给全局计数器 +1，污染 cache 测试的 delta 断言
        // （偶发 16≠17），panic 时还毒化锁连锁炸掉同组测试。
        let _g = super::TRANSPILE_TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("ojtla-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("probe.ts");
        std::fs::write(&p, "export {};\nwhile (!tasks.stopping()) { await Kafka(\"default\").poll([\"t\"], { timeoutMs: 30 }); }\n").unwrap();
        let out = super::cached_transpile(&p).unwrap();
        assert!(out.contains("await"), "TLA lost: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
