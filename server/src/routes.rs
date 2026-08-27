#![allow(
    clippy::type_complexity,
    clippy::collapsible_if,
    clippy::redundant_closure
)]

//! 任意深度；无 api 文件的目录不是路由（可作纯工具代码目录）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 目录镜像路由器。ts=true（--dev）找 api.ts，否则 api.js。
#[derive(Clone)]
pub struct Routes {
    base: String,
    root: PathBuf,
    ts: bool,
}

impl Routes {
    pub fn new(base: &str, root: impl Into<PathBuf>, ts: bool) -> Self {
        // 归一 base：保证前后各一个 '/'（"/v1/api" 与 "/v1/api/" 等价）。
        let base = format!("/{}/", base.trim_matches('/'));
        Self {
            base,
            root: root.into(),
            ts,
        }
    }

    /// 解析 HTTP 路径 → api 文件绝对路径；目录不存在/越界/非文件 → None。
    pub fn resolve(&self, http_path: &str) -> Option<PathBuf> {
        let rel = http_path.strip_prefix(self.base.as_str())?;
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return None;
        }
        // 安全：拒绝空段与越界段（目录穿越按 404 处理）。
        if rel
            .split('/')
            .any(|s| s.is_empty() || s == ".." || s == "." || s.contains('\\') || s.contains('\0'))
        {
            return None;
        }
        let file = self
            .root
            .join(rel)
            .join(if self.ts { "api.ts" } else { "api.js" });
        file.is_file().then_some(file)
    }
}

/// 查表前守卫+归一：`\`/`\0`/空段/`.`/`..` → None（404，对齐旧 resolve 契约，routes.rs:28-33）；
/// 尾斜杠归一（`/a/`→`/a`，根保持 `/`）。
pub fn normalize(path: &str) -> Option<String> {
    if path.contains('\\') || path.contains('\0') || !path.starts_with('/') {
        return None;
    }
    let t = path.trim_end_matches('/');
    if t.is_empty() {
        return Some("/".into());
    }
    if t[1..]
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return None;
    }
    Some(t.to_string())
}

/// 匹配后参数：percent-decode（`+` 保持字面，路径语义）+ 走私校验，None → 404。
/// 拒绝：解码值为 `.`/`..`、含 `\`/`\0`，或 **raw 无 `/` 而解码后有 `/`**（单段参数走私 `%2F`）；
/// catch-all 的 raw 值含真实分隔符，放行。
pub fn decode_params(
    pairs: impl Iterator<Item = (String, String)>,
) -> Option<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (k, raw) in pairs {
        let v = percent_encoding::percent_decode_str(&raw)
            .decode_utf8()
            .ok()?;
        let smuggled = !raw.contains('/') && v.contains('/');
        if v == "." || v == ".." || v.contains('\\') || v.contains('\0') || smuggled {
            return None;
        }
        out.insert(k, v.into_owned());
    }
    Some(out)
}

/// HTTP 动词 → handler 方法名（全表；DELETE→del）。未映射 → None（405）。
pub fn method_name(m: &str) -> Option<&'static str> {
    match m {
        "GET" => Some("get"),
        "POST" => Some("post"),
        "PUT" => Some("put"),
        "DELETE" => Some("del"),
        "PATCH" => Some("patch"),
        "HEAD" => Some("head"),
        "OPTIONS" => Some("options"),
        _ => None,
    }
}

/// 归一化文件标识：路由表内每个唯一 api 文件一个 id，消除 (file, method) 的 PathBuf 重复存储。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(pub u32);

/// 单 pattern 下某方法的归宿：文件 / 冲突（请求期 500）。
#[derive(Clone)]
pub enum Entry {
    File(FileId),
    Conflict(String),
}

/// 查表结果四态（handle 据此映射 200/500/405/404）。
pub enum Lookup {
    Hit {
        file: PathBuf,
        params: HashMap<String, String>,
    },
    Conflict(String),
    MethodNotAllowed,
    NotFound,
}

/// 启动打印行（method × pattern × file_id）。
#[derive(Clone)]
pub struct RouteRow {
    pub method: String,
    pub pattern: String,
    pub file: FileId,
}

/// 路由表：单 matchit matcher，pattern 的 value 是 方法名 → Entry 映射——
/// 405 判定 O(1)（命中 pattern 但方法缺席），“冲突哨兵”即映射里的 Conflict 变体。
/// files 为文件表：FileId → 唯一绝对路径，消除 (file, method) 的 PathBuf 重复存储。
#[derive(Clone)]
pub struct RouteTable {
    matcher: matchit::Router<HashMap<String, Entry>>,
    /// 挂了 .route 的 (file_id, js 方法名)：dev 兜底不得复活其目录镜像 URL。
    replaced: std::collections::HashSet<(FileId, String)>,
    rows: Vec<RouteRow>,
    /// 文件表：FileId → 唯一绝对路径（去重存储，供 file_path / 分组输出复用）。
    files: Vec<PathBuf>,
}

const METHODS: [&str; 7] = ["get", "post", "put", "del", "patch", "head", "options"];

impl Default for RouteTable {
    fn default() -> Self {
        Self {
            matcher: matchit::Router::new(),
            replaced: std::collections::HashSet::new(),
            rows: Vec::new(),
            files: Vec::new(),
        }
    }
}

impl RouteTable {
    /// 建表：内省闭包按文件返回 Vec<(方法, .route 或 None)>；返回 (表, 失败/冲突清单)。
    /// 纯逻辑（依赖倒置：不依赖 JS 运行时），CLI/测试注入真实或假内省。
    pub fn build(
        base: &str,
        root: &Path,
        ts: bool,
        introspect: impl Fn(&Path) -> Result<Vec<(String, Option<String>)>, String>,
    ) -> (Self, Vec<String>) {
        let b = base.trim_matches('/');
        let mut failures = Vec::new();
        let mut t = RouteTable {
            matcher: matchit::Router::new(),
            replaced: std::collections::HashSet::new(),
            rows: Vec::new(),
            files: Vec::new(),
        };
        for file in api_files(root, ts) {
            let decls = match introspect(&file) {
                Ok(d) => d,
                Err(e) => {
                    failures.push(format!("{}: {e}", file.display()));
                    continue;
                }
            };
            let rel = file
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .unwrap_or(Path::new(""))
                .to_string_lossy()
                .replace('\\', "/");
            let dir_base = if rel.is_empty() {
                format!("/{b}")
            } else {
                format!("/{b}/{rel}")
            };
            for (method, route) in decls {
                if !METHODS.contains(&method.as_str()) {
                    continue;
                }
                let route = route.filter(|r| !r.is_empty()); // "" 视同未挂
                let pattern = match &route {
                    None => dir_base.clone(),
                    Some(r) if r.starts_with('/') => format!("/{b}{r}"), // 根级（base 根下）
                    Some(r) => format!("{dir_base}/{r}"),                // 相对
                };
                if route.is_some() {
                    let fid = t.intern(&file);
                    t.replaced.insert((fid, method.clone()));
                }
                t.register(&mut failures, &method, &pattern, &file);
            }
        }
        (t, failures)
    }

    /// release 直载：routes.js 导出的全量行（pattern 已含 base，file 相对 root）。
    /// 注册语义与 build 一致（合并 / 冲突 / 非法 pattern 丢弃），replaced 恒空（无 fs 兜底）。
    pub fn from_entries(root: &Path, entries: &[RouteEntry]) -> (Self, Vec<String>) {
        let mut t = RouteTable {
            matcher: matchit::Router::new(),
            replaced: std::collections::HashSet::new(),
            rows: Vec::new(),
            files: Vec::new(),
        };
        let mut failures = Vec::new();
        for e in entries {
            if !METHODS.contains(&e.method.as_str()) {
                failures.push(format!(
                    "routes.js: unknown method {} {}",
                    e.method, e.pattern
                ));
                continue;
            }
            let legal_file = |f: &str| {
                !f.is_empty()
                    && !f.split('/').any(|s| {
                        s.is_empty()
                            || s == ".."
                            || s == "."
                            || s.contains('\\')
                            || s.contains('\0')
                    })
            };
            if !legal_file(&e.file) {
                failures.push(format!("routes.js: illegal file path {}", e.file));
                continue;
            }
            if !e.pattern.starts_with('/') || e.pattern.contains("//") {
                failures.push(format!("routes.js: illegal pattern {}", e.pattern));
                continue;
            }
            t.register(&mut failures, &e.method, &e.pattern, &root.join(&e.file));
        }
        (t, failures)
    }

    /// 文件去重：相同路径复用同一 FileId；否则追加到 files 表。
    fn intern(&mut self, file: &Path) -> FileId {
        if let Some(i) = self.files.iter().position(|p| p == file) {
            return FileId(i as u32);
        }
        let id = FileId(self.files.len() as u32);
        self.files.push(file.to_path_buf());
        id
    }

    /// 注册一行：新 pattern 建方法映射；已有 pattern 合并方法；
    /// 同 (pattern, method) 二次声明 → Conflict（请求期 500）；matchit 拒绝 → 记 failures。
    fn register(&mut self, failures: &mut Vec<String>, method: &str, pattern: &str, file: &Path) {
        let fid = self.intern(file);
        match self.matcher.at_mut(pattern) {
            Ok(m) => match m.value.get(method) {
                Some(Entry::File(a)) => {
                    let msg = format!(
                        "route conflict: {method} {pattern} declared in {} and {}",
                        self.files[a.0 as usize].display(),
                        file.display()
                    );
                    *m.value.get_mut(method).unwrap() = Entry::Conflict(msg.clone());
                    failures.push(msg);
                }
                _ => {
                    m.value.insert(method.to_string(), Entry::File(fid));
                    self.rows.push(RouteRow {
                        method: method.to_string(),
                        pattern: pattern.to_string(),
                        file: fid,
                    });
                }
            },
            Err(_) => {
                let mut map = HashMap::new();
                map.insert(method.to_string(), Entry::File(fid));
                match self.matcher.insert(pattern.to_string(), map) {
                    Ok(()) => self.rows.push(RouteRow {
                        method: method.to_string(),
                        pattern: pattern.to_string(),
                        file: fid,
                    }),
                    // 非法语法 / 结构性冲突（同位置异名参数）：日志丢弃后来者
                    Err(e) => failures.push(format!(
                        "invalid route {method} {pattern} from {}: {e}",
                        file.display()
                    )),
                }
            }
        }
    }

    /// 查表：path 须先经 `normalize`。未映射动词按"路径存在 → 405"契约处理。
    pub fn lookup(&self, path: &str, verb: &str) -> Lookup {
        let m = match self.matcher.at(path) {
            Ok(m) => m,
            Err(_) => return Lookup::NotFound,
        };
        let Some(name) = method_name(verb) else {
            return Lookup::MethodNotAllowed;
        };
        match m.value.get(name) {
            Some(Entry::File(f)) => {
                let pairs = m.params.iter().map(|(k, v)| (k.to_string(), v.to_string()));
                match decode_params(pairs) {
                    Some(params) => Lookup::Hit {
                        file: self.files[f.0 as usize].clone(),
                        params,
                    },
                    None => Lookup::NotFound, // 走私参数 → 404（§6.1-4）
                }
            }
            Some(Entry::Conflict(msg)) => Lookup::Conflict(msg.clone()),
            None => Lookup::MethodNotAllowed,
        }
    }

    /// dev 兜底守卫：该 (file, 方法) 是否已挂 .route（目录镜像被替换）。
    pub fn is_replaced(&self, file: &Path, js_method: &str) -> bool {
        match self.id_of(file) {
            Some(id) => self.replaced.contains(&(id, js_method.to_string())),
            None => false,
        }
    }

    /// 路径 → FileId（仅当路径已入表）；dev 兜底比对用，未入表返回 None。
    fn id_of(&self, file: &Path) -> Option<FileId> {
        self.files
            .iter()
            .position(|p| p == file)
            .map(|i| FileId(i as u32))
    }

    pub fn listing(&self) -> &[RouteRow] {
        &self.rows
    }

    /// FileId → 绝对路径（lookup 已把 Hit 解析为 PathBuf；此处供 banner / is_replaced 复用）。
    pub fn file_path(&self, id: FileId) -> &Path {
        &self.files[id.0 as usize]
    }

    /// 按文件分组输出（FileId 分组）：同一 api 文件的多个谓词归到一行文件头下，
    /// 避免 (method × pattern) 散落成多行。返回 [(FileId, &Path, [(METHOD, pattern)])]。
    pub fn grouped(&self) -> Vec<(FileId, &Path, Vec<(String, String)>)> {
        let mut out: Vec<(FileId, &Path, Vec<(String, String)>)> = Vec::new();
        for row in &self.rows {
            let path = &self.files[row.file.0 as usize];
            match out.iter_mut().find(|(id, _, _)| *id == row.file) {
                Some(slot) => slot
                    .2
                    .push((row.method.to_uppercase(), row.pattern.clone())),
                None => out.push((
                    row.file,
                    path,
                    vec![(row.method.to_uppercase(), row.pattern.clone())],
                )),
            }
        }
        out
    }
}

/// root 下全部 api 文件（排序 → 冲突裁决顺序确定）。
fn api_files(root: &Path, ts: bool) -> Vec<PathBuf> {
    let ext = if ts { "api.ts" } else { "api.js" };
    let mut out = Vec::new();
    walk_files(root, ext, &mut out);
    out.sort();
    out
}

pub(crate) fn walk_files(dir: &Path, ext: &str, acc: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_files(&p, ext, acc);
        } else if e.file_name().to_string_lossy() == ext {
            acc.push(p);
        }
    }
}

/// routes.js 导出行（oj build 生成；release 直载免内省）。
pub struct RouteEntry {
    pub method: String,
    pub pattern: String,
    pub file: String,
}

/// routes.js 的 default 导出 → 行集（缺字段/类型错的行跳过）。
pub fn entries_from_value(v: &serde_json::Value) -> Vec<RouteEntry> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(RouteEntry {
                        method: e.get("method")?.as_str()?.to_string(),
                        pattern: e.get("pattern")?.as_str()?.to_string(),
                        file: e.get("file")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 内省结果 Value（introspect_module 约定：仅函数导出的方法，null=未挂）→ decls。
pub fn decls_from_value(v: &serde_json::Value) -> Vec<(String, Option<String>)> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    Some((
                        k.clone(),
                        match v {
                            serde_json::Value::String(s) => Some(s.clone()),
                            serde_json::Value::Null => None,
                            _ => return None,
                        },
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 真实内省闭包：每文件一线程 + 独立 current_thread runtime（Bridge !Send 不跨线程；
/// 嵌套 runtime 会 panic，故换线程）。CLI 与测试共用。
/// ponytail: 每文件起线程；文件数极大时改单线程批处理。
pub fn bridge_introspector(
    make: impl Fn() -> only_js::bridge::Bridge + Send + Sync + 'static,
) -> impl Fn(&Path) -> Result<Vec<(String, Option<String>)>, String> {
    let make = std::sync::Arc::new(make);
    move |f: &Path| {
        let f = f.to_path_buf();
        let make = make.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("introspect rt");
            let b = make();
            rt.block_on(async { b.introspect_module(&f).await })
                .map(|v| decls_from_value(&v))
                .map_err(|e| e.to_string())
        })
        .join()
        .unwrap_or_else(|_| Err("introspect thread panicked".into()))
    }
}

/// 读模块 default 导出（release 直载 dist/routes.js）：独立线程 + current_thread rt，
/// 与 bridge_introspector 同构（Bridge !Send，不可在异步上下文嵌套建 runtime）。
pub fn bridge_default_reader(
    make: impl Fn() -> only_js::bridge::Bridge + Send + Sync + 'static,
) -> impl Fn(&Path) -> Result<serde_json::Value, String> {
    let make = std::sync::Arc::new(make);
    move |f: &Path| {
        let f = f.to_path_buf();
        let make = make.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("reader rt");
            let b = make();
            rt.block_on(async { b.read_module_default(&f).await })
                .map_err(|e| e.to_string())
        })
        .join()
        .unwrap_or_else(|_| Err("routes.js reader thread panicked".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(files: &[&str]) -> std::path::PathBuf {
        // 计数器唯一化：并行测试下 `{:p}` 指针可被分配器复用，曾致临时目录串台。
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "oj-routes-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        for rel in files {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "// api").unwrap();
        }
        base
    }

    #[test]
    fn mirrors_directory_tree_any_depth() {
        let root = fixture(&["user/account/api.ts", "user/profile/detail/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        assert_eq!(
            r.resolve("/v1/api/user/account/"),
            Some(root.join("user/account/api.ts"))
        );
        assert_eq!(
            r.resolve("/v1/api/user/account"),
            Some(root.join("user/account/api.ts"))
        );
        assert_eq!(
            r.resolve("/v1/api/user/profile/detail/"),
            Some(root.join("user/profile/detail/api.ts"))
        );
    }

    #[test]
    fn missing_or_traversal_is_none() {
        let root = fixture(&["user/account/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        assert_eq!(r.resolve("/v1/api/none/here/"), None);
        assert_eq!(r.resolve("/v1/api/../etc/"), None);
        assert_eq!(r.resolve("/v1/api//dbl/"), None);
        assert_eq!(r.resolve("/other/base/user/account/"), None);
        assert_eq!(r.resolve("/v1/api/"), None);
        assert_eq!(r.resolve("/v1/apifoo/user/"), None);
    }

    #[test]
    fn release_mode_maps_api_js() {
        let root = fixture(&["user/account/api.js"]);
        assert!(
            Routes::new("/v1/api", &root, false)
                .resolve("/v1/api/user/account/")
                .is_some()
        );
        assert!(
            Routes::new("/v1/api", &root, true)
                .resolve("/v1/api/user/account/")
                .is_none()
        );
    }

    #[test]
    fn method_table_complete() {
        assert_eq!(method_name("GET"), Some("get"));
        assert_eq!(method_name("DELETE"), Some("del"));
        assert_eq!(method_name("PATCH"), Some("patch"));
        assert_eq!(method_name("HEAD"), Some("head"));
        assert_eq!(method_name("OPTIONS"), Some("options"));
        assert_eq!(method_name("TRACE"), None);
    }

    #[test]
    fn normalize_guards_and_trims() {
        assert_eq!(
            normalize("/v1/api/user/account/"),
            Some("/v1/api/user/account".into())
        );
        assert_eq!(
            normalize("/v1/api/user/account"),
            Some("/v1/api/user/account".into())
        );
        assert_eq!(normalize("/"), Some("/".into()));
        assert_eq!(normalize("/v1/api//dbl"), None); // 空段（missing_or_traversal_is_none 契约）
        assert_eq!(normalize("/v1/api/../etc"), None); // 穿越段
        assert_eq!(normalize("/v1/api/./x"), None);
        assert_eq!(normalize("/v1/api/a\\b"), None); // 反斜杠
        assert_eq!(normalize("/v1/api/a\0b"), None); // NUL
    }

    #[test]
    fn decode_params_validates_smuggling() {
        let one = |v: &str| decode_params(vec![("id".into(), v.into())].into_iter());
        assert_eq!(one("42").unwrap()["id"], "42");
        assert_eq!(one("%41").unwrap()["id"], "A"); // 正常解码
        assert!(one("%2e%2e").is_none()); // 编码穿越
        assert!(one(".").is_none());
        // 单段走私斜杠：raw 无 / 而解码后有 → 拒绝
        assert!(one("a%2Fb").is_none());
        assert!(one("a%5Cb").is_none());
        // catch-all 值：raw 含真实分隔符 → 放行
        let ca = decode_params(vec![("path".into(), "a/b%20c".into())].into_iter());
        assert_eq!(ca.unwrap()["path"], "a/b c");
    }

    // ----- RouteTable（纯逻辑，假内省闭包） -----

    fn tbl(files: &[&str], decls: &[(&str, &str, &str)]) -> (RouteTable, Vec<String>) {
        // decls: (文件相对路径, 方法, .route 值；空串 = 未挂)
        let root = fixture(files);
        let m: HashMap<String, Vec<(String, Option<String>)>> = decls
            .iter()
            .map(|(f, m, r)| {
                (
                    f.to_string(),
                    vec![(
                        m.to_string(),
                        if r.is_empty() {
                            None
                        } else {
                            Some(r.to_string())
                        },
                    )],
                )
            })
            .collect();
        RouteTable::build("/v1/api", &root, true, |p: &Path| {
            // 跨平台：file 路径在 Windows 用反斜杠，而 decls 的 key 用正斜杠，
            // 统一转正斜杠再查表，否则 Windows 下 key 不匹配 → 整表空注册。
            let key = p
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            Ok(m.get(&key).cloned().unwrap_or_default())
        })
    }

    #[test]
    fn table_registers_relative_and_rooted() {
        let (t, f) = tbl(
            &["user/account/api.ts"],
            &[("user/account/api.ts", "get", "")],
        );
        assert!(f.is_empty(), "{f:?}");
        assert!(matches!(
            t.lookup("/v1/api/user/account", "GET"),
            Lookup::Hit { .. }
        ));
        assert!(matches!(
            t.lookup("/v1/api/user/account", "POST"),
            Lookup::MethodNotAllowed
        ));
        assert!(matches!(t.lookup("/v1/api/none", "GET"), Lookup::NotFound));
        // 根级 api.ts：dir_base = base 本身（无尾斜杠）
        let (t2, f2) = tbl(&["api.ts"], &[("api.ts", "get", "")]);
        assert!(f2.is_empty(), "{f2:?}");
        assert!(matches!(t2.lookup("/v1/api", "GET"), Lookup::Hit { .. }));
    }

    #[test]
    fn table_param_extraction_and_route_suffix() {
        let (t, _) = tbl(
            &["user/account/api.ts"],
            &[("user/account/api.ts", "get", "{id}")],
        );
        match t.lookup("/v1/api/user/account/42", "GET") {
            Lookup::Hit { params, .. } => assert_eq!(params["id"], "42"),
            _ => panic!("expected hit"),
        }
        // 挂 .route 后目录镜像不再注册（替换语义）
        assert!(matches!(
            t.lookup("/v1/api/user/account", "GET"),
            Lookup::NotFound
        ));
        let id = t.listing().iter().find(|r| r.method == "get").unwrap().file;
        assert!(t.is_replaced(t.file_path(id), "get"));
        assert!(!t.is_replaced(t.file_path(id), "post"));
    }

    #[test]
    fn table_rooted_route_ignores_dir() {
        let (t, _) = tbl(
            &["legacy/compat/api.ts"],
            &[("legacy/compat/api.ts", "get", "/v2/user/{id}")],
        );
        assert!(matches!(
            t.lookup("/v1/api/v2/user/42", "GET"),
            Lookup::Hit { .. }
        ));
        assert!(matches!(
            t.lookup("/v1/api/legacy/compat", "GET"),
            Lookup::NotFound
        ));
    }

    #[test]
    fn table_duplicate_is_conflict_500() {
        let (t, f) = tbl(
            &["a/api.ts", "b/api.ts"],
            &[
                ("a/api.ts", "get", "/user/{id}"),
                ("b/api.ts", "get", "/user/{id}"),
            ],
        );
        assert!(f.iter().any(|s| s.contains("route conflict")), "{f:?}");
        assert!(matches!(
            t.lookup("/v1/api/user/1", "GET"),
            Lookup::Conflict(_)
        ));
        // 冲突 pattern 的其它 verb 仍 405 语义
        assert!(matches!(
            t.lookup("/v1/api/user/1", "POST"),
            Lookup::MethodNotAllowed
        ));
    }

    #[test]
    fn table_merges_verbs_across_files() {
        let (t, f) = tbl(
            &["a/api.ts", "b/api.ts"],
            &[("a/api.ts", "get", "/x"), ("b/api.ts", "post", "/x")],
        );
        assert!(f.is_empty(), "{f:?}");
        assert!(matches!(t.lookup("/v1/api/x", "GET"), Lookup::Hit { .. }));
        assert!(matches!(t.lookup("/v1/api/x", "POST"), Lookup::Hit { .. }));
    }

    #[test]
    fn table_invalid_pattern_dropped_with_failure() {
        let (t, f) = tbl(&["a/api.ts"], &[("a/api.ts", "get", "{*p}/tail")]); // catch-all 非末尾
        assert!(!f.is_empty());
        assert!(matches!(t.lookup("/v1/api/a", "GET"), Lookup::NotFound));
    }

    #[test]
    fn table_mixed_segment_patterns_rejected() {
        // axum 钉 matchit =0.8.4：参数段内混字面一律非法（{id}.json / v{major}.{minor}）
        // → 丢弃 + failures；0.8.6 放宽后此测试需反转（手册 §7.1）。
        let (t, f) = tbl(
            &["a/api.ts", "b/api.ts"],
            &[
                ("a/api.ts", "get", "{id}.json"),
                ("b/api.ts", "get", "v{major}.{minor}"),
            ],
        );
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f[0].contains("invalid route"), "{f:?}");
        assert!(matches!(
            t.lookup("/v1/api/a/42.json", "GET"),
            Lookup::NotFound
        ));
        assert!(matches!(
            t.lookup("/v1/api/b/v1.2", "GET"),
            Lookup::NotFound
        ));
    }

    #[test]
    fn table_catch_all_needs_one_segment() {
        let (t, _) = tbl(&["file/api.ts"], &[("file/api.ts", "get", "{*path}")]);
        match t.lookup("/v1/api/file/a/b/c", "GET") {
            Lookup::Hit { params, .. } => assert_eq!(params["path"], "a/b/c"),
            _ => panic!("expected hit"),
        }
        assert!(matches!(t.lookup("/v1/api/file", "GET"), Lookup::NotFound));
    }

    #[test]
    fn table_introspect_failure_skips_file() {
        let root = fixture(&["bad/api.ts", "good/api.ts"]);
        let (t, f) = RouteTable::build("/v1/api", &root, true, |p: &Path| {
            if p.ends_with("bad/api.ts") {
                Err("syntax error".into())
            } else {
                Ok(vec![("get".into(), None)])
            }
        });
        assert_eq!(f.len(), 1);
        assert!(matches!(
            t.lookup("/v1/api/good", "GET"),
            Lookup::Hit { .. }
        ));
        assert!(matches!(t.lookup("/v1/api/bad", "GET"), Lookup::NotFound));
    }

    #[test]
    fn table_unmapped_verb_405_when_path_exists() {
        let (t, _) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "")]);
        assert!(matches!(
            t.lookup("/v1/api/u", "TRACE"),
            Lookup::MethodNotAllowed
        ));
        assert!(matches!(
            t.lookup("/v1/api/none", "TRACE"),
            Lookup::NotFound
        ));
    }

    #[test]
    fn table_empty_route_string_means_unset() {
        // .route = "" 视同未挂：目录镜像照常注册
        let (t, _) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "")]);
        assert!(matches!(t.lookup("/v1/api/u", "GET"), Lookup::Hit { .. }));
        let (t2, _) = tbl(&["v/api.ts"], &[("v/api.ts", "get", "  ")]);
        // 非空但仅空白：作为字面 pattern 注册（不特判，文档写明空串视同未挂）
        assert!(matches!(
            t2.lookup("/v1/api/v/  ", "GET"),
            Lookup::Hit { .. }
        ));
    }

    // ----- release 直载（routes.js）-----

    #[test]
    fn from_entries_registers_and_conflicts() {
        let root = PathBuf::from("/r");
        let es = |m: &str, p: &str, f: &str| RouteEntry {
            method: m.into(),
            pattern: p.into(),
            file: f.into(),
        };
        let (t, failures) = RouteTable::from_entries(
            &root,
            &[
                es("get", "/a/{id}", "a/api.js"),
                es("post", "/a/{id}", "a/api.js"), // 跨方法合并
                es("get", "/a/{id}", "b/api.js"),  // 同 (pattern, method) → 冲突（请求期 500）
                es("get", "/bad/{*x}tail", "c/api.js"), // matchit 拒绝 → failure
                es("brew", "/a", "d/api.js"),      // 未知方法 → failure
            ],
        );
        assert_eq!(failures.len(), 3, "{failures:?}");
        assert!(matches!(t.lookup("/a/1", "GET"), Lookup::Conflict(_)));
        assert!(matches!(t.lookup("/a/1", "POST"), Lookup::Hit { .. }));
        assert!(matches!(t.lookup("/a/1", "PUT"), Lookup::MethodNotAllowed));
        // 无冲突表：Hit 的 file 相对 root 解析
        let (t2, _) = RouteTable::from_entries(&root, &[es("get", "/a/{id}", "a/api.js")]);
        let Lookup::Hit { file, .. } = t2.lookup("/a/1", "GET") else {
            panic!()
        };
        assert_eq!(file, PathBuf::from("/r/a/api.js"));
        // release 无 fs 兜底：表外路径 404
        assert!(matches!(t.lookup("/nope", "GET"), Lookup::NotFound));
    }

    #[test]
    fn entries_from_value_parses_and_skips() {
        let v = serde_json::json!([
            { "method": "get", "pattern": "/a/{id}", "file": "a/api.js" },
            { "method": 1 },  // 缺字段/类型错 → 跳过
            "junk",
        ]);
        let es = entries_from_value(&v);
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].pattern, "/a/{id}");
        assert!(entries_from_value(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn from_entries_rejects_traversal_and_bad_pattern() {
        let root = PathBuf::from("/r");
        let es = |m: &str, p: &str, f: &str| RouteEntry {
            method: m.into(),
            pattern: p.into(),
            file: f.into(),
        };
        let (t, failures) = RouteTable::from_entries(
            &root,
            &[
                es("get", "/a/{id}", "../etc/passwd"), // 穿越
                es("get", "/a/{id}", "a/../b.js"),     // 中段 ..
                es("get", "/a/{id}", "a\\b.js"),       // 反斜杠
                es("get", "/a//x", "a/api.js"),        // pattern 空段
                es("get", "a/x", "a/api.js"),          // pattern 无首斜杠
                es("get", "/a/{id}", "a/api.js"),      // 合法行仍注册
            ],
        );
        assert_eq!(failures.len(), 5, "{failures:?}");
        assert!(
            failures.iter().all(|f| f.contains("illegal")),
            "{failures:?}"
        );
        assert!(matches!(t.lookup("/a/1", "GET"), Lookup::Hit { .. }));
    }
}
