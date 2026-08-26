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

/// module 白名单：非空、[A-Za-z0-9_-]，禁路径字符与 ..（进路径拼接，信任边界）。
pub fn validate_module(m: &str) -> Result<(), String> {
    let ok = !m.is_empty()
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!("illegal module name {m:?}"))
    }
}

/// version 白名单：非空、[A-Za-z0-9.]，拒连续点（兼容 0.1.0-beta 缺横线也放行——含 '-'）。
pub fn validate_version(v: &str) -> Result<(), String> {
    let ok = !v.is_empty()
        && !v.contains("..")
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!("illegal version {v:?}"))
    }
}

/// 单个 manifest.yaml → Manifest（load_modules 复用此解析，DRY）。
pub fn parse_one(path: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    serde_yaml::from_str(&text).map_err(|e| format!("parse {path:?}: {e}"))
}

/// dist/manifests.yaml：模块 → 锁定版本。缺失 = 空表（首次构建合法）；
/// 其余读错 / 类型错 / 解析错 = Err（坏锁不得被当空表静默重置）。
pub fn load_lock(path: &Path) -> Result<std::collections::BTreeMap<String, String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_yaml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// 原子写（tmp + rename）。ponytail: 多进程并发构建的读-改-写竞争不做锁，标 ceiling。
pub fn save_lock(
    path: &Path,
    lock: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let yaml = serde_yaml::to_string(lock).map_err(|e| format!("serialize lock: {e}"))?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, yaml).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
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
        let m = parse_one(&mf)?;
        if m.name != dirname {
            return Err(format!(
                "manifest name {:?} != directory name {:?} (in {})",
                m.name,
                dirname,
                p.display()
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
        write(
            d.0.join("user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        );
        let ms = load_modules(&d.0).unwrap();
        assert_eq!(ms[0].name, "user");

        let bad = tmp("mf-bad");
        write(
            bad.0.join("order/manifest.yaml"),
            "name: orderr\ndesc: d\nversion: 0.1.0\n",
        );
        let e = load_modules(&bad.0).unwrap_err();
        assert!(e.contains("orderr") && e.contains("order"), "{e}");

        let none = tmp("mf-none");
        write(none.0.join("x/keep.txt"), "");
        let e2 = load_modules(&none.0).unwrap_err();
        assert!(e2.contains("manifest.yaml"), "{e2}");
    }

    #[test]
    fn whitelists_module_and_version() {
        assert!(validate_module("user").is_ok());
        assert!(validate_module("user_2").is_ok());
        for bad in ["", "../x", "a/b", "a\\b", "a b", ".."] {
            assert!(validate_module(bad).is_err(), "{bad}");
        }
        assert!(validate_version("0.1.0").is_ok());
        assert!(validate_version("0.1.0-beta").is_ok());
        for bad in ["", "0..1", "../../etc", "a/b", "a\\b", "a b"] {
            assert!(validate_version(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lock_roundtrip_and_atomic_save() {
        let d = tmp("lock");
        let p = d.0.join("manifests.yaml");
        let mut m = std::collections::BTreeMap::new();
        m.insert("user".into(), "0.1.0".into());
        m.insert("order".into(), "0.2.0".into());
        save_lock(&p, &m).unwrap();
        assert_eq!(load_lock(&p).unwrap(), m);
        // upsert 语义由调用方实现：save 前合并
        m.insert("user".into(), "0.2.0".into());
        save_lock(&p, &m).unwrap();
        assert_eq!(load_lock(&p).unwrap()["user"], "0.2.0");
        // 序列化确定性（同输入同字节）
        save_lock(&p, &m).unwrap();
        let b1 = std::fs::read(&p).unwrap();
        save_lock(&p, &m).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b1);
        // 缺失 = 空表（首次构建合法）
        assert!(load_lock(&d.0.join("none.yaml")).unwrap().is_empty());
        // yaml 非 map 类型
        std::fs::write(&p, "- a\n").unwrap();
        assert!(load_lock(&p).is_err());
    }

    #[test]
    fn parse_one_reads_single_manifest() {
        let d = tmp("one");
        write(d.0.join("m.yaml"), "name: user\ndesc: d\nversion: 0.1.0\n");
        let m = parse_one(&d.0.join("m.yaml")).unwrap();
        assert_eq!((m.name.as_str(), m.version.as_str()), ("user", "0.1.0"));
        assert!(parse_one(&d.0.join("none.yaml")).is_err());
    }
}
