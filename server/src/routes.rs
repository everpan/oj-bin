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

    /// 解析 HTTP 路径 → (api 文件绝对路径, 路径参数)。目录不存在/越界/非文件 → None。
    /// 快路径：字面 join（无参，旧契约）；miss 时带回溯下降（每层 字面 → 首个排序
    /// `_x_` 候选），经 `_x_` 段收集 (参数名, 实参段)，最后统一过 `decode_params`
    /// （percent-decode + 走私校验，不过 → 404，与表内命中同一条防线）。
    pub fn resolve(&self, http_path: &str) -> Option<(PathBuf, HashMap<String, String>)> {
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
        let segs: Vec<&str> = rel.split('/').collect();
        let api = if self.ts { "api.ts" } else { "api.js" };
        let file = self
            .root
            .join(segs[..].iter().collect::<PathBuf>())
            .join(api);
        if file.is_file() {
            return Some((file, HashMap::new()));
        }
        // ponytail: 每层至多一次 read_dir（dev 兜底、仅表外 miss 路径）；
        // 树极宽且高频 miss 时再考虑缓存。
        let mut raw: Vec<(String, String)> = Vec::new();
        let file = self.descend(&self.root, &segs, api, &mut raw)?;
        decode_params(raw.into_iter()).map(|params| (file, params))
    }

    /// 单层候选序：字面目录 → 排序后首个 `_x_` 目录（同位异名参数在建表期即结构性
    /// 冲突被丢弃，只试首个，避免命中 release 永远服务不到的路由）。子树失败回溯，
    /// 回溯时截断已收集的参数。
    fn descend(
        &self,
        dir: &Path,
        segs: &[&str],
        api: &str,
        raw: &mut Vec<(String, String)>,
    ) -> Option<PathBuf> {
        if segs.is_empty() {
            let f = dir.join(api);
            return f.is_file().then_some(f);
        }
        let mark = raw.len();
        // 1) 字面目录优先（对齐 matchit 静态段优先）。
        let lit = dir.join(segs[0]);
        if lit.is_dir()
            && let Some(f) = self.descend(&lit, &segs[1..], api, raw)
        {
            return Some(f);
        }
        raw.truncate(mark);
        // 2) 首个排序后的 `_x_` 候选。
        let (name, sub) = underscore_child(dir)?;
        raw.push((name, segs[0].to_string()));
        let out = self.descend(&sub, &segs[1..], api, raw);
        if out.is_none() {
            raw.truncate(mark);
        }
        out
    }
}

/// 目录下排序后的首个可转换 `_name_` 子目录 → (参数名, 路径)。无则 None。
/// 用严格转换谓词（`fs_seg_to_pattern` 有转换才命中），与建表期口径一致：
/// `__x__`/`_a{b}_` 这类不转换的目录在这里同样不是参数候选。
fn underscore_child(dir: &Path) -> Option<(String, PathBuf)> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut cands: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let conv = fs_seg_to_pattern(&name);
            if e.path().is_dir() && conv != name {
                let inner = conv
                    .trim_start_matches('{')
                    .trim_end_matches('}')
                    .to_string();
                Some((inner, e.path()))
            } else {
                None
            }
        })
        .collect();
    cands.sort();
    cands.into_iter().next()
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

/// 段是否为 `_name_` 形态（fs 动态参数写法）。宽松判定，仅用于告警提示。
pub fn looks_like_underscore_param(seg: &str) -> bool {
    seg.len() > 2 && seg.starts_with('_') && seg.ends_with('_')
}

/// fs 段 → URL pattern 段（v0.1.27 `_name_` 约定）：
/// 仅整段 `_name_` 转换——len>2、首尾各一个 `_`，且 inner 不以 `_` 开头/结尾
/// （`__x__`/`___`/`_a__` 不转，宁枉勿纵）、inner 不含 `{`/`}`
/// （`_a{b}_` 不转，提前挡掉而非留给 matchit 启动期报错）。
/// `_`、`__`、`_a`、`a_`、`_shared`、`a_bb_c` 保持字面。
/// 下划线是 ASCII，`seg[1..len-1]` 恒为合法字符边界。
pub fn fs_seg_to_pattern(seg: &str) -> String {
    if looks_like_underscore_param(seg) {
        let inner = &seg[1..seg.len() - 1];
        if !inner.starts_with('_') && !inner.ends_with('_') && !inner.contains(['{', '}']) {
            return format!("{{{inner}}}");
        }
    }
    seg.to_string()
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
///
/// **值不放在 matcher 里**：matcher 只持 `Vec` 下标（slot），真正的 方法名 → Entry
/// 映射存在 `nodes` 里，pattern 字符串 ↔ slot 由 `slots` 记录。同 pattern 去重因此
/// 走得 route 字符串相等，而不是 `matcher.at_mut(pattern)`——后者是**路径匹配**：
/// 先注册 `/x/{pk}` 时，`at_mut("/x/me")` 会把 `me` 当实参匹配成功，把静态兄弟的方法
/// 嫁接到 `{pk}` 节点上（静态段被参数段吞掉 / 同动词静态之间假冲突）。
#[derive(Clone)]
pub struct RouteTable {
    matcher: matchit::Router<usize>,
    /// pattern（注册期的字面量，含参数花括号）→ nodes 下标。
    slots: HashMap<String, usize>,
    /// 节点表：方法名 → Entry；与 matcher 的 value（下标）一一对应。
    nodes: Vec<HashMap<String, Entry>>,
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
            slots: HashMap::new(),
            nodes: Vec::new(),
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
        let mut t = RouteTable::default();
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
            // v0.1.27：fs 段 `_name_` → `{name}`（rel 已归一为正斜杠；逐段转换，含模块段）。
            let dir_base = if rel.is_empty() {
                format!("/{b}")
            } else {
                let conv = rel
                    .split('/')
                    .map(fs_seg_to_pattern)
                    .collect::<Vec<_>>()
                    .join("/");
                format!("/{b}/{conv}")
            };
            for (method, route) in decls {
                if !METHODS.contains(&method.as_str()) {
                    continue;
                }
                let route = route.filter(|r| !r.is_empty()); // "" 视同未挂
                if let Some(r) = &route {
                    // v0.1.27 告警：`_name_` 是 fs 目录写法；`.route` 值是 matchit 语法，
                    // `_name_` 在其中是字面段。warning: 前缀由消费方分流（见 app.rs 打印循环）。
                    if r.split('/').any(looks_like_underscore_param) {
                        failures.push(format!(
                            "warning: {method} {}: .route value {r:?} looks like `_name_` fs-param syntax; in .route it stays a literal segment (use {{name}} for params)",
                            file.display()
                        ));
                    }
                }
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
        let mut t = RouteTable::default();
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
        // 同 pattern 去重按 **字符串相等**（查 slots），不用 matcher.at_mut(pattern)：
        // 后者是路径匹配，会把 `/x/me` 当成 `/x/{pk}` 的实参，嫁接到参数节点上。
        if let Some(&slot) = self.slots.get(pattern) {
            let map = &mut self.nodes[slot];
            match map.get(method) {
                Some(Entry::File(a)) => {
                    let msg = format!(
                        "route conflict: {method} {pattern} declared in {} and {}",
                        self.files[a.0 as usize].display(),
                        file.display()
                    );
                    map.insert(method.to_string(), Entry::Conflict(msg.clone()));
                    failures.push(msg);
                }
                // 冲突**钉死**：第三方再声明不得把它冲回 File（否则 500 静默变 200，
                // 且指向第三个文件）。仍记 failures，方便运维看到到底有几个文件打架。
                Some(Entry::Conflict(_)) => failures.push(format!(
                    "route conflict: {method} {pattern} declared in more than two files (also {})",
                    file.display()
                )),
                _ => {
                    map.insert(method.to_string(), Entry::File(fid));
                    self.rows.push(RouteRow {
                        method: method.to_string(),
                        pattern: pattern.to_string(),
                        file: fid,
                    });
                }
            }
            return;
        }
        let mut map = HashMap::new();
        map.insert(method.to_string(), Entry::File(fid));
        let slot = self.nodes.len();
        match self.matcher.insert(pattern.to_string(), slot) {
            Ok(()) => {
                self.slots.insert(pattern.to_string(), slot);
                self.nodes.push(map);
                self.rows.push(RouteRow {
                    method: method.to_string(),
                    pattern: pattern.to_string(),
                    file: fid,
                });
            }
            // 非法语法 / 结构性冲突（同位置异名参数）：日志丢弃后来者
            Err(e) => failures.push(format!(
                "invalid route {method} {pattern} from {}: {e}",
                file.display()
            )),
        }
    }

    /// 查表：path 须先经 `normalize`。未映射动词按"路径存在 → 405"契约处理。
    pub fn lookup(&self, path: &str, verb: &str) -> Lookup {
        // 精确段（静态）优先于参数段：matchit 自带该优先级，与注册顺序无关。
        let Ok(m) = self.matcher.at(path) else {
            return Lookup::NotFound;
        };
        let Some(name) = method_name(verb) else {
            return Lookup::MethodNotAllowed;
        };
        match self.nodes[*m.value].get(name) {
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
            r.resolve("/v1/api/user/account/").map(|(f, _)| f),
            Some(root.join("user/account/api.ts"))
        );
        assert_eq!(
            r.resolve("/v1/api/user/account").map(|(f, _)| f),
            Some(root.join("user/account/api.ts"))
        );
        assert_eq!(
            r.resolve("/v1/api/user/profile/detail/").map(|(f, _)| f),
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
    fn resolve_underscore_dir_descends_with_params() {
        let root = fixture(&["a/_id_/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        match r.resolve("/v1/api/a/42") {
            Some((f, p)) => {
                assert_eq!(f, root.join("a/_id_/api.ts"));
                assert_eq!(p["id"], "42");
            }
            None => panic!("expected hit"),
        }
        // 字面 `_id_` URL：兜底快路径命中磁盘字面目录（无参数）——生产上流
        // 程表查询在前，注册过的 `_id_` 目录由 `{id}` 参数吃掉（见
        // table_underscore_dir_becomes_param）；兜底只服务表外文件，保持字面优先。
        let (f, p) = r.resolve("/v1/api/a/_id_").unwrap();
        assert_eq!(f, root.join("a/_id_/api.ts"));
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn resolve_descend_literal_first_and_backtracks() {
        // 字面目录优先：a/me/api.ts 存在时 /a/me 命中它（无参数）。
        let root = fixture(&["a/_id_/api.ts", "a/me/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        let (f, p) = r.resolve("/v1/api/a/me").unwrap();
        assert_eq!(f, root.join("a/me/api.ts"));
        assert!(p.is_empty(), "{p:?}");
        // 回溯：字面 a/b/ 存在但子树死路，回退到 _x_ 候选。
        let root2 = fixture(&["a/_x_/c/api.ts"]);
        std::fs::create_dir_all(root2.join("a/b")).unwrap();
        let r2 = Routes::new("/v1/api", &root2, true);
        let (f2, p2) = r2.resolve("/v1/api/a/b/c").unwrap();
        assert_eq!(f2, root2.join("a/_x_/c/api.ts"));
        assert_eq!(p2["x"], "b");
    }

    #[test]
    fn resolve_descend_first_candidate_only_and_smuggling() {
        // 同层多个 _x_：只试排序首个（对齐建表期同位异名丢弃语义）。
        let root = fixture(&["a/_aa_/api.ts", "a/_bb_/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        let (f, p) = r.resolve("/v1/api/a/zz").unwrap();
        assert_eq!(f, root.join("a/_aa_/api.ts"));
        assert_eq!(p["aa"], "zz");
        // 走私：%2e%2e / a%2Fb 经参数段 → decode_params 拒 → 404
        assert!(r.resolve("/v1/api/a/%2e%2e").is_none());
        assert!(r.resolve("/v1/api/a/a%2Fb").is_none());
    }

    #[test]
    fn resolve_deep_mixed_underscore_dirs() {
        // PRD 原例形态（含模块段 _aa_）。
        let root = fixture(&["_aa_/bb/_cc_/api.ts"]);
        let r = Routes::new("/v1/api", &root, true);
        let (f, p) = r.resolve("/v1/api/11/bb/22").unwrap();
        assert_eq!(f, root.join("_aa_/bb/_cc_/api.ts"));
        assert_eq!(p["aa"], "11");
        assert_eq!(p["cc"], "22");
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

    // ----- fs 段转换（v0.1.27 `_name_` 约定） -----

    #[test]
    fn fs_seg_conversion_rules() {
        // 转换：整段 `_name_`，inner 可含下划线
        assert_eq!(fs_seg_to_pattern("_aa_"), "{aa}");
        assert_eq!(fs_seg_to_pattern("_a_b_"), "{a_b}");
        // 不转换（原样返回）
        for lit in [
            "_", "__", "_a", "a_", "__x__", "___", "_a{b}_", "_shared", "a_bb_c", "plain",
        ] {
            assert_eq!(fs_seg_to_pattern(lit), lit, "{lit}");
        }
        // 宽松判定（告警用）与严格转换的区别
        assert!(looks_like_underscore_param("__x__")); // 宽松命中……
        assert_eq!(fs_seg_to_pattern("__x__"), "__x__"); // ……但严格转换拒绝
    }

    #[test]
    fn table_underscore_dir_becomes_param() {
        let (t, f) = tbl(&["user/_id_/api.ts"], &[("user/_id_/api.ts", "get", "")]);
        assert!(f.is_empty(), "{f:?}");
        match t.lookup("/v1/api/user/42", "GET") {
            Lookup::Hit { file, params } => {
                assert!(file.ends_with("_id_/api.ts"), "{file:?}");
                assert_eq!(params["id"], "42");
            }
            other => panic!("{}", kind(&other)),
        }
    }

    #[test]
    fn table_underscore_dir_static_sibling_priority() {
        let (t, f) = tbl(
            &["x/_pk_/api.ts", "x/me/api.ts"],
            &[("x/_pk_/api.ts", "get", ""), ("x/me/api.ts", "get", "")],
        );
        assert!(f.is_empty(), "{f:?}");
        match t.lookup("/v1/api/x/me", "GET") {
            Lookup::Hit { file, params } => {
                assert!(file.ends_with("x/me/api.ts"), "{file:?}");
                assert!(params.is_empty(), "{params:?}");
            }
            other => panic!("{}", kind(&other)),
        }
        match t.lookup("/v1/api/x/42", "GET") {
            Lookup::Hit { params, .. } => assert_eq!(params["pk"], "42"),
            other => panic!("{}", kind(&other)),
        }
    }

    #[test]
    fn table_same_level_underscore_params_conflict() {
        // 同层异名 `_x_` = matchit 同位异名参数 → 结构性冲突，后者丢弃。
        let (t, f) = tbl(
            &["u/_aa_/api.ts", "u/_bb_/api.ts"],
            &[("u/_aa_/api.ts", "get", ""), ("u/_bb_/api.ts", "get", "")],
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].contains("invalid route"), "{f:?}");
        assert!(matches!(t.lookup("/v1/api/u/1", "GET"), Lookup::Hit { .. }));
    }

    #[test]
    fn table_underscore_dir_plus_relative_route() {
        let (t, f) = tbl(&["u/_id_/api.ts"], &[("u/_id_/api.ts", "get", "{sub}")]);
        assert!(f.is_empty(), "{f:?}");
        match t.lookup("/v1/api/u/42/99", "GET") {
            Lookup::Hit { params, .. } => {
                assert_eq!(params["id"], "42");
                assert_eq!(params["sub"], "99");
            }
            other => panic!("{}", kind(&other)),
        }
    }

    #[test]
    fn table_brace_dir_conflicts_with_underscore_dir() {
        // `{id}` 字面目录今天已意外可用；与 `_id_` 同 pattern 同方法 → 冲突钉死 500。
        let (t, f) = tbl(
            &["u/{id}/api.ts", "u/_id_/api.ts"],
            &[("u/{id}/api.ts", "get", ""), ("u/_id_/api.ts", "get", "")],
        );
        assert!(f.iter().any(|s| s.contains("route conflict")), "{f:?}");
        assert!(matches!(
            t.lookup("/v1/api/u/1", "GET"),
            Lookup::Conflict(_)
        ));
    }

    #[test]
    fn table_route_value_underscore_shape_warns() {
        // `.route = "_id_"` 是字面段（不转换），但须给出 warning: 前缀告警。
        let (t, f) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "_id_")]);
        assert!(f.iter().any(|s| s.starts_with("warning:")), "{f:?}");
        match t.lookup("/v1/api/u/_id_", "GET") {
            Lookup::Hit { params, .. } => assert!(params.is_empty(), "{params:?}"),
            other => panic!("{}", kind(&other)),
        }
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

    // ----- 参数路由与静态兄弟的优先级（注册顺序无关） -----

    #[test]
    fn table_param_route_does_not_graft_later_static_siblings() {
        // 缺陷形态（upstream issue: matchit grafting）：参数路由先注册时，后到的静态兄弟
        // 曾被去重用的 at_mut(pattern) 当成实参匹配，把方法嫁接到 {pk} 节点上。
        let root = PathBuf::from("/r");
        let es = |m: &str, p: &str, f: &str| RouteEntry {
            method: m.into(),
            pattern: p.into(),
            file: f.into(),
        };
        let (t, failures) = RouteTable::from_entries(
            &root,
            &[
                es("del", "/x/admins/{pk}", "pk/api.js"),
                es("get", "/x/admins/me", "me/api.js"),
                es("get", "/x/admins/session", "session/api.js"),
                es("post", "/x/admins/sign-in", "sign-in/api.js"),
            ],
        );
        assert!(failures.is_empty(), "{failures:?}");
        let hit = |path: &str, verb: &str, expect: &str| match t.lookup(path, verb) {
            Lookup::Hit { file, .. } => assert_eq!(file, PathBuf::from(expect), "{path} {verb}"),
            other => panic!("{path} {verb} → {}", kind(&other)),
        };
        hit("/x/admins/me", "GET", "/r/me/api.js");
        hit("/x/admins/session", "GET", "/r/session/api.js");
        hit("/x/admins/sign-in", "POST", "/r/sign-in/api.js");
        hit("/x/admins/42", "DELETE", "/r/pk/api.js");
        // 真实静态段优先：me 不得被 {pk} 吞掉（反之亦然）
        assert!(matches!(
            t.lookup("/x/admins/me", "DELETE"),
            Lookup::MethodNotAllowed
        ));
    }

    #[test]
    fn table_static_sibling_beats_same_verb_param() {
        // 最凶的一形态：参数与静态**同动词**。旧实现里 `{pk}` 先注册会把 `me` 判成
        // 假冲突（`at_mut` 把 me 当实参），该 (pattern, method) 直接 500。
        let (t, failures) = tbl(
            &["a/api.ts", "b/api.ts"],
            &[("a/api.ts", "get", "/x/{pk}"), ("b/api.ts", "get", "/x/me")],
        );
        assert!(failures.is_empty(), "{failures:?}");
        match t.lookup("/v1/api/x/me", "GET") {
            // 静态命中不得带参数（被嫁接时会命中 {pk} 节点、带上 pk=me）
            Lookup::Hit { file, params } => {
                assert!(file.ends_with("b/api.ts"), "{file:?}");
                assert!(params.is_empty(), "{params:?}");
            }
            other => panic!("{}", kind(&other)),
        }
        match t.lookup("/v1/api/x/42", "GET") {
            Lookup::Hit { file, params } => {
                assert!(file.ends_with("a/api.ts"), "{file:?}");
                assert_eq!(params["pk"], "42");
            }
            other => panic!("{}", kind(&other)),
        }
    }

    #[test]
    fn table_conflict_survives_third_declaration() {
        // 冲突哨兵必须钉死：第三个文件再声明同一 (pattern, method) 不得把它冲回 File
        // （否则请求从 500 静默变 200，且指向第三个文件）。
        let root = PathBuf::from("/r");
        let (t, failures) = RouteTable::from_entries(
            &root,
            &[
                RouteEntry {
                    method: "get".into(),
                    pattern: "/a/{id}".into(),
                    file: "a/api.js".into(),
                },
                RouteEntry {
                    method: "get".into(),
                    pattern: "/a/{id}".into(),
                    file: "b/api.js".into(),
                },
                RouteEntry {
                    method: "get".into(),
                    pattern: "/a/{id}".into(),
                    file: "c/api.js".into(),
                },
            ],
        );
        assert_eq!(failures.len(), 2, "{failures:?}");
        match t.lookup("/a/1", "GET") {
            Lookup::Conflict(msg) => assert!(msg.contains("a/api.js"), "{msg}"),
            other => panic!("{}", kind(&other)),
        }
    }

    #[test]
    fn table_differing_param_names_at_same_slot_conflict() {
        // 同位置**异名**参数是结构性冲突（设计 §5）：后来者不进树。
        // 修复前靠 at_mut 命中同类节点而被静默合并，与文档口径不符。
        let (t, failures) = tbl(
            &["a/api.ts", "b/api.ts"],
            &[
                ("a/api.ts", "get", "/x/{id}"),
                ("b/api.ts", "get", "/x/{name}"),
            ],
        );
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("invalid route"), "{failures:?}");
        assert!(matches!(t.lookup("/v1/api/x/1", "GET"), Lookup::Hit { .. }));
    }

    #[test]
    fn table_build_keeps_static_siblings_registered_after_param() {
        // dev/release 共用的 build 路径：文件按路径排序，a→b→c 即「参数在前、静态在后」。
        let (t, failures) = tbl(
            &["a/api.ts", "b/api.ts", "c/api.ts", "d/api.ts"],
            &[
                ("a/api.ts", "del", "/admins/{pk}"),
                ("b/api.ts", "get", "/admins/me"),
                ("c/api.ts", "get", "/admins/session"),
                ("d/api.ts", "post", "/admins/sign-in"),
            ],
        );
        assert!(failures.is_empty(), "{failures:?}");
        let hit = |path: &str, verb: &str, expect: &str| match t.lookup(path, verb) {
            Lookup::Hit { file, .. } => assert!(file.ends_with(expect), "{file:?} vs {expect}"),
            other => panic!("{path} {verb} → {}", kind(&other)),
        };
        hit("/v1/api/admins/me", "GET", "b/api.ts");
        hit("/v1/api/admins/session", "GET", "c/api.ts");
        hit("/v1/api/admins/sign-in", "POST", "d/api.ts");
        hit("/v1/api/admins/42", "DELETE", "a/api.ts");
        // rows/listing 须与查表一致（嫁接时 rows 会列出实际不在树里的行）
        assert_eq!(t.listing().len(), 4, "{:?}", t.listing().len());
    }

    fn kind(l: &Lookup) -> &'static str {
        match l {
            Lookup::Hit { .. } => "Hit",
            Lookup::Conflict(_) => "Conflict",
            Lookup::MethodNotAllowed => "MethodNotAllowed",
            Lookup::NotFound => "NotFound",
        }
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
