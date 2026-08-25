//! dynptr 评估 host：加载插件拿 dyn Pinger，断言 roundtrip / FfiFuture / panic 收敛。

use contract::{FfiFuture, Pinger, PingerDyn};
use libloading::Library;
use stabby::string::String as RString;

type BoxedPinger = stabby::dynptr!(stabby::boxed::Box<dyn Pinger + Send + Sync>);

fn wait(mut f: FfiFuture) -> Result<Vec<u8>, String> {
    loop {
        match (f.poll)(f.state) {
            0 => std::thread::yield_now(),
            code => {
                let r = (f.take)(f.state);
                (f.free)(f.state);
                f.state = std::ptr::null_mut();
                return match (code, std::result::Result::from(r)) {
                    (1, Ok(b)) => Ok(b.iter().copied().collect()),
                    (_, Ok(_)) => Err("poll=-1 but take ok".into()),
                    (_, Err(e)) => Err(e[..].to_string()),
                };
            }
        }
    }
}

fn plugin_path() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    p.join("target/debug").join(format!("{prefix}plugin_dynptr.{ext}"))
}

fn main() {
    let lib = unsafe { Library::new(plugin_path()) }.expect("load dynptr plugin");
    let lib: &'static Library = Box::leak(Box::new(lib));
    let make = unsafe { lib.get::<extern "C" fn() -> BoxedPinger>(b"make_pinger").unwrap() };
    let pinger = make();

    assert_eq!(&pinger.ping()[..], "pong");
    println!("dynptr sync call OK");

    let out = wait(pinger.ping_async()).expect("ping_async");
    assert_eq!(out, b"pong-async");
    println!("dynptr + FfiFuture OK");

    match std::result::Result::from(pinger.boom()) {
        Err(e) => assert!(e[..].contains("panic"), "{e}"),
        Ok(_) => panic!("boom must converge to Err"),
    }
    println!("dynptr + panic convergence OK");

    println!("ALL SPIKE S.3 DYNPTR CHECKS PASSED");
    let _ = RString::new(); // silence unused import in some cfgs
}
