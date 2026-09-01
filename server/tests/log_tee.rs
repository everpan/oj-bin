//! 终端镜像（tee）集成测试 —— **独占一个测试二进制**。
//!
//! tee 是**进程级**的：`dup2` 接管 fd 1/2 后，读端与保存的终端 fd 按进程生命周期
//! 泄漏，不做优雅关闭（`server/src/logging.rs` 模块头）。装进单元测试二进制会有
//! 两个后果：
//!
//! 1. 同二进制里其余用例的输出也一起进管道；
//! 2. libtest 最后打印的 `test result:` 汇总行和任何 panic 信息同样进管道，由后台
//!    镜像线程异步写出。进程退出若快过线程刷盘，尾部字节就丢了 —— 表现为
//!    `cargo test --workspace` 的输出里有时整整少一个二进制的汇总行，
//!    于是"测试数"看起来在跳动。
//!
//! 单独放一个二进制后，tee 的影响面只剩这一条用例本身：丢也只是丢这一行的汇总，
//! `server` 单元二进制的输出永远完整。cargo 判失败看的是测试二进制的退出码而非
//! 汇总文本，所以这纯属显示问题，不影响正确性。
//!
//! tee 依赖 unix 的 pipe/dup2 语义，非 unix 平台整个文件不参与编译。

#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use server::logging::install_terminal_tee;

/// 单文件阈值（B）。直接调 install 可绕开 `init` 里 100M 的下限钳制。
const MAX_BYTES: u64 = 64;
/// 保留个数（含活动文件）。
const KEEP: usize = 3;

/// 一行 18B：`rot-00-0123456789\n`。
fn rot_line(i: u32) -> String {
    format!("rot-{i:02}-0123456789\n")
}

#[test]
fn tee_mirrors_and_rotates() {
    // tee 只能装一次 —— 再装一次会把先前的管道误当终端（读写成环）。本二进制只有
    // 这一条用例，但不排除以后会加，故在此写明约束。
    let dir = tempfile::tempdir().unwrap();
    install_terminal_tee(dir.path(), MAX_BYTES, KEEP, true).unwrap();

    // --- 镜像：stdout / stderr 都要落到文件，且 ANSI 已剥离 --------------------
    // 直接写标准句柄：绕过 libtest 的 set_output_capture（宏级捕获到不了 fd 层）。
    let _ = std::io::stdout().write_all(b"TEE-OUT-MARK\n");
    let _ = std::io::stderr().write_all(b"TEE-ERR-MARK\n");
    // 镜像线程异步落盘，轮询等待（终端侧同步可见，文件侧允许小延迟）。
    let mut mirrored = false;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        let all = read_all(dir.path());
        if all.contains("TEE-OUT-MARK") && all.contains("TEE-ERR-MARK") {
            assert!(!all.contains("\x1b["), "ANSI codes must be stripped");
            mirrored = true;
            break;
        }
    }
    assert!(
        mirrored,
        "tee did not mirror stdout/stderr into the log file"
    );

    // --- 滚动：续写跨过阈值 → 活动文件让位为 .1.log，总数钳在 keep --------------
    //
    // 必须等前一行落盘再写下一行：镜像线程一次 read 拿到多少就交给 LogWriter::write
    // 多少，而滚动只在每次 write 的**开头**判定一次。若若干行被合并成一次 write，
    // 就整批落进活动文件、一次都不滚 —— 这是本用例在单元测试二进制里能稳定滚动、
    // 拆出来后却不稳定的原因（原先靠其余 61 条用例的并发输出制造了分块）。
    // 等前一行可见，等价于确认这次 write 已发生，下一行必然是另一次调用。
    //
    // 8 行足够：26B（两个 mark）+ 每 18B 一行，阈值 64B → 第 3、7 行各触发一次滚动，
    // 恰好滚出 .1/.2 两个后移位、凑满 keep=3，且没有任何一行被挤出保留窗口
    // （故上面按行等待时每行都还找得到）。
    for i in 0..8u32 {
        let _ = std::io::stderr().write_all(rot_line(i).as_bytes());
        let needle = format!("rot-{i:02}-");
        let mut seen = false;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(10));
            if read_all(dir.path()).contains(&needle) {
                seen = true;
                break;
            }
        }
        assert!(seen, "line rot-{i:02} never landed in the log file");
    }

    let files: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        files.iter().any(|f| f.ends_with(".1.log")),
        "expected a rotated .1.log, got {files:?}"
    );
    assert_eq!(files.len(), KEEP, "{files:?}");
}

/// 串起目录里全部日志文件（含滚动件）—— 内容被滚走后仍在别的文件里。
fn read_all(dir: &Path) -> String {
    let mut all = String::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        all.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
    }
    all
}
