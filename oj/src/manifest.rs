//! manifest.yaml：模块清单（name 必须等于目录名——启动期强约束）。

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub desc: String,
    pub version: String,
    #[serde(default)]
    pub config: serde_yaml::Value,
}

/// 加载 dir 首层全部模块清单并校验 name==目录名。
pub fn load_modules(dir: &Path) -> Result<Vec<Manifest>, String> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // 模块目录缺失 = 无模块（空即合法；目录在而缺 manifest.yaml 仍是错误）。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(format!("read module dir {}: {e}", dir.display())),
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let dirname = e.file_name().to_string_lossy().into_owned();
        let mf = p.join("manifest.yaml");
        if !mf.is_file() {
            return Err(format!("module '{dirname}' missing manifest.yaml"));
        }
        let m: Manifest = serde_yaml::from_str(
            &std::fs::read_to_string(&mf).map_err(|e| format!("read {mf:?}: {e}"))?,
        )
        .map_err(|e| format!("parse {mf:?}: {e}"))?;
        if m.name != dirname {
            return Err(format!(
                "manifest name {:?} != directory name {:?} (in {})",
                m.name, dirname, p.display()
            ));
        }
        out.push(m);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    fn tmp(tag: &str) -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let d = std::env::temp_dir().join(format!(
            "oj-mf-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }
    fn write(p: PathBuf, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loads_and_validates() {
        let d = tmp("mf-ok");
        write(d.0.join("user/manifest.yaml"), "name: user\ndesc: d\nversion: 0.1.0\n");
        let ms = load_modules(&d.0).unwrap();
        assert_eq!(ms[0].name, "user");

        let bad = tmp("mf-bad");
        write(bad.0.join("order/manifest.yaml"), "name: orderr\ndesc: d\nversion: 0.1.0\n");
        let e = load_modules(&bad.0).unwrap_err();
        assert!(e.contains("orderr") && e.contains("order"), "{e}");

        let none = tmp("mf-none");
        write(none.0.join("x/keep.txt"), "");
        let e2 = load_modules(&none.0).unwrap_err();
        assert!(e2.contains("manifest.yaml"), "{e2}");
    }
}
