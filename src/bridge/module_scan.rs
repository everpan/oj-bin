//! 模块索引：源码树根 → 模块根（含 `manifest.yaml` 的目录）→ 模块身份与 URL 前缀。
//!
//! 职责边界（SRP）：本模块**只回答两个问题**——「这棵树里有哪些模块、各自根在哪」与
//! 「某个文件属于哪个模块、在模块内的相对路径是什么」。它不解析 handler、不建路由、
//! 不做构建；消费方一律只依赖 `ModuleIndex` 的查询面（`get` / `locate` / `names`），
//! 不自己拼 `base.join(name)`——「模块根可能不在首层」因此只在本文件内部存在。
//!
//! 两种发现模式由调用方选择（OCP：新增布局 = 新增一种扫描口径，消费端零改动）：
//! - `recursive = false`：今日语义（`-d` 首层子目录 = 模块根，且 `name` 必须等于目录名）；
//! - `recursive = true`：`--module-root`，在源码树内递归找 `manifest.yaml`，**命中即停**，
//!   模块名一律取 `manifest.yaml` 的 `name`。
//!
//! **两个名字，两种用途**（互不混用）：
//! - `name`：模块身份。产物 `dist/<name>-<version>/`、锁键、tgz 名、`deps` 声明、
//!   `#/m/x` 别名首段、`oj build <MODULE>` 位置参数都用 `name`。
//! - `prefix`：URL 前缀（可选）。`manifest.yaml` 未声明或声明为空串时等于 `name`。
//!   路由形态恒为 `{base_url}/{prefix}/{模块根内相对路径}`。

use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use super::module_loader::strip_verbatim;

/// 递归扫描的深度上限：防巨树遍历与符号链接成环。`src/<m>/api` 只到 2 层。
const MAX_DEPTH: usize = 8;
/// 命中模块根数量上限（异常布局 / 误指到仓库根的兜底）。
const MAX_ROOTS: usize = 1024;

/// manifest.yaml 的**发现期字段**（完整结构在 `oj/src/manifest.rs`；此处只取定位所需项，
/// 避免把 CLI 侧的完整清单结构上移到运行期 crate）。
#[derive(Debug, Deserialize)]
struct ManifestHead {
    name: String,
    version: String,
    /// 可选 URL 前缀；缺失或空串 → 等于 `name`（`resolve_prefix` 归一）。
    #[serde(default)]
    prefix: Option<String>,
}

/// URL 前缀归一：未声明 / 空串 → 用模块名；否则去首尾空白后原样（已过白名单）。
pub fn resolve_prefix<'a>(prefix: Option<&'a str>, name: &'a str) -> &'a str {
    match prefix.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => p,
        None => name,
    }
}

/// 读 manifest.yaml 的发现期字段：`(name, version, 声明的 prefix)`。
/// prefix 未归一（空串与缺失同义），调用方经 `resolve_prefix` 取得最终值。
pub fn manifest_head(path: &Path) -> Result<(String, String, Option<String>), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let h: ManifestHead =
        serde_yaml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok((h.name, h.version, h.prefix))
}

/// 模块名白名单：非空、[A-Za-z0-9_-]，禁路径字符与 `..`（进路径拼接，信任边界）。
/// `name` 决定产物目录 `dist/<name>-<version>/`、锁键、tgz 名，故在发现期即校验。
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

/// 版本白名单：非空、[A-Za-z0-9.]，拒连续点（兼容 0.1.0-beta 缺横线也放行——含 '-'）。
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

/// URL 前缀白名单：空串（= 用模块名）合法；非空时允许 `/` 分段（如 `sys/user`），
/// 每段 `[A-Za-z0-9_-]`，禁空段 / `.` / `..` / `\` / 首尾斜杠——前缀进路由 pattern，
/// 不得借路径段逃逸挂载根。
pub fn validate_prefix(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Ok(());
    }
    let bad_seg = p
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == ".." || !s.chars().all(is_prefix_char));
    if p.contains('\\') || bad_seg {
        return Err(format!("illegal url prefix {p:?}"));
    }
    Ok(())
}

fn is_prefix_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 单个模块（模块根 + manifest 声明的版本 + 最终 URL 前缀）。
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    /// 模块根（含 `manifest.yaml` 的目录）。
    pub root: PathBuf,
    /// `manifest.yaml` 的 version。
    pub version: String,
    /// 最终 URL 前缀（`prefix` 声明非空即用之，否则等于模块名）。
    pub prefix: String,
}

/// 模块索引。构造后不可变，可 `Arc` 共享给运行期与构建期（同源保证 dev/release 一致）。
#[derive(Debug, Clone, Default)]
pub struct ModuleIndex {
    /// 源码树根（canonical + 去 verbatim 前缀）。
    pub base: PathBuf,
    /// 模块名（`manifest.yaml` 的 name）→ 模块。`BTreeMap`：遍历顺序确定。
    pub modules: BTreeMap<String, ModuleEntry>,
}

impl ModuleIndex {
    /// 按模块名取模块（别名 `#/m/x`、`oj build <MODULE>` 与迁移的定位入口）。
    pub fn get(&self, name: &str) -> Option<&ModuleEntry> {
        self.modules.get(name)
    }

    /// 模块名（已排序）。
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.modules.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// 文件归属（最长前缀）：落在哪个模块根下，以及**模块内的相对路径**。
    /// 返回 `(模块名, 模块, 模块内相对路径)`。不在任何模块内（tasks 池、tests 目录）→ None。
    ///
    /// 最长前缀而非首段：模块根深度可配置后，「相对源码树根的首段 = 模块名」不再成立
    /// （`src/foo/api/x.ts` 的首段是 `foo`，而模块名可能是 `m1`）。
    pub fn locate(&self, file: &Path) -> Option<(&str, &ModuleEntry, PathBuf)> {
        let plain = strip_verbatim(file);
        // 兜底：调用方给的可能是只绝对化未 canonical 的路径（dev 路由装配即此形态），
        // 归一后再比一次，避免同一条路径因形态不同归属不上。
        // 归一父目录而非文件本身：调用方可能拿尚不存在的路径做归属预判（构建期算产物
        // 路径），canonicalize 文件会失败，而父目录一定在。
        let canon = match plain.parent() {
            Some(parent) => strip_verbatim(
                &parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf()),
            )
            .join(plain.file_name().unwrap_or_default()),
            None => plain.clone(),
        };
        let mut best: Option<(&String, &ModuleEntry, PathBuf)> = None;
        for (name, e) in &self.modules {
            let Some(rel) = plain
                .strip_prefix(&e.root)
                .ok()
                .or_else(|| canon.strip_prefix(&e.root).ok())
                .map(|p| p.to_path_buf())
            else {
                continue;
            };
            let deeper = best
                .as_ref()
                .map(|(_, b, _)| e.root.components().count() > b.root.components().count())
                .unwrap_or(true);
            if deeper {
                best = Some((name, e, rel));
            }
        }
        best.map(|(n, e, r)| (n.as_str(), e, r))
    }

    /// 登记模块：`name` 与 `prefix` 都必须全局唯一（重名会抢产物目录，重前缀会让
    /// 两个模块争同一条 URL）。
    fn insert(
        &mut self,
        name: &str,
        version: &str,
        prefix: &str,
        root: PathBuf,
    ) -> Result<(), String> {
        if let Some(prev) = self.modules.get(name) {
            return Err(format!(
                "module {name:?} declared in multiple module roots ({} and {})",
                prev.root.display(),
                root.display()
            ));
        }
        if let Some((other, prev)) = self.modules.iter().find(|(_, e)| e.prefix == prefix) {
            return Err(format!(
                "url prefix {prefix:?} declared by both module {other:?} ({}) and {name:?} ({}) \
                 — manifest.yaml 的 prefix 必须唯一（省略即用模块名）",
                prev.root.display(),
                root.display()
            ));
        }
        self.modules.insert(
            name.to_string(),
            ModuleEntry {
                root,
                version: version.to_string(),
                prefix: prefix.to_string(),
            },
        );
        Ok(())
    }
}

/// canonical + 去 verbatim（与 `build_cmd` / 装配层同口径）。
fn canonical(p: &Path) -> PathBuf {
    strip_verbatim(&p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
}

/// 扫描源码树根，构造模块索引。
///
/// - `recursive = false`：今日语义（首层子目录 = 模块根；`name` 必须等于目录名）；目录缺失
///   视为空表（合法：fresh 项目），目录在而缺 `manifest.yaml` 仍是错误——与既有行为一致。
/// - `recursive = true`：`--module-root` 递归发现，**命中 0 个即报错**（T5：显式指定了模块
///   根却一个都没找到，是配置错误而非空项目，静默成功会让下游 CI 假绿）。
/// - `skip`：保留目录名（tasks 池，非模块）。
pub fn resolve_roots(
    root: &Path,
    recursive: bool,
    skip: Option<&str>,
) -> Result<ModuleIndex, String> {
    let mut idx = ModuleIndex {
        base: canonical(root),
        modules: BTreeMap::new(),
    };
    if !recursive {
        let rd = match std::fs::read_dir(root) {
            Ok(rd) => rd,
            // 模块目录缺失 = 无模块（空即合法；目录在而缺 manifest.yaml 仍是错误）。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(idx),
            Err(e) => return Err(format!("read module dir {}: {e}", root.display())),
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let dirname = e.file_name().to_string_lossy().into_owned();
            if Some(dirname.as_str()) == skip {
                continue;
            }
            let mf = p.join("manifest.yaml");
            if !mf.is_file() {
                return Err(format!("module '{dirname}' missing manifest.yaml"));
            }
            let (name, version, declared) = manifest_head(&mf)?;
            if name != dirname {
                return Err(format!(
                    "manifest name {:?} != directory name {:?} (in {})",
                    name,
                    dirname,
                    p.display()
                ));
            }
            let prefix = resolve_prefix(declared.as_deref(), &name);
            validate_prefix(prefix).map_err(|e| format!("{e} (in {})", p.display()))?;
            idx.insert(&name, &version, prefix, p)?;
        }
        return Ok(idx);
    }

    let hits = scan_recursive(&idx.base, skip)?;
    if hits.is_empty() {
        return Err(format!(
            "no manifest.yaml under module root {} (--module-root)\n  \
             下一步：确认该目录是后端源码树根（含 <模块>/manifest.yaml），或改指正确的目录",
            root.display()
        ));
    }
    for dir in hits {
        let (name, version, declared) = manifest_head(&dir.join("manifest.yaml"))?;
        // name / prefix 来自 manifest（可被仓库内容控制），且在递归模式下 name 不再受
        // 「== 目录名」约束——必须在发现期即过白名单：它们要进产物目录名与路由 pattern。
        validate_module(&name).map_err(|e| format!("{e} (in {})", dir.display()))?;
        validate_version(&version).map_err(|e| format!("{e} (in {})", dir.display()))?;
        let prefix = resolve_prefix(declared.as_deref(), &name);
        validate_prefix(prefix).map_err(|e| format!("{e} (in {})", dir.display()))?;
        idx.insert(&name, &version, prefix, dir)?;
    }
    Ok(idx)
}

/// 递归找含 `manifest.yaml` 的目录：命中即停（不再深入，保证模块根互不嵌套）、
/// 跳过 `node_modules` 与点目录、**不跟随目录符号链接**、深度与命中数有上限。
fn scan_recursive(base: &Path, skip: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut hits: Vec<PathBuf> = Vec::new();
    let cbase = canonical(base);
    let mut stack: Vec<(PathBuf, usize)> = vec![(base.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if dir.join("manifest.yaml").is_file() {
            // 越界守卫（不跟随目录 symlink 已保证物理包含，此处 canonical 复核一次）。
            if !canonical(&dir).starts_with(&cbase) {
                continue;
            }
            hits.push(dir);
            if hits.len() > MAX_ROOTS {
                return Err(format!(
                    "too many module roots (> {MAX_ROOTS}) under {} — 确认 --module-root 指向的是模块树而非仓库根",
                    base.display()
                ));
            }
            continue; // 命中即停：祖先已是模块根，其下再出现 manifest.yaml 属嵌套（S008）
        }
        if depth >= MAX_DEPTH {
            continue;
        }
        // release 树（含构建锁 manifests.yaml，复数）= `oj build` 产物：其下各版本目录里的
        // manifest.yaml 是**原样复制的副本**，会把同一模块重复登记成第二个模块根。产物树
        // 只应由 `-d dist` 消费，源码树扫描跳过它——锁文件名是 oj 生成专用，无误伤。
        if dir.join("manifests.yaml").is_file() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') || n == "node_modules" || Some(n.as_str()) == skip {
                continue;
            }
            // 不跟随目录符号链接：指向仓库外的链接会被整棵当模块根，其 .ts/.sql 会被
            // 转译打进 dist 与 tgz（信息泄露 + 供应链面）。文件/其它类型一并跳过。
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            stack.push((e.path(), depth + 1));
        }
    }
    hits.sort();
    Ok(hits)
}

/// 源码树内**未被索引认领**的 `manifest.yaml` 所在目录（相对 base 的路径，确定性排序）。
/// 今天它意味着「嵌套 manifest」——`#` 别名以「向上最近的 manifest.yaml」为锚点，嵌套声明会
/// 静默改写整个子树的语义，故 fail-fast（S008）。
pub fn stray_manifests(idx: &ModuleIndex) -> Vec<String> {
    let known: HashSet<&Path> = idx.modules.values().map(|e| e.root.as_path()).collect();
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(idx.base.clone(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') || n == "node_modules" {
                continue;
            }
            let Ok(ft) = e.file_type() else {
                continue;
            };
            if ft.is_symlink() || !ft.is_dir() {
                continue;
            }
            let p = e.path();
            // release 树（含 manifests.yaml 锁）= 构建产物：里面的 manifest.yaml 是原样
            // 复制的副本，不是嵌套声明——与扫描同口径整棵跳过，S008 不误伤产物。
            if p.join("manifests.yaml").is_file() {
                continue;
            }
            if p.join("manifest.yaml").is_file()
                && !known.contains(p.as_path())
                && let Ok(rel) = p.strip_prefix(&idx.base)
            {
                out.push(rel.display().to_string().replace('\\', "/"));
            }
            // 与扫描不同：这里**必须**下探已认领的模块根——嵌套 manifest 正藏在模块根
            // 内部，命中即停会把它漏掉（S008 的守卫对象）。
            if depth + 1 < MAX_DEPTH {
                stack.push((p, depth + 1));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::module_loader::try_symlink;

    struct Tmp(PathBuf);
    fn tmp(tag: &str) -> Tmp {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        use std::sync::atomic::Ordering;
        let d = std::env::temp_dir().join(format!(
            "oj-scan-{tag}-{}-{}",
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
    /// manifest 文本；`prefix: None` 时不写该键（与真实「未声明」同形）。
    fn mf(name: &str, version: &str, prefix: Option<&str>) -> String {
        match prefix {
            Some(p) => format!("name: {name}\nversion: {version}\nprefix: {p}\ndesc: d\n"),
            None => format!("name: {name}\nversion: {version}\ndesc: d\n"),
        }
    }

    #[test]
    fn given_flat_tree_when_scan_flat_then_keeps_today_semantics() {
        // Given: 旧布局（首层目录 = 模块根，name == 目录名）
        let d = tmp("flat");
        write(d.0.join("user/manifest.yaml"), &mf("user", "0.1.0", None));

        // When: 按默认（非递归）口径扫描
        let idx = resolve_roots(&d.0, false, None).unwrap();

        // Then: 模块名与 URL 前缀都等于 name（无 prefix 声明）
        assert_eq!(idx.names().cloned().collect::<Vec<_>>(), vec!["user"]);
        assert_eq!(idx.get("user").unwrap().prefix, "user");
        assert!(idx.get("user").unwrap().root.ends_with("user"));
    }

    #[test]
    fn given_name_mismatch_when_scan_flat_then_same_error_as_today() {
        // Given: name 与目录名不一致（默认路径下非法）
        let bad = tmp("flat-bad");
        write(
            bad.0.join("order/manifest.yaml"),
            &mf("orderr", "0.1.0", None),
        );

        // When: 扫描
        let e = resolve_roots(&bad.0, false, None).unwrap_err();

        // Then: 错误文案与今日逐字一致
        assert!(e.contains("orderr") && e.contains("order"), "{e}");
    }

    #[test]
    fn given_dir_without_manifest_when_scan_flat_then_reports_missing() {
        // Given: 首层目录缺 manifest.yaml
        let none = tmp("flat-none");
        write(none.0.join("x/keep.txt"), "");

        // When: 扫描
        let e = resolve_roots(&none.0, false, None).unwrap_err();

        // Then: 报 missing manifest.yaml（默认路径文案不变）
        assert!(e.contains("missing manifest.yaml"), "{e}");
    }

    #[test]
    fn given_tasks_dir_when_scan_flat_then_skipped() {
        // Given: tasks 池目录（非模块）
        let sk = tmp("flat-skip");
        write(sk.0.join("tasks/job.ts"), "");

        // When: 以 skip=tasks 扫描
        let idx = resolve_roots(&sk.0, false, Some("tasks")).unwrap();

        // Then: 空索引
        assert!(idx.is_empty());
    }

    #[test]
    fn given_nested_roots_when_scan_recursive_then_name_comes_from_manifest() {
        // Given: 模块根在第二层，目录名与模块名无关
        let d = tmp("rec");
        write(d.0.join("foo/api/manifest.yaml"), &mf("m1", "0.1.0", None));
        write(d.0.join("bar/api/manifest.yaml"), &mf("m2", "0.2.0", None));
        write(
            d.0.join("foo/api/xxx/yyy/api.ts"),
            "export async function get(){}\n",
        );

        // When: 递归扫描
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // Then: 身份取 manifest.name，目录名只作定位
        assert_eq!(
            idx.names().cloned().collect::<Vec<_>>(),
            vec!["m1".to_string(), "m2".to_string()]
        );
        assert!(idx.get("m1").unwrap().root.ends_with("foo/api"));
        assert_eq!(idx.get("m2").unwrap().version, "0.2.0");
        assert_eq!(idx.get("m1").unwrap().prefix, "m1");
    }

    #[test]
    fn given_file_in_nested_module_when_locate_then_relative_to_module_root() {
        // Given: 嵌套模块内的文件
        let d = tmp("locate");
        write(d.0.join("foo/api/manifest.yaml"), &mf("m1", "0.1.0", None));
        let f = d.0.join("foo/api/xxx/yyy/api.ts");
        write(f.clone(), "");
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // When: 归属查询
        let (name, _e, rel) = idx.locate(&f).unwrap();

        // Then: 命中模块 m1，相对路径不含目录名 foo/api
        assert_eq!(name, "m1");
        assert_eq!(rel, PathBuf::from("xxx/yyy/api.ts"));
    }

    #[test]
    fn given_manifest_declares_prefix_when_scan_then_prefix_overrides_url_only() {
        // Given: manifest 声明了 prefix（与 name 不同）
        let d = tmp("prefix");
        write(
            d.0.join("foo/api/manifest.yaml"),
            &mf("m1", "0.1.0", Some("sys/user")),
        );

        // When: 扫描
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // Then: 身份仍是 name，只有 URL 前缀是 prefix
        assert!(idx.get("m1").is_some(), "身份键仍是 name");
        assert_eq!(idx.get("m1").unwrap().prefix, "sys/user");
    }

    #[test]
    fn given_prefix_empty_or_absent_when_scan_then_prefix_equals_name() {
        // Given: （a）未声明 prefix（b）声明为空串
        let d = tmp("prefix-empty");
        write(d.0.join("a/api/manifest.yaml"), &mf("ma", "0.1.0", None));
        write(
            d.0.join("b/api/manifest.yaml"),
            &mf("mb", "0.1.0", Some("")),
        );
        write(
            d.0.join("c/api/manifest.yaml"),
            &mf("mc", "0.1.0", Some("   ")),
        );

        // When: 扫描
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // Then: 三种写法都回落为 name
        assert_eq!(idx.get("ma").unwrap().prefix, "ma");
        assert_eq!(idx.get("mb").unwrap().prefix, "mb");
        assert_eq!(idx.get("mc").unwrap().prefix, "mc");
    }

    #[test]
    fn given_two_modules_same_prefix_when_scan_then_rejects() {
        // Given: 两个模块声明了同一个 URL 前缀
        let d = tmp("prefix-dup");
        write(
            d.0.join("x/api/manifest.yaml"),
            &mf("m1", "0.1.0", Some("same")),
        );
        write(
            d.0.join("y/api/manifest.yaml"),
            &mf("m2", "0.1.0", Some("same")),
        );

        // When: 扫描
        let e = resolve_roots(&d.0, true, None).unwrap_err();

        // Then: 报错点明冲突双方（URL 不能被两个模块争）
        assert!(
            e.contains("url prefix") && e.contains("m1") && e.contains("m2"),
            "{e}"
        );
    }

    #[test]
    fn given_illegal_prefix_when_scan_then_rejects() {
        // Given: 带路径逃逸 / 空段 / 反斜杠的前缀
        let d = tmp("prefix-bad");
        for (i, p) in ["../evil", "a//b", "a\\b", "/lead", "trail/", "a/../b"]
            .iter()
            .enumerate()
        {
            write(
                d.0.join(format!("m{i}/api/manifest.yaml")),
                &mf(&format!("m{i}"), "0.1.0", Some(p)),
            );
        }

        // When + Then: 每一种都被拒（前缀进路由 pattern，不得逃逸挂载根）
        for i in 0..6 {
            let root = d.0.join(format!("m{i}"));
            let e = resolve_roots(&root, true, None).unwrap_err();
            assert!(e.contains("illegal url prefix"), "{i}: {e}");
        }
    }

    #[test]
    fn given_two_modules_same_name_when_scan_recursive_then_rejects() {
        // Given: 两个模块根声明了同一个 name（会抢同一产物目录）
        let dup = tmp("rec-dup");
        write(
            dup.0.join("x/api/manifest.yaml"),
            &mf("same", "0.1.0", None),
        );
        write(
            dup.0.join("y/api/manifest.yaml"),
            &mf("same", "0.1.0", None),
        );

        // When: 扫描
        let e = resolve_roots(&dup.0, true, None).unwrap_err();

        // Then: 报错列两条路径
        assert!(
            e.contains("same") && e.contains("multiple module roots"),
            "{e}"
        );
    }

    #[test]
    fn given_illegal_name_or_version_when_scan_recursive_then_rejects() {
        // Given: name / version 非法（name 权威后必须过白名单：它要进产物目录名与锁键）
        let bad = tmp("rec-bad");
        write(
            bad.0.join("x/api/manifest.yaml"),
            &mf("../evil", "0.1.0", None),
        );
        let badv = tmp("rec-badv");
        write(badv.0.join("x/api/manifest.yaml"), &mf("m", "0..1", None));

        // When + Then
        assert!(
            resolve_roots(&bad.0, true, None)
                .unwrap_err()
                .contains("illegal module name")
        );
        assert!(
            resolve_roots(&badv.0, true, None)
                .unwrap_err()
                .contains("illegal version")
        );
    }

    #[test]
    fn given_nested_manifest_when_scan_recursive_then_hit_stops_and_stray_is_reported() {
        // Given: 祖先已是模块根，其下还有 manifest.yaml
        let d = tmp("rec-stop");
        write(d.0.join("a/manifest.yaml"), &mf("a", "0.1.0", None));
        write(d.0.join("a/sub/manifest.yaml"), &mf("sub", "0.1.0", None));

        // When: 扫描 + 查未认领的 manifest
        let idx = resolve_roots(&d.0, true, None).unwrap();
        let stray = stray_manifests(&idx);

        // Then: 只认祖先（命中即停），嵌套的由 stray 暴露给 S008
        assert_eq!(idx.names().cloned().collect::<Vec<_>>(), vec!["a"]);
        assert_eq!(stray, vec!["a/sub"]);
    }

    #[test]
    fn given_no_manifest_when_scan_recursive_then_zero_hits_is_error() {
        // Given: 指定目录下没有任何 manifest.yaml（T5：这是配置错误，不是空项目）
        let d = tmp("rec-zero");
        write(d.0.join("m1/web/index.html"), "");

        // When: 递归扫描
        let e = resolve_roots(&d.0, true, None).unwrap_err();

        // Then: 报错并给下一步（对照：默认口径下空目录仍合法、不报错）
        assert!(
            e.contains("no manifest.yaml") && e.contains("--module-root"),
            "{e}"
        );
        let empty = tmp("rec-zero-flat");
        assert!(resolve_roots(&empty.0, false, None).unwrap().is_empty());
    }

    #[test]
    fn given_node_modules_and_dot_dirs_when_scan_recursive_then_pruned() {
        // Given: node_modules / .git 下也有人放了 manifest.yaml
        let d = tmp("rec-prune");
        write(d.0.join("m1/api/manifest.yaml"), &mf("m1", "0.1.0", None));
        write(
            d.0.join("m1/api/node_modules/pkg/manifest.yaml"),
            &mf("pkg", "1.0.0", None),
        );
        write(
            d.0.join(".git/hidden/manifest.yaml"),
            &mf("hidden", "1.0.0", None),
        );

        // When: 扫描
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // Then: 只认业务模块，剪枝目录不进索引也不算嵌套
        assert_eq!(idx.names().cloned().collect::<Vec<_>>(), vec!["m1"]);
        assert!(stray_manifests(&idx).is_empty());
    }

    #[test]
    fn given_symlink_to_outside_when_scan_recursive_then_not_a_module_root() {
        // Given: 一个指向仓库外的符号链接目录（内含 manifest.yaml）
        let d = tmp("rec-symlink");
        let outside = tmp("rec-outside");
        write(
            outside.0.join("manifest.yaml"),
            &mf("stolen", "9.9.9", None),
        );
        write(d.0.join("m1/api/manifest.yaml"), &mf("m1", "0.1.0", None));
        if !try_symlink(&outside.0, &d.0.join("linked"), true) {
            eprintln!("skip: symlink unsupported on this host");
            return;
        }

        // When: 扫描
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // Then: 链接不被跟随，仓库外内容不会进 dist/tgz
        assert!(idx.get("stolen").is_none(), "仓库外链接被当成了模块根");
        assert_eq!(idx.names().cloned().collect::<Vec<_>>(), vec!["m1"]);
    }

    #[test]
    fn given_symlink_cycle_when_scan_recursive_then_terminates() {
        // Given: 自引用符号链接（成环）
        let d = tmp("rec-cycle");
        std::fs::create_dir_all(d.0.join("a")).unwrap();
        if !try_symlink(&d.0.join("a"), &d.0.join("a/self"), true) {
            eprintln!("skip: symlink unsupported on this host");
            return;
        }

        // When + Then: 不跟随目录 symlink，遍历终止（0 命中即报错，不会挂死）
        assert!(resolve_roots(&d.0, true, None).is_err());
    }

    #[test]
    fn given_file_outside_any_module_when_locate_then_none() {
        // Given: tests 目录里的文件（模块外）
        let d = tmp("locate-out");
        write(d.0.join("m1/api/manifest.yaml"), &mf("m1", "0.1.0", None));
        write(d.0.join("tests/x.test.ts"), "");
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // When + Then: 模块外归属为 None（别名走既有「未找到模块根」报错）
        assert!(idx.locate(&d.0.join("tests/x.test.ts")).is_none());
        assert!(idx.locate(&d.0.join("m1/api/api.ts")).is_some());
    }

    #[test]
    fn given_built_dist_inside_module_root_when_scan_recursive_then_product_copies_skipped() {
        // Given: 源码树里就地 build 过（dist/ 与模块根同级）——产物目录里是 manifest 的
        // **原样副本**，不跳过就会把同一模块登记成第二个模块根（实测报重名）。
        let d = tmp("rec-dist");
        write(d.0.join("foo/api/manifest.yaml"), &mf("m1", "0.1.0", None));
        write(d.0.join("dist/manifests.yaml"), "m1: 0.1.0\n");
        write(
            d.0.join("dist/m1-0.1.0/manifest.yaml"),
            &mf("m1", "0.1.0", None),
        );
        write(d.0.join("dist/m1-0.1.0/api.js"), "");

        // When: 递归扫描
        let idx = resolve_roots(&d.0, true, None).unwrap();

        // Then: 只认源码侧模块根一次；产物副本不进索引也不算嵌套（S008 不误伤）
        assert_eq!(idx.names().cloned().collect::<Vec<_>>(), vec!["m1"]);
        assert!(idx.get("m1").unwrap().root.ends_with("foo/api"));
        assert!(stray_manifests(&idx).is_empty());
    }
}
