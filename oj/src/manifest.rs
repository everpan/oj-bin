//! manifest.yaml：模块清单。
//!
//! **两种发现口径**（分支只在此文件与 `bridge::module_scan`）：
//! - 无索引（今日默认）：`dir` 首层子目录 = 模块根，且 `name` 必须等于目录名；
//! - 有索引（`--module-root`）：模块根由索引给出，`name` 取 `manifest.yaml` 的 name，
//!   目录名只作定位。索引由装配期一次性构造（`bridge::resolve_roots`），dev/serve/
//!   build/migrate 共用同一份——「哪个目录是模块」不允许有两处判定。

use serde::Deserialize;
use std::path::{Path, PathBuf};

use only_js::bridge::ModuleIndex;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub desc: String,
    pub version: String,
    #[serde(default)]
    pub config: serde_yaml::Value,
    /// 绑定命名库（§5.3）：模块字面 "default" 的 db 调用重定向到该库；None = 用 default。
    #[serde(default)]
    pub db: Option<String>,
    /// 本模块拥有的表名（schema.yaml 存在时冗余校验：不一致 fail-fast；§5.2）。
    #[serde(default)]
    pub tables: Vec<String>,
    /// 依赖模块（键 = 模块名）：归属守卫放行依据 + S004 依赖存在性检查。
    #[serde(default)]
    pub deps: std::collections::HashMap<String, String>,
}

/// name / version 白名单：与运行期发现（`bridge::module_scan`）**同一份实现**——
/// 两处各写一份必然漂移，而 name 要进产物目录名与锁键，错一处就是路径穿越面。
pub use only_js::bridge::{validate_module, validate_version};

/// 索引是否覆盖 `dir`：多挂载下每棵树各自判定（`--module-root` 只指定了一棵树，
/// 索引不能越树生效——否则第二棵树的首层目录会被误判成「索引里的模块」）。
pub(crate) fn covers<'a>(index: Option<&'a ModuleIndex>, dir: &Path) -> Option<&'a ModuleIndex> {
    let idx = index?;
    let c = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    (only_js::bridge::strip_verbatim(&c) == idx.base).then_some(idx)
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

/// 加载模块清单：`index` 覆盖 `dir` 时按索引（模块根可不在首层、name 取 manifest）；
/// 否则今日语义（首层子目录 = 模块根，校验 name==目录名）。
/// `skip`：保留目录名（如 tasks.dir 的任务池——非模块）；None = 不跳过。
pub fn load_modules(
    dir: &Path,
    skip: Option<&str>,
    index: Option<&ModuleIndex>,
) -> Result<Vec<Manifest>, String> {
    if let Some(idx) = covers(index, dir) {
        let mut out = Vec::new();
        for e in idx.modules.values() {
            out.push(parse_one(&e.root.join("manifest.yaml"))?);
        }
        return Ok(out);
    }
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
        // 保留目录（任务池，spec 2026-09-07 §6/T10）——非模块，由 tasks 监督器/
        // build 镜像单独处理；其余目录缺 manifest.yaml 仍是错误。
        if Some(dirname.as_str()) == skip {
            continue;
        }
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

/// 模块发现（迁移门禁 / fixture / `oj migrate` 共用）：`index` 覆盖 `dir` 时按索引；
/// 否则 dev（ts）= 首层子目录（load_modules 已保证 name==目录名）；release = 锁
/// `dist/manifests.yaml` {m: v} → `<dir>/<m>-<v>/`。返回 (模块名, 模块目录)，按模块名排序。
pub fn discover(
    dir: &Path,
    ts: bool,
    index: Option<&ModuleIndex>,
) -> Result<Vec<(String, PathBuf)>, String> {
    if let Some(idx) = covers(index, dir) {
        return Ok(idx
            .modules
            .iter()
            .map(|(n, e)| (n.clone(), e.root.clone()))
            .collect());
    }
    if ts {
        // 任务池保留目录按约定默认跳过（tasks.dir 改名 + migrate 的组合不识别——
        // build/serve 路径已按配置名跳过；ponytail: 命名场景把 migrate 也接配置再补）。
        let mut out: Vec<_> = load_modules(dir, Some("tasks"), None)?
            .into_iter()
            .map(|m| {
                let p = dir.join(&m.name);
                (m.name, p)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    } else {
        load_lock(&dir.join("manifests.yaml"))?
            .into_iter()
            .map(|(m, v)| {
                let p = dir.join(format!("{m}-{v}"));
                if !p.is_dir() {
                    return Err(format!(
                        "lock points to missing version dir: {}（先 oj build）",
                        p.display()
                    ));
                }
                Ok((m, p))
            })
            .collect()
    }
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
        let ms = load_modules(&d.0, None, None).unwrap();
        assert_eq!(ms[0].name, "user");

        let bad = tmp("mf-bad");
        write(
            bad.0.join("order/manifest.yaml"),
            "name: orderr\ndesc: d\nversion: 0.1.0\n",
        );
        let e = load_modules(&bad.0, None, None).unwrap_err();
        assert!(e.contains("orderr") && e.contains("order"), "{e}");

        let none = tmp("mf-none");
        write(none.0.join("x/keep.txt"), "");
        let e2 = load_modules(&none.0, None, None).unwrap_err();
        assert!(e2.contains("manifest.yaml"), "{e2}");
    }

    /// BDD：索引覆盖时，模块身份取 `manifest.yaml` 的 name，目录名只作定位——
    /// 发现结果必须与「哪个目录是模块」的唯一判据（索引）一致。
    #[test]
    fn given_index_covers_dir_when_load_modules_then_names_come_from_manifest() {
        // Given: 模块根在第二层，目录名与模块名无关
        let d = tmp("mf-index");
        write(
            d.0.join("foo/api/manifest.yaml"),
            "name: m1\ndesc: d\nversion: 0.1.0\n",
        );
        write(
            d.0.join("bar/api/manifest.yaml"),
            "name: m2\ndesc: d\nversion: 0.2.0\n",
        );
        let idx = only_js::bridge::resolve_roots(&d.0, true, None).unwrap();

        // When: 按索引加载
        let mut ms = load_modules(&d.0, None, Some(&idx)).unwrap();
        ms.sort_by(|a, b| a.name.cmp(&b.name));

        // Then: 身份与版本都来自 manifest（目录名不出现）
        assert_eq!(
            ms.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
        assert_eq!(ms[1].version, "0.2.0");
    }

    /// BDD：索引**不覆盖**该树（多挂载下另一棵树）时不得生效——否则那棵树的首层
    /// 目录会被误判成「索引里的模块」。
    #[test]
    fn given_index_of_another_tree_when_load_modules_then_today_semantics() {
        let d = tmp("mf-other");
        write(
            d.0.join("user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        );
        let other = tmp("mf-other-idx");
        write(
            other.0.join("x/api/manifest.yaml"),
            "name: m1\ndesc: d\nversion: 0.1.0\n",
        );
        let idx = only_js::bridge::resolve_roots(&other.0, true, None).unwrap();

        // When: 用别的树的索引加载本树
        let ms = load_modules(&d.0, None, Some(&idx)).unwrap();

        // Then: 走今日语义（name == 目录名校验仍在）
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].name, "user");
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

    #[test]
    fn discover_dev_and_release_layouts() {
        let d = tmp("disc");
        // dev：首层目录（name==目录名），按名排序。
        write(
            d.0.join("user/manifest.yaml"),
            "name: user\ndesc: d\nversion: 0.1.0\n",
        );
        write(
            d.0.join("order/manifest.yaml"),
            "name: order\ndesc: d\nversion: 0.1.0\n",
        );
        let ms = discover(&d.0, true, None).unwrap();
        assert_eq!(
            ms.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec!["order", "user"]
        );
        // release：锁 → 版本目录；锁指向缺失目录 → Err。
        write(
            d.0.join("dist/order-0.1.0/manifest.yaml"),
            "name: order\ndesc: d\nversion: 0.1.0\n",
        );
        std::fs::write(
            d.0.join("dist/manifests.yaml"),
            "order: 0.1.0\nghost: 9.9.9\n",
        )
        .unwrap();
        let e = discover(&d.0.join("dist"), false, None).unwrap_err();
        assert!(e.contains("ghost-9.9.9"), "{e}");
        std::fs::write(d.0.join("dist/manifests.yaml"), "order: 0.1.0\n").unwrap();
        let ms = discover(&d.0.join("dist"), false, None).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].0, "order");
        assert!(ms[0].1.ends_with("order-0.1.0"));
    }
}
