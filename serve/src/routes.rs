#![allow(
    clippy::type_complexity,
    clippy::collapsible_if,
    clippy::redundant_closure
)]

//! 任意深度；无 api 文件的目录不是路由（可作纯工具代码目录）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use only_js::bridge::ModuleIndex;

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

/// 单 pattern 下某方法的归宿：文件 + handler 链 / 冲突（请求期 500）。
/// v0.1.64：链为点分路径（`"cccc.get"`；顶层动词即 `"get"`），`run_module` 据此逐段取值。
#[derive(Clone)]
pub enum Entry {
    File(FileId, String),
    Conflict(String),
}

/// 查表结果四态（handle 据此映射 200/500/405/404）。
pub enum Lookup {
    Hit {
        file: PathBuf,
        params: HashMap<String, String>,
        /// handler 链（点分；末段即动词）。派发凭此取值，不再用 method 反查。
        handler: String,
    },
    Conflict(String),
    MethodNotAllowed,
    NotFound,
}

/// 启动打印行（method × pattern × file_id × 契约）。
#[derive(Clone)]
pub struct RouteRow {
    pub method: String,
    pub pattern: String,
    pub file: FileId,
    /// handler 链（点分；`method` 即其末段）。契约注册与 openapi 同源。
    pub handler: String,
    /// handler 声明的入参契约（`.schema`）；None = 未声明（不校验）。
    /// **由路由表单一承载** —— dev 内省与 release `routes.js` 都落到此处，
    /// 契约的三个消费者（运行期校验 / oj build 落盘 / oj openapi 出契约）同读一源。
    pub schema: Option<serde_json::Value>,
}

/// handler 链段名合法性（§四 权威规则，与 JS driver 同口径）：非空、不含 `/`、
/// 排除 `__proto__` / `constructor` / `prototype`（原型链防护）。段含 `.` 不可能
/// （链按 `.` 拆分）；连字符等非标识符段合法（计算式访问天然兼容）。
pub fn legal_handler_chain(chain: &str) -> bool {
    !chain.is_empty()
        && chain.split('.').all(|s| {
            !s.is_empty()
                && !s.contains('/')
                && s != "__proto__"
                && s != "constructor"
                && s != "prototype"
        })
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
    /// api 根（dev 的 `src` / release 的 `dist`）：v0.1.70 起留存，供
    /// `rel_file`（对外产物不带绝对路径）与 `module_of`（模块归属）使用。
    root: PathBuf,
    /// release 直载布局（`from_entries`=true / `build`=false）：`module_of` 是否剥
    /// 首段 `<module>-<version>` 的版本后缀。dev 模块目录允许同形名（如
    /// `preview-0.9`），误剥会把 openapi tags / 生成客户端归错模块（v0.1.83）。
    release: bool,
    /// 模块索引（`--module-root` 的递归发现结果；None = 今日首层语义）：模块归属与
    /// URL 前缀都由它给，避免首段（目录名）被当成模块名（v0.1.85）。
    modules: Option<ModuleIndex>,
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
            root: PathBuf::new(),
            release: false,
            modules: None,
        }
    }
}

impl RouteTable {
    /// 建表：内省闭包按文件返回 Vec<(handler 链, 有效 .route 或 None, schema)>；
    /// 返回 (表, 失败/冲突清单)。纯逻辑（依赖倒置：不依赖 JS 运行时），
    /// CLI/测试注入真实或假内省。v0.1.64：链来自 `introspect_module` 的扁平 leaf
    /// （组前缀已在 JS 侧解析为有效路由）；动词取链尾，METHODS 白名单作纵深防线。
    ///
    /// `index`（仅 `--module-root` 下有值）：模块根可不在挂载根首层时，路由的模块段
    /// 与模块内路径**只能**由索引给出（`dev_rel`）——否则 dev 会按挂载根拼出
    /// `/{base}/foo/api/xxx/yyy`，而 release（`build_cmd::rel_pattern`）以模块根为基，
    /// 两侧 URL 分叉。None = 今日语义（相对挂载 api 根，flat 布局下逐字等价）。
    /// 注册 pattern 全集（含 base 与 `{param}` 段；slots 的键集）。U-18 批 T1.1：
    /// 装配期 route_timeouts 校验的匹配集来源。
    pub fn patterns(&self) -> Vec<&str> {
        self.slots.keys().map(String::as_str).collect()
    }

    pub fn build(
        base: &str,
        root: &Path,
        ts: bool,
        index: Option<&ModuleIndex>,
        introspect: impl Fn(
            &Path,
        )
            -> Result<Vec<(String, Option<String>, Option<serde_json::Value>)>, String>,
    ) -> (Self, Vec<String>) {
        let b = base.trim_matches('/');
        let mut failures = Vec::new();
        let mut t = RouteTable {
            root: root.to_path_buf(),
            modules: index.cloned(),
            ..Default::default()
        };
        for file in api_files(root, ts) {
            let decls = match introspect(&file) {
                Ok(d) => d,
                Err(e) => {
                    failures.push(format!("{}: {e}", file.display()));
                    continue;
                }
            };
            let rel = mirror_rel(root, &file, index);
            // v0.1.27：fs 段 `_name_` → `{name}`（rel 已归一为正斜杠；逐段转换，含模块段）。
            let conv = rel
                .split('/')
                .map(fs_seg_to_pattern)
                .collect::<Vec<_>>()
                .join("/");
            let dir_base = join_url(b, &conv);
            for (chain, route, schema) in decls {
                // 动词 = 链尾（内省保证只有 verb 键成 leaf；此处为纵深防线）。
                let method = chain.rsplit('.').next().unwrap_or(&chain);
                if !METHODS.contains(&method) {
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
                    Some(r) if r.starts_with('/') => join_url(b, r), // 根级（base 根下）
                    Some(r) => join_url(&dir_base, r),               // 相对
                };
                // 有效路由非空 = 声明了显式 pattern（自身或组 .route）→ 该 (file, verb)
                // 的目录镜像不得被 dev 兜底复活。顶层动词无 .route 不标记（旧语义）。
                if route.is_some() {
                    let fid = t.intern(&file);
                    t.replaced.insert((fid, method.to_string()));
                }
                t.register(
                    &mut failures,
                    method,
                    &pattern,
                    &file,
                    schema.clone(),
                    &chain,
                );
            }
        }
        (t, failures)
    }

    /// release 直载：routes.js 导出的全量行（pattern 已含 base，file 相对 root）。
    /// 注册语义与 build 一致（合并 / 冲突 / 非法 pattern 丢弃），replaced 恒空（无 fs 兜底）。
    /// handler 链：行内 `handler` 字段优先；旧产物缺字段 → 以 `method` 兜底填链
    /// （单段链即旧行为，老 dist 不必重新 build 照常启动）。
    pub fn from_entries(root: &Path, entries: &[RouteEntry]) -> (Self, Vec<String>) {
        let mut t = RouteTable {
            root: root.to_path_buf(),
            release: true,
            ..Default::default()
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
            // handler 链解析（A6：release 入口段名校验，与 §四 白名单同口径）。
            let chain = e.handler.clone().unwrap_or_else(|| e.method.clone());
            if !legal_handler_chain(&chain) {
                failures.push(format!(
                    "routes.js: illegal handler chain {:?} (method {}, pattern {})",
                    chain, e.method, e.pattern
                ));
                continue;
            }
            t.register(
                &mut failures,
                &e.method,
                &e.pattern,
                &root.join(&e.file),
                e.schema.clone(),
                &chain,
            );
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
    fn register(
        &mut self,
        failures: &mut Vec<String>,
        method: &str,
        pattern: &str,
        file: &Path,
        schema: Option<serde_json::Value>,
        handler: &str,
    ) {
        let fid = self.intern(file);
        // 同 pattern 去重按 **字符串相等**（查 slots），不用 matcher.at_mut(pattern)：
        // 后者是路径匹配，会把 `/x/me` 当成 `/x/{pk}` 的实参，嫁接到参数节点上。
        if let Some(&slot) = self.slots.get(pattern) {
            let map = &mut self.nodes[slot];
            match map.get(method) {
                Some(Entry::File(a, _)) => {
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
                    map.insert(method.to_string(), Entry::File(fid, handler.to_string()));
                    self.rows.push(RouteRow {
                        method: method.to_string(),
                        pattern: pattern.to_string(),
                        file: fid,
                        handler: handler.to_string(),
                        schema: schema.clone(),
                    });
                }
            }
            return;
        }
        let mut map = HashMap::new();
        map.insert(method.to_string(), Entry::File(fid, handler.to_string()));
        let slot = self.nodes.len();
        match self.matcher.insert(pattern.to_string(), slot) {
            Ok(()) => {
                self.slots.insert(pattern.to_string(), slot);
                self.nodes.push(map);
                self.rows.push(RouteRow {
                    method: method.to_string(),
                    pattern: pattern.to_string(),
                    file: fid,
                    handler: handler.to_string(),
                    schema: schema.clone(),
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
            Some(Entry::File(f, handler)) => {
                let pairs = m.params.iter().map(|(k, v)| (k.to_string(), v.to_string()));
                match decode_params(pairs) {
                    Some(params) => Lookup::Hit {
                        file: self.files[f.0 as usize].clone(),
                        params,
                        handler: handler.clone(),
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

    /// FileId → **相对 api 根**的路径（v0.1.70）。
    ///
    /// 对外产物（openapi.json 的 `x-oj-file`）走这里：产物是公开文件，不该带本机
    /// 绝对路径（信息泄露，且换台机器就不可比）。
    pub fn rel_file(&self, id: FileId) -> &Path {
        let p = self.file_path(id);
        p.strip_prefix(&self.root).unwrap_or(p)
    }

    /// 路由所属**模块名**（v0.1.70）：供 `oj openapi` 出 `tags`，下游据此把
    /// `__client` 分模块落盘（生成代码必须知道归属，光有 operationId 不够）。
    ///
    /// - 有索引（`--module-root`）：归属由索引按最长前缀给出 —— 模块根可不在首层，
    ///   首段是目录名（`foo`），而下游分组要的是 `manifest.yaml` 的 `name`（v0.1.85）；
    /// - dev（无索引，`build` 建表）：`src/<module>/<...>/api.ts` 的首段**原样保留**；
    /// - release（`from_entries` 建表）：`routes.js` 的 `<module>-<version>/...`
    ///   首段剥掉版本后缀（v0.1.83 起仅 release 剥——dev 的 `preview-0.9` 这类
    ///   同形模块名此前被误剥成 `preview`）。
    ///
    /// 只剩一层（api 根的裸 `api.ts` / `api.js`）→ None（无模块归属）。
    pub fn module_of(&self, id: FileId) -> Option<String> {
        if let Some(idx) = &self.modules {
            // 索引认领该文件 → 模块名即身份（与 URL 前缀、产物目录同源）。
            if let Some((name, _, _)) = idx.locate(self.file_path(id)) {
                return Some(name.to_string());
            }
        }
        let rel = self.rel_file(id);
        let mut it = rel.components();
        let first = it.next()?.as_os_str().to_string_lossy().into_owned();
        it.next()?; // 根的裸 api 文件，不属于任何模块
        if self.release {
            // `<module>-<version>`：尾段是「数字与点」且含点才剥（模块名本身允许
            // 连字符，`my-module` 的尾段 `module` 不是版本号，不会被误剥）。
            if let Some((m, v)) = first.rsplit_once('-') {
                if !m.is_empty()
                    && v.contains('.')
                    && v.chars().all(|c| c.is_ascii_digit() || c == '.')
                {
                    return Some(m.to_string());
                }
            }
        }
        Some(first)
    }

    /// 全表 `.schema` **声明合法性审计**（v0.1.70 唯一入口）。
    ///
    /// 消费路径必须调用它（当前三处：`oj serve` 装配 / `oj build` 落盘 /
    /// `oj openapi` 生成）。**release 的 `routes.js` 可能来自第三方 `.tgz` 模块**——
    /// 那里的 `.schema` 是构建期不可信输入，不能只靠「本仓 build 时审过」。
    /// 审计项与报错文案见 `only_js::contract::audit_schema`。
    pub fn audit_schemas(&self) -> Result<(), String> {
        for row in &self.rows {
            let Some(sv) = &row.schema else { continue };
            only_js::contract::audit_schema(sv).map_err(|e| {
                format!(
                    "invalid `.schema` on {} {} (handler `{}`, {}): {e}",
                    row.method,
                    row.pattern,
                    row.handler,
                    self.rel_file(row.file).display()
                )
            })?;
        }
        Ok(())
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

/// 目录镜像的「模块内相对路径」（含 URL 前缀段；已归一为正斜杠）。
///
/// api.ts 与 ws.ts **同源**——WS 的目录镜像曾各算一套（`file.parent()` 相对挂载根），
/// 在 `--module-root` 布局下比 api.ts 多出一层布局段（`src/wiki/api/ws.ts` 挂成
/// `/api/wiki/api/ws`，v0.1.87 修）。任何新增的目录镜像面都必须走本函数。
///
/// - 索引命中：`{prefix}/{模块根内目录}`——目录名（`foo/api`）不进 URL，模块段取
///   `manifest.yaml` 的 `prefix`（缺省 = 模块名），与 release 的 `rel_pattern` 同源；
/// - 索引缺失或未命中该文件（模块外的 `tasks/`、挂载根散文件）→ 今日语义：
///   相对挂载 api 根（flat 布局下与上一条逐字等价）。
pub(crate) fn mirror_rel(root: &Path, file: &Path, index: Option<&ModuleIndex>) -> String {
    let Some(idx) = index else {
        return plain_rel(root, file);
    };
    let Some((_, e, rel)) = idx.locate(file) else {
        return plain_rel(root, file);
    };
    let inner = rel
        .parent()
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");
    if inner.is_empty() {
        e.prefix.clone()
    } else {
        format!("{}/{inner}", e.prefix)
    }
}

/// 今日语义：api 文件所在目录相对挂载 api 根（flat 布局下模块名段即首段）。
fn plain_rel(root: &Path, file: &Path) -> String {
    file.parent()
        .and_then(|p| p.strip_prefix(root).ok())
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .replace('\\', "/")
}

/// URL 段拼接（base + 目录镜像相对段）：逐段过滤空段，恒以单个 `/` 开头。
///
/// 挂载前缀可为根（`/`，`resolve_app_prefix` 允许）——裸 `format!("/{base}/{rel}")`
/// 会拼出 `//ws` 这类双斜杠路径（注册成功却永远匹配不到）。api.ts 与 ws.ts 共用。
pub(crate) fn join_url(base: &str, rel: &str) -> String {
    let segs: Vec<&str> = base
        .split('/')
        .chain(rel.split('/'))
        .filter(|s| !s.is_empty())
        .collect();
    if segs.is_empty() {
        return "/".to_string();
    }
    format!("/{}", segs.join("/"))
}

/// root 下全部 api 文件（排序 → 冲突裁决顺序确定）。
fn api_files(root: &Path, ts: bool) -> Vec<PathBuf> {
    let ext = if ts { "api.ts" } else { "api.js" };
    let mut out = Vec::new();
    walk_files(root, ext, &mut out);
    out.sort();
    out
}

/// 递归收集 `ext` 文件。剪枝与模块发现同口径（`module_scan::scan_recursive`）：
/// 跳过点目录与 `node_modules`、**不跟随目录符号链接**——挂载根下的
/// `node_modules/**/ws.ts`（或被引到仓库外的链接目录）不该变成一条对外端点。
pub(crate) fn walk_files(dir: &Path, ext: &str, acc: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let p = e.path();
        // file_type() 不跟随链接：`is_symlink()` 的目录链接整棵跳过。
        let Ok(ft) = e.file_type() else {
            continue;
        };
        if ft.is_dir() {
            if ft.is_symlink() || name.starts_with('.') || name == "node_modules" {
                continue;
            }
            walk_files(&p, ext, acc);
        } else if name == ext {
            acc.push(p);
        }
    }
}

/// 构建期 pattern 校验（oj build fail-fast，v0.1.27）：逐条插入临时 matchit Router
/// （补首斜杠），非法语法 / 同位异名参数 → Err。此前这类错误要到部署启动才爆
/// （release 启动对 from_entries failures 硬失败）。重复 pattern（多方法）可合并。
pub fn check_patterns(patterns: &[String]) -> Result<(), String> {
    let mut r = matchit::Router::new();
    // matchit 对完全相同的 path 重复 insert 也报 Conflict——先按字符串去重
    // （与 RouteTable 的 slots 去重同口径；多方法共享 pattern 是常态）。
    let mut seen = std::collections::HashSet::new();
    for p in patterns {
        let full = format!("/{}", p.trim_start_matches('/'));
        if !seen.insert(full.clone()) {
            continue;
        }
        r.insert(full.clone(), ())
            .map_err(|e| format!("invalid route pattern {full:?}: {e}"))?;
    }
    Ok(())
}

/// routes.js 导出行（oj build 生成；release 直载免内省）。
#[derive(Clone, Default)]
pub struct RouteEntry {
    pub method: String,
    pub pattern: String,
    pub file: String,
    /// handler 链（v0.1.64 嵌套组；点分，如 `"cccc.get"`）。**旧产物无此字段**
    /// → None（向后兼容：`from_entries` 以 `method` 兜底填链，老 dist 照常启动）。
    pub handler: Option<String>,
    /// handler 声明的入参契约（`.schema`；v0.1.44）。旧产物**无此字段** → None
    /// （向后兼容：`routes.js` 缺 schema 视为未声明契约，不得因此拒启）。
    pub schema: Option<serde_json::Value>,
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
                        handler: match e.get("handler") {
                            Some(serde_json::Value::Null) | None => None,
                            Some(h) => h.as_str().map(|s| s.to_string()),
                        },
                        schema: match e.get("schema") {
                            Some(serde_json::Value::Null) | None => None,
                            Some(v) => Some(v.clone()),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 内省结果 Value（v0.1.64 `introspect_module` 约定：扁平 leaf 数组
/// `[{ chain, route, schema }, ...]`）→ (handler 链, 有效 route, schema) 三元组。
/// **route 与 schema 同源取一趟**，杜绝两处真源漂移。
pub fn decls_from_value(
    v: &serde_json::Value,
) -> Vec<(String, Option<String>, Option<serde_json::Value>)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let o = l.as_object()?;
                    let chain = o.get("chain")?.as_str()?.to_string();
                    let route = o
                        .get("route")
                        .and_then(|r| r.as_str())
                        .map(|s| s.to_string());
                    let schema = match o.get("schema") {
                        Some(serde_json::Value::Null) | None => None,
                        Some(s) => Some(s.clone()),
                    };
                    Some((chain, route, schema))
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
) -> impl Fn(&Path) -> Result<Vec<(String, Option<String>, Option<serde_json::Value>)>, String> {
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
#[path = "routes_tests.rs"]
mod tests;
