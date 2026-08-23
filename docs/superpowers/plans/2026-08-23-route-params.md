# 路径参数路由实现计划（Route Params）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/route-params-design.md`（已两轮评审修正）实现方法级 `.route` 路径参数路由：启动内省建表 + matchit 匹配 + dev 文件系统兜底。

**Architecture:** `server/src/routes.rs` 持有纯逻辑 `RouteTable`（单 matcher + 方法映射值，冲突写进 value）；`src/bridge/mod.rs` 复用 `run_module` 管道加内省 driver（`introspect_module`）；`server/src/lib.rs` 的 `handle` 改查表，dev 模式保留旧 `Routes::resolve` 兜底；CLI 在 actor 池前建表并打印。路由逻辑与 JS 运行时经闭包解耦（依赖倒置），`RouteTable::build` 接受任意内省函数，纯逻辑可无 JS 测试。

**Tech Stack:** Rust 2024 / axum 0.8 / matchit 0.8.4（语法 `{id}`、`{*p}`）/ percent-encoding 2.3 / form_urlencoded 1.2（三者均已在 Cargo.lock）/ deno_core 0.410。

## Global Constraints

- 不新增 lockfile 外依赖；`matchit`/`percent-encoding`/`form_urlencoded` 仅在 `server/Cargo.toml` 声明。
- 用户侧 `.route` 语法 = matchit 原生 `{name}` / `{*name}`，**零翻译层**。
- 每任务 TDD：先写失败测试→跑→实现→跑过→commit。测试命令 `cargo test -p mdm-server`、`cargo test -p mdm-base-rust bridge`。
- 注释风格对齐仓库：中文、模块头 `//!`、ponytail 简化处标注 `// ponytail:`。
- 现有契约不得回归：`//`、`..`、`\`、`\0` → 404；`/a` 与 `/a/` 等价；path 存在但 verb 未注册 → 405；超时 408。
- 每任务结束全量 `cargo test --workspace` 绿后再 commit。

---

### Task 1: 依赖声明 + `normalize` / `decode_params` 纯函数

**Files:**
- Modify: `server/Cargo.toml`（dependencies 加三行）
- Modify: `server/src/routes.rs`（新增两个 pub fn + 测试）
- Modify: `docs/route-params-design.md` §6.1（修正 `/` 规则表述）

**Interfaces:**
- Produces: `pub fn normalize(path: &str) -> Option<String>`；`pub fn decode_params(pairs: impl Iterator<Item = (String, String)>) -> Option<HashMap<String, String>>`（Task 2 的 `lookup` 消费）。

- [ ] **Step 1: Cargo.toml 加依赖**

```toml
matchit = "0.8"
percent-encoding = "2.3"
form_urlencoded = "1.2"
```

- [ ] **Step 2: 写失败测试**（routes.rs `mod tests` 追加）

```rust
#[test]
fn normalize_guards_and_trims() {
    assert_eq!(normalize("/v1/api/user/account/"), Some("/v1/api/user/account".into()));
    assert_eq!(normalize("/v1/api/user/account"), Some("/v1/api/user/account".into()));
    assert_eq!(normalize("/"), Some("/".into()));
    assert_eq!(normalize("/v1/api//dbl"), None);          // 空段（routes.rs:111 契约）
    assert_eq!(normalize("/v1/api/../etc"), None);         // 穿越段
    assert_eq!(normalize("/v1/api/./x"), None);
    assert_eq!(normalize("/v1/api/a\\b"), None);           // 反斜杠
    assert_eq!(normalize("/v1/api/a\0b"), None);           // NUL
}

#[test]
fn decode_params_validates_smuggling() {
    use std::collections::HashMap;
    let ok = |v: &str| decode_params(vec![("id".into(), v.into())].into_iter());
    assert_eq!(ok("42").unwrap()["id"], "42");
    assert_eq!(ok("%41").unwrap()["id"], "A");             // 正常解码
    assert!(ok("%2e%2e").is_none());                       // 编码穿越
    assert!(ok(".").is_none());
    // 单段走私斜杠：raw 无 / 而解码后有 → 拒绝
    assert!(ok("a%2Fb").is_none());
    assert!(ok("a%5Cb").is_none());
    // catch-all 值：raw 含真实分隔符 → 放行
    let ca = decode_params(vec![("path".into(), "a/b%20c".into())].into_iter());
    assert_eq!(ca.unwrap()["path"], "a/b c");
}
```

- [ ] **Step 3: 跑测试确认编译失败**（`normalize`/`decode_params` 未定义）
Run: `cargo test -p mdm-server routes` → 编译错误

- [ ] **Step 4: 实现**（routes.rs，`use` 加 `percent_encoding::percent_decode_str`）

```rust
/// 查表前守卫+归一：`\`/`\0`/空段/`.`/`..` → None(404，对齐旧 resolve 契约 routes.rs:28-33)；
/// 尾斜杠归一（`/a/`→`/a`，根保持 `/`）。
pub fn normalize(path: &str) -> Option<String> {
    if path.contains('\\') || path.contains('\0') || !path.starts_with('/') {
        return None;
    }
    let t = path.trim_end_matches('/');
    if t.is_empty() {
        return Some("/".into());
    }
    if t[1..].split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return None;
    }
    Some(t.to_string())
}

/// 匹配后参数：percent-decode（`+` 保持字面，路径语义）+ 走私校验，None → 404。
/// 规则（设计稿 §6.1-4）：解码值为 `.`/`..`、含 `\`/`\0`，或 **raw 无 `/` 而解码后有 `/`**（单段参数走私 %2F）→ 拒绝；
/// catch-all 的 raw 值含真实分隔符，放行。
pub fn decode_params(pairs: impl Iterator<Item = (String, String)>) -> Option<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (k, raw) in pairs {
        let v = percent_decode_str(&raw).decode_utf8().ok()?;
        let smuggled = !raw.contains('/') && v.contains('/');
        if v == "." || v == ".." || v.contains('\\') || v.contains('\0') || smuggled {
            return None;
        }
        out.insert(k, v.into_owned());
    }
    Some(out)
}
```

- [ ] **Step 5: 跑测试过** `cargo test -p mdm-server routes`

- [ ] **Step 6: 同步修设计稿 §6.1 第 4 条**（现文"任一参数值含 `/`"会误杀 catch-all）：
  改为 `任一参数值解码后为 `.`/`..`、含 `\`/`\0`，或 **raw 无 `/` 而解码后有 `/`**（单段走私 `%2F`）→ 404；catch-all 的 raw 值含真实分隔符，放行。`

- [ ] **Step 7: Commit** `git add -A && git commit -m "feat(routes): normalize guard + param decode/validate (TDD)"`

---

### Task 2: `RouteTable` 核心（build + lookup，纯逻辑）

**Files:**
- Modify: `server/src/routes.rs`

**Interfaces:**
- Consumes: Task 1 的 `normalize`/`decode_params`；现有 `method_name`。
- Produces（Task 4/5 消费）:
  - `pub enum Entry { File(PathBuf), Conflict(String) }`
  - `pub enum Lookup { Hit { file: PathBuf, params: HashMap<String, String> }, Conflict(String), MethodNotAllowed, NotFound }`
  - `pub struct RouteRow { pub method: String, pub pattern: String, pub file: PathBuf }`
  - `impl RouteTable { pub fn build(base: &str, root: &Path, ts: bool, introspect: impl Fn(&Path) -> Result<Vec<(String, Option<String>)>, String>) -> (Self, Vec<String>); pub fn lookup(&self, path: &str, verb: &str) -> Lookup; pub fn is_replaced(&self, file: &Path, js_method: &str) -> bool; pub fn listing(&self) -> &[RouteRow] }`

- [ ] **Step 1: 写失败测试**（fixture 复用现有 `fn fixture`；内省用假闭包）

```rust
fn tbl(files: &[&str], decls: &[(&str, &str, &str)]) -> (RouteTable, Vec<String>) {
    // files: api 文件相对路径；decls: (文件, 方法, .route 值或 "")
    let root = fixture(files);
    let m: HashMap<String, Vec<(String, Option<String>)>> = decls.iter()
        .map(|(f, m, r)| (f.to_string(), vec![(m.to_string(),
            if r.is_empty() { None } else { Some(r.to_string()) })])).collect();
    let (t, fail) = RouteTable::build("/v1/api", &root, true, |p| {
        let key = p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
        Ok(m.get(&key).cloned().unwrap_or_default())
    });
    (t, fail)
}

#[test]
fn table_registers_relative_and_rooted() {
    let (t, f) = tbl(&["user/account/api.ts"], &[("user/account/api.ts", "get", "")]);
    assert!(f.is_empty());
    assert!(matches!(t.lookup("/v1/api/user/account", "GET"),
        Lookup::Hit { .. }));
    assert!(matches!(t.lookup("/v1/api/user/account/", "GET"), Lookup::Hit { .. })); // normalize 前置由调用方做；lookup 收已归一路径
    assert!(matches!(t.lookup("/v1/api/user/account", "POST"), Lookup::MethodNotAllowed));
    assert!(matches!(t.lookup("/v1/api/none", "GET"), Lookup::NotFound));
}

#[test]
fn table_param_extraction_and_route_suffix() {
    let (t, _) = tbl(&["user/account/api.ts"], &[("user/account/api.ts", "get", "{id}")]);
    match t.lookup("/v1/api/user/account/42", "GET") {
        Lookup::Hit { params, .. } => assert_eq!(params["id"], "42"),
        _ => panic!("expected hit"),
    }
    // 挂 .route 后目录镜像不再注册
    assert!(matches!(t.lookup("/v1/api/user/account", "GET"), Lookup::NotFound));
    assert!(t.is_replaced(&fixture_root_marker, "get")); // 实现时用返回的 file 断言，见 Step 3 备注
}

#[test]
fn table_rooted_route_ignores_dir() {
    let (t, _) = tbl(&["legacy/compat/api.ts"], &[("legacy/compat/api.ts", "get", "/v2/user/{id}")]);
    assert!(matches!(t.lookup("/v1/api/v2/user/42", "GET"), Lookup::Hit { .. }));
    assert!(matches!(t.lookup("/v1/api/legacy/compat", "GET"), Lookup::NotFound));
}

#[test]
fn table_duplicate_is_conflict_500() {
    let (t, f) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[("a/api.ts", "get", "/user/{id}"), ("b/api.ts", "get", "/user/{id}")],
    );
    assert!(f.iter().any(|s| s.contains("route conflict")));
    assert!(matches!(t.lookup("/v1/api/user/1", "GET"), Lookup::Conflict(_)));
    // 冲突 pattern 的其它 verb 仍 405 语义
    assert!(matches!(t.lookup("/v1/api/user/1", "POST"), Lookup::MethodNotAllowed));
}

#[test]
fn table_merges_verbs_across_files() {
    let (t, f) = tbl(
        &["a/api.ts", "b/api.ts"],
        &[("a/api.ts", "get", "/x"), ("b/api.ts", "post", "/x")],
    );
    assert!(f.is_empty());
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
fn table_catch_all_needs_one_segment() {
    let (t, _) = tbl(&["file/api.ts"], &[("file/api.ts", "get", "{*path}")]);
    assert!(matches!(t.lookup("/v1/api/file/a/b/c", "GET"), Lookup::Hit { .. }));
    assert!(matches!(t.lookup("/v1/api/file", "GET"), Lookup::NotFound));
}

#[test]
fn table_introspect_failure_skips_file() {
    let root = fixture(&["bad/api.ts", "good/api.ts"]);
    let (t, f) = RouteTable::build("/v1/api", &root, true, |p| {
        if p.ends_with("bad/api.ts") { Err("syntax error".into()) }
        else { Ok(vec![("get".into(), None)]) }
    });
    assert_eq!(f.len(), 1);
    assert!(matches!(t.lookup("/v1/api/good", "GET"), Lookup::Hit { .. }));
    assert!(matches!(t.lookup("/v1/api/bad", "GET"), Lookup::NotFound));
}

#[test]
fn table_unmapped_verb_405_when_path_exists() {
    let (t, _) = tbl(&["u/api.ts"], &[("u/api.ts", "get", "")]);
    assert!(matches!(t.lookup("/v1/api/u", "TRACE"), Lookup::MethodNotAllowed));
    assert!(matches!(t.lookup("/v1/api/none", "TRACE"), Lookup::NotFound));
}
```

- [ ] **Step 2: 跑测试确认编译失败**（`RouteTable` 未定义）
Run: `cargo test -p mdm-server routes`

- [ ] **Step 3: 实现**（routes.rs；测试备注：`is_replaced` 断言用 `listing()` 里返回的 file 路径，不依赖 marker）

```rust
use matchit::Router;
use std::collections::HashSet;

/// 单 pattern 下某方法的归宿：文件 / 冲突（请求期 500）。
#[derive(Clone)]
pub enum Entry { File(PathBuf), Conflict(String) }

pub enum Lookup {
    Hit { file: PathBuf, params: HashMap<String, String> },
    Conflict(String),
    MethodNotAllowed,
    NotFound,
}

pub struct RouteRow { pub method: String, pub pattern: String, pub file: PathBuf }

pub struct RouteTable {
    matcher: Router<HashMap<String, Entry>>,
    /// 挂了 .route 的 (file, js 方法名)：dev 兜底不得复活其目录镜像 URL。
    replaced: HashSet<(PathBuf, String)>,
    rows: Vec<RouteRow>,
}

const METHODS: [&str; 7] = ["get", "post", "put", "del", "patch", "head", "options"];

impl RouteTable {
    /// 建表：内省闭包按文件返回 Vec<(方法, .route 或 None)>；返回 (表, 失败/冲突清单)。
    /// 纯逻辑（DIP：不依赖 JS 运行时），CLI/测试注入真实或假内省。
    pub fn build(
        base: &str,
        root: &Path,
        ts: bool,
        introspect: impl Fn(&Path) -> Result<Vec<(String, Option<String>)>, String>,
    ) -> (Self, Vec<String>) {
        let b = base.trim_matches('/');
        let mut failures = Vec::new();
        let mut t = RouteTable { matcher: Router::new(), replaced: HashSet::new(), rows: Vec::new() };
        for file in api_files(root, ts) {
            let decls = match introspect(&file) {
                Ok(d) => d,
                Err(e) => { failures.push(format!("{}: {e}", file.display())); continue; }
            };
            let rel = file.parent().and_then(|p| p.strip_prefix(root)).unwrap_or(Path::new(""));
            let rel = rel.to_string_lossy().replace('\\', "/");
            let dir_base = if rel.is_empty() { format!("/{b}") } else { format!("/{b}/{rel}") };
            for (method, route) in decls {
                if !METHODS.contains(&method.as_str()) { continue; }
                let route = route.filter(|r| !r.is_empty());   // "" 视同未挂
                let pattern = match &route {
                    None => dir_base.clone(),
                    Some(r) if r.starts_with('/') => format!("/{b}{r}"),
                    Some(r) => format!("{dir_base}/{r}"),
                };
                if route.is_some() { t.replaced.insert((file.clone(), method.clone())); }
                match t.matcher.at_mut(&pattern) {
                    Ok(m) => match m.value.get_mut(&method) {
                        Some(Entry::File(a)) => {
                            let msg = format!("route conflict: {method} {pattern} declared in {a} and {}", file.display());
                            *m.value.get_mut(&method).unwrap() = Entry::Conflict(msg.clone());
                            failures.push(msg);
                        }
                        _ => {
                            m.value.insert(method.clone(), Entry::File(file.clone()));
                            t.rows.push(RouteRow { method, pattern, file: file.clone() });
                        }
                    },
                    Err(_) => {
                        let mut map = HashMap::new();
                        map.insert(method.clone(), Entry::File(file.clone()));
                        match t.matcher.insert(pattern.clone(), map) {
                            Ok(()) => t.rows.push(RouteRow { method, pattern, file: file.clone() }),
                            // 非法语法 / 结构性冲突（同位置异名参数）：日志丢弃后来者
                            Err(e) => failures.push(format!("invalid route {method} {pattern} from {}: {e}", file.display())),
                        }
                    }
                }
            }
        }
        (t, failures)
    }

    /// 查表：path 须先经 `normalize`。未映射动词按"路径存在 → 405"契约处理。
    pub fn lookup(&self, path: &str, verb: &str) -> Lookup {
        let hit = |m: matchit::Match<&HashMap<String, Entry>>| m; // 供两分支共用
        let name = method_name(verb);
        let m = match self.matcher.at(path) {
            Ok(m) => m,
            Err(_) => return Lookup::NotFound,
        };
        let Some(name) = name else { return Lookup::MethodNotAllowed };
        match m.value.get(name) {
            Some(Entry::File(f)) => {
                let pairs = m.params.iter().map(|(k, v)| (k.to_string(), v.to_string()));
                match decode_params(pairs) {
                    Some(params) => Lookup::Hit { file: f.clone(), params },
                    None => Lookup::NotFound,   // 走私参数 → 404（§6.1-4）
                }
            }
            Some(Entry::Conflict(msg)) => Lookup::Conflict(msg.clone()),
            None => Lookup::MethodNotAllowed,
        }
    }

    pub fn is_replaced(&self, file: &Path, js_method: &str) -> bool {
        self.replaced.contains(&(file.to_path_buf(), js_method.to_string()))
    }

    pub fn listing(&self) -> &[RouteRow] { &self.rows }
}

/// root 下全部 api 文件（排序，冲突裁决确定）。
fn api_files(root: &Path, ts: bool) -> Vec<PathBuf> {
    let ext = if ts { "api.ts" } else { "api.js" };
    let mut out = Vec::new();
    walk_files(root, ext, &mut out);
    out.sort();
    out
}

fn walk_files(dir: &Path, ext: &str, acc: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() { walk_files(&p, ext, acc); }
        else if e.file_name().to_string_lossy() == ext { acc.push(p); }
    }
}
```

（删除：未用闭包 `hit`；`lookup` 里未映射动词分支在 `matcher.at` Err 时先返回 NotFound —— 注意未映射动词 + 路径不存在应为 NotFound，实现时按上面顺序即正确：先 at() 再判 name。）

- [ ] **Step 4: 跑测试过** `cargo test -p mdm-server routes`（含 Task 1 测试）
- [ ] **Step 5: Commit** `git commit -am "feat(routes): RouteTable build/lookup with conflict-in-value (TDD)"`

---

### Task 3: `Bridge::introspect_module`（内省 driver）

**Files:**
- Modify: `src/bridge/mod.rs`（`run_module` 后加方法；`pub const INTROSPECT_TIMEOUT`）

**Interfaces:**
- Consumes: `run_module` 的 driver/KillSwitch/checkin 结构（mod.rs:340-400）。
- Produces: `pub const INTROSPECT_TIMEOUT: std::time::Duration`（=2s）；`impl Bridge { pub async fn introspect_module(&self, api_path: &Path) -> Result<serde_json::Value, RunError> }`——返回信封 `data`：`{"get": "{id}" | null, ...}`（仅含函数导出的方法；null = 导出但未挂 `.route`）。Task 4 的 `decls_from_value` 消费。

- [ ] **Step 1: 写失败测试**（mod.rs `mod tests`，沿用现有 TRANSPILE_TEST_LOCK 互斥模式，参考既有 run_module 测试 :729-805 的 fixture 写法）

```rust
#[tokio::test]
async fn introspect_reads_route_decls() {
    let api = api_fixture(&[
        ("src/a/api.ts", r#"
            function get() { json.ok({}); }
            get.route = "{id}";
            function del() { json.ok({}); }
            export default { get, del };
        "#),
    ]);
    let b = test_bridge_with_loader(&api.parent);   // 复用现有测试 bridge 构造（LoaderShared project_root=fixture 根）
    let v = b.introspect_module(&api.join("a/api.ts")).await.unwrap();
    assert_eq!(v["get"], json!("{id}"));
    assert_eq!(v["del"], json!(null));
    assert!(v.get("post").is_none());               // 未导出 → 缺席
}

#[tokio::test]
async fn introspect_broken_module_errs() {
    let api = api_fixture(&[("src/bad/api.ts", "function {{{{\nexport default {};")]);
    let b = test_bridge_with_loader(&api.parent);
    assert!(b.introspect_module(&api.join("bad/api.ts")).await.is_err());
}

#[tokio::test]
async fn introspect_top_level_loop_times_out() {
    let api = api_fixture(&[("src/loop/api.ts", "while (true) {}\nexport default { get() {} };")]);
    let b = test_bridge_with_loader(&api.parent);
    let err = b.introspect_module(&api.join("loop/api.ts")).await.unwrap_err();
    assert!(matches!(err, RunError::Timeout));
}
```
（`api_fixture`/`test_bridge_with_loader` 按现有 mod.rs 测试 helper 实情命名调整；若无直接可复用的，参照 :754 的 run_module 测试构造方式写最小 fixture。）

- [ ] **Step 2: 跑测试确认编译失败** Run: `cargo test -p mdm-base-rust bridge::tests::introspect`

- [ ] **Step 3: 实现**（结构完全镜像 `run_module`，仅 driver 体与返回不同）

```rust
/// 内省超时：坏模块顶层死循环不挂死启动。
/// ponytail: 常量而非配置；真有 >2s 的合法顶层模块再加 server.introspect_timeout。
pub const INTROSPECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// 启动期内省：import api 模块、读 default[method].route，经 json.ok 信封回传 data。
/// 复用 run_module 的 driver/KillSwitch/checkin 管道（mod.rs:340-400 注释同样适用）。
pub async fn introspect_module(&self, api_path: &std::path::Path) -> Result<serde_json::Value, RunError> {
    let spec = module_loader::versioned_specifier(api_path)
        .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e))))?;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let driver_spec = deno_core::ModuleSpecifier::parse(&format!("file:///oj/introspect/{n}.js"))
        .map_err(|e| RunError::Core(CoreError::from(std::io::Error::other(e.to_string()))))?;
    let code = format!(
        "const m = await import(\"{spec}\");\n\
         const out = {{}};\n\
         for (const k of [\"get\",\"post\",\"put\",\"del\",\"patch\",\"head\",\"options\"]) {{\n\
           const fn = m.default && m.default[k];\n\
           if (typeof fn === \"function\") out[k] = fn.route === undefined ? null : String(fn.route);\n\
         }}\n\
         json.ok(out);\n"
    );
    // —— 与 run_module 相同的 arm/disarm + side module 求值 + 事件循环（照抄 :370-399 结构），
    // timeout 用 INTROSPECT_TIMEOUT，ReqState reset(RequestInfo::default())。
    // 成功路径：从 Capture 解信封取 data：
    //   let v: serde_json::Value = serde_json::from_slice(&capture.body).unwrap_or_default();
    //   Ok(v["data"].clone())
}
```

- [ ] **Step 4: 跑测试过** `cargo test -p mdm-base-rust bridge`
- [ ] **Step 5: Commit** `git commit -am "feat(bridge): introspect_module reuses run_module pipeline (TDD)"`

---

### Task 4: server `handle` 接线 + `parse_query` 解码 + `bootstrap.js`（e2e）

**Files:**
- Modify: `server/src/lib.rs`（AppState/app/serve/handle/parse_query + 测试 helper）
- Modify: `server/src/routes.rs`（加 `decls_from_value` + `bridge_introspector`）
- Modify: `src/bridge/bootstrap.js:43-48`（`http.param` 合并 path→query）

**Interfaces:**
- Consumes: Task 2 `RouteTable`/`Lookup`/`is_replaced`；Task 3 `introspect_module`/`INTROSPECT_TIMEOUT`；旧 `Routes::resolve`（兜底）。
- Produces（Task 5 消费）: `pub fn app(base: &str, dir, ts, table: RouteTable, actor, timeout)`；`pub async fn serve(addr, base, dir, ts, table: RouteTable, actor, timeout)`；`serve_with_listener` 同步加参；`pub fn bridge_introspector(make: impl Fn() -> Bridge + Send + Sync + 'static) -> impl Fn(&Path) -> Result<Vec<(String, Option<String>)>, String>`。

- [ ] **Step 1: 写失败 e2e 测试**（lib.rs `mod tests`；先改 `spawn_server`：建表 → 传参）

```rust
pub(crate) async fn spawn_server(base: &str, dir: PathBuf, ts: bool, timeout: Option<Duration>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = build_table(&dir, ts, base);          // Step 3 实现：bridge_introspector + RouteTable::build
    let base_s = base.to_string();
    tokio::spawn(async move {
        serve_with_listener(listener, &base_s, dir.clone(), ts, table, actor(dir, ts), timeout).await.unwrap();
    });
    addr
}
```

```rust
#[tokio::test]
async fn serves_path_param_route() {
    let t = routes(&[("user/account/api.ts",
        "function detail(){ json.ok({ id: Number(http.param(\"id\", 0)) }); }\n\
         detail.route = \"{id}\";\nexport default { get: detail };")]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let resp = raw_http(addr, "GET /v1/api/user/account/42 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains("\"id\":42"), "{resp}");
    // 尾斜杠等价
    let resp2 = raw_http(addr, "GET /v1/api/user/account/42/ HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(resp2.starts_with("HTTP/1.1 200"), "{resp2}");
    // 挂 .route 后目录镜像 404（替换语义）
    let resp3 = raw_http(addr, "GET /v1/api/user/account HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(resp3.starts_with("HTTP/1.1 404"), "{resp3}");
}

#[tokio::test]
async fn path_param_overrides_query_and_decodes() {
    let t = routes(&[("u/api.ts",
        "function get(){ json.ok({ id: http.param(\"id\", 0) }); }\n\
         get.route = \"{id}\";\nexport default { get };")]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let resp = raw_http(addr, "GET /v1/api/u/42%41?id=99 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(resp.contains("\"id\":\"42A\""), "{resp}");      // 解码 + 路径优先
}

#[tokio::test]
async fn catch_all_and_guards() {
    let t = routes(&[("file/api.ts",
        "function get(){ json.ok({ p: http.param(\"path\", \"\") }); }\n\
         get.route = \"{*path}\";\nexport default { get };")]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let ok = raw_http(addr, "GET /v1/api/file/a/b/c HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("a/b/c"), "{ok}");
    for path in ["/v1/api/file", "/v1/api/file/", "/v1/api//file/a", "/v1/api/file/%2e%2e"] {
        let r = raw_http(addr, &format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 404"), "{path}: {r}");
    }
}

#[tokio::test]
async fn verb_missing_is_405_and_trace_405() {
    let t = routes(&[("u/f/api.ts", "function get(){ json.ok({}); }\nexport default { get };")]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    for verb in ["DELETE", "TRACE"] {
        let r = raw_http(addr, &format!("{verb} /v1/api/u/f HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 405"), "{verb}: {r}");
    }
}

#[tokio::test]
async fn conflict_route_returns_500() {
    let t = routes(&[
        ("a/api.ts", "function get(){ json.ok({a:1}); }\nget.route = \"/user/{id}\";\nexport default { get };"),
        ("b/api.ts", "function get(){ json.ok({b:1}); }\nget.route = \"/user/{id}\";\nexport default { get };"),
    ]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let r = raw_http(addr, "GET /v1/api/user/9 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 500") && r.contains("route conflict"), "{r}");
}

#[tokio::test]
async fn query_decodes_form_urlencoded() {
    let t = routes(&[("q/api.ts",
        "export default { get() { json.ok({ q: http.param(\"q\", \"\") }); } };")]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let r = raw_http(addr, "GET /v1/api/q?q=a+b%21 HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(r.contains("\"q\":\"a b!\""), "{r}");
}

#[tokio::test]
async fn dev_fallback_serves_new_file_without_rebuild() {
    let t = routes(&[]);                     // 建表时无文件
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let p = t.0.join("late/api.ts");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "export default { get() { json.ok({ late: true }); } };").unwrap();
    let r = raw_http(addr, "GET /v1/api/late HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
}

#[tokio::test]
async fn dev_fallback_does_not_resurrect_replaced_route() {
    // 建表时文件在、get 挂了 .route → 目录镜像被替换，兜底不得复活
    let t = routes(&[("r/api.ts",
        "function get(){ json.ok({}); }\nget.route = \"{id}\";\nexport default { get };")]);
    let addr = spawn_server("/v1/api", t.0.clone(), true, None).await;
    let r = raw_http(addr, "GET /v1/api/r HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 404"), "{r}");
}
```

- [ ] **Step 2: 跑测试确认失败**（`serve_with_listener` 签名不匹配 → 编译错；改 helper 后逐条验证行为断言失败）
Run: `cargo test -p mdm-server`

- [ ] **Step 3: 实现**

`routes.rs` 追加（server 已依赖 mdm-base-rust 与 serde_json）：

```rust
use mdm_base_rust::bridge::Bridge;

/// 内省结果 Value（Task 3 约定：仅函数导出的方法，null=未挂）→ decls。
pub fn decls_from_value(v: &serde_json::Value) -> Vec<(String, Option<String>)> {
    v.as_object().map(|o| o.iter()
        .filter_map(|(k, v)| Some((k.clone(), match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Null => None,
            _ => return None,
        })))
        .collect()).unwrap_or_default()
}

/// 真实内省闭包：每文件一线程 + 独立 current_thread runtime（Bridge !Send 不跨线程；
/// 嵌套 runtime 会 panic，故换线程）。CLI 与测试共用。
/// ponytail: 每文件起线程；文件数极大时改为单线程批处理。
pub fn bridge_introspector(
    make: impl Fn() -> Bridge + Send + Sync + 'static,
) -> impl Fn(&Path) -> Result<Vec<(String, Option<String>)>, String> {
    move |f: &Path| {
        let f = f.to_path_buf();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("introspect rt");
            let b = make();
            rt.block_on(async { b.introspect_module(&f).await })
                .map(|v| decls_from_value(&v))
                .map_err(|e| e.to_string())
        })
        .join()
        .unwrap_or_else(|_| Err("introspect thread panicked".into()))
    }
}
```
（server/Cargo.toml tokio 无 `rt` feature? 已有 `rt`。若缺按编译提示补。）

`lib.rs`：

```rust
pub struct AppState {
    table: RouteTable,
    fallback: Option<Routes>,   // dev（ts=true）文件系统兜底；release None
    actor: JsActor,
    timeout: Option<std::time::Duration>,
}

pub fn app(base: &str, dir: impl Into<PathBuf>, ts: bool, table: RouteTable, actor: JsActor,
           timeout: Option<std::time::Duration>) -> Router {
    let dir = dir.into();
    Router::new().fallback(any(handle)).with_state(AppState {
        table,
        fallback: ts.then(|| Routes::new(base, dir, ts)),
        actor, timeout,
    })
}
// serve / serve_with_listener 同步加 `table: RouteTable` 参数透传 app。

async fn handle(State(st): State<AppState>, method: Method, uri: Uri, headers: HeaderMap,
                body: axum::body::Bytes) -> Response {
    let verb = method.as_str();
    let run = |file: PathBuf, params: HashMap<String, String>| async move { /* 原 85-100 行尾段：RequestInfo+run_module+capture */ };
    if let Some(path) = crate::routes::normalize(uri.path()) {
        match st.table.lookup(&path, verb) {
            Lookup::Hit { file, params } => return run(file, params).await,
            Lookup::Conflict(msg) => return fail_response(500, &msg),
            Lookup::MethodNotAllowed => return fail_response(405, &format!("method {verb} not allowed")),
            Lookup::NotFound => {}
        }
    }
    // dev 兜底：目录镜像（挂 .route 的方法已被替换，不得复活）
    if let Some(fb) = &st.fallback {
        if let Some(file) = fb.resolve(uri.path()) {
            match crate::routes::method_name(verb) {
                Some(m) if !st.table.is_replaced(&file, m) => return run(file, HashMap::new()).await,
                Some(_) => {}                       // replaced → 404
                None => return fail_response(405, &format!("method {verb} not mapped")),
            }
        }
    }
    fail_response(404, "no route matched")
}

/// `?a=1&b=2` → map（form_urlencoded 解码：`%XX` + `+`→空格）。
fn parse_query(q: Option<&str>) -> HashMap<String, String> {
    q.map(|s| form_urlencoded::parse(s.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
     .unwrap_or_default()
}
```

`bootstrap.js:43-48` 替换为：

```js
if (p === "param") {
  return (name, def) => {
    const info = httpInfo();
    const v = info.params[name] !== undefined ? info.params[name] : info.query[name];
    return v === undefined ? def : v;
  };
}
```

测试 helper `build_table`（lib.rs tests）：

```rust
pub(crate) fn build_table(dir: &Path, ts: bool, base: &str) -> RouteTable {
    let root = dir.canonicalize().unwrap();
    let make = move || Bridge::with_dbs_and_loader(
        HashMap::new(), Arc::new(InMemoryKV::new()), SchemaRegistry::new(), false,
        Some(Arc::new(LoaderShared { project_root: root.clone(), ts })));
    let (t, failures) = RouteTable::build(base, &dir.canonicalize().unwrap(), ts, bridge_introspector(make));
    assert!(failures.is_empty(), "{failures:?}");
    t
}
```
（`actor()` helper 的闭包构造同款 hoist 复用；`routes()`/`actor()` 现有签名保持。）

- [ ] **Step 4: 全量跑** `cargo test -p mdm-server`（含既有 5 个 e2e：镜像信封/404+405/408/500 均须仍绿——500 语法错误用例经 dev 兜底路径保平价）
- [ ] **Step 5: Commit** `git commit -am "feat(server): table-driven handle with dev fs fallback + query decode + param merge (TDD)"`

---

### Task 5: CLI 接线与启动打印

**Files:**
- Modify: `../../../oj/src/server_cmd.rs`（hoist make 闭包；内省建表；打印；serve 传参）

**Interfaces:**
- Consumes: Task 4 的 `serve(addr, base, dir, ts, table, actor, timeout)`、`RouteTable::listing`、`bridge_introspector`；Task 3 `INTROSPECT_TIMEOUT`（间接）。

- [ ] **Step 1: 实现**（CLI 无测试基建，逻辑全部在已测组件里，本任务只做接线——接线正确性由 Step 2 手工验证兜住）

```rust
// server_cmd.rs run() 内，替换 :62-68 打印段，actor 池构造前：
let (table, failures) = {
    let (dbs, kv, loader) = (dbs.clone(), kv.clone(), loader.clone());
    let make = move || Bridge::with_dbs_and_loader(dbs.clone(), kv.clone(), SchemaRegistry::new(), false, Some(loader.clone()));
    let intro = mdm_server::routes::bridge_introspector(make);
    RouteTable::build(&base, &dir, ts, intro)
};
for f in &failures { eprintln!("error: {f}"); }
if !failures.is_empty() { eprintln!("warn: {} route declaration(s) skipped (see errors above)", failures.len()); }
for r in table.listing() {
    println!("  {:8} {}  <- {}", r.method.to_uppercase(), r.pattern, r.file.display());
}
// actor 池用同一 make（hoist 到 table 构造前的变量，move 进 pool 闭包）。
// serve 调用加 table 参数。
```
（`Bridge`/`SchemaRegistry` 等已在 server_cmd.rs use；确认 `mdm_server::routes::{RouteTable, bridge_introspector}` 导入。）

- [ ] **Step 2: 手工验证**（sample 项目跑通）

```bash
cargo build -p oj
cd sample && ../target/debug/oj server -c config.yaml -b /v1/api -d src --dev &
sleep 1
curl -s localhost:8080/v1/api/user/account        # 现有镜像路由 200
curl -s -i localhost:8080/v1/api/user/account/    # 尾斜杠 200
kill %1
```
（端口/启动命令按 sample/README.md 或 config.yaml 实情调整；预期启动日志含方法×pattern 路由表。）

- [ ] **Step 3: 全量测试 + Commit** `cargo test --workspace && git commit -am "feat(cli): build route table at startup, print methods+patterns"`

---

### Task 6: sample 示例 + `global.d.ts` + 用户手册

**Files:**
- Create: `sample/global.d.ts`
- Create: `sample/src/file/api.ts`、`sample/src/user/item/api.ts`
- Modify: `docs/user-manual.md`（§9 表、§10 表、§11 走读）

- [ ] **Step 1: sample 文件**

`sample/global.d.ts`:
```ts
declare global { interface Function { route?: string } }
export {};
```

`sample/src/user/item/api.ts`（单参数）:
```ts
function detail() {
  json.ok({ id: Number(http.param("id", 0)) });
}
detail.route = "{id}";
export default { get: detail };
```

`sample/src/file/api.ts`（catch-all）:
```ts
function get() {
  json.ok({ segs: http.param("path", "").split("/") });
}
get.route = "{*path}";
export default { get };
```

- [ ] **Step 2: 手工验证 demo**（同 Task 5 Step 2 启动后）
```bash
curl -s localhost:8080/v1/api/user/item/42        # {"id":42}
curl -s localhost:8080/v1/api/file/a/b/c          # {"segs":["a","b","c"]}
curl -s -i localhost:8080/v1/api/file             # 404（catch-all ≥1 段）
```

- [ ] **Step 3: user-manual.md**
- §9 表 `http.param(name, default)` 行改为：`取参数：**路径参数优先**，query 兜底（同名时路径胜）`；表中新增行 `http.params | 路径参数对象（\`{id:"42"}\`，已解码）`。
- §9 表后补安全告诫一行：`路径参数已解码（可含 /、.. 字面）——仅用于参数化查询与类型转换，勿拼接文件路径/URL。` 与 TS 提示（global.d.ts）。
- §10 表：404 行文案改 `no route matched`；405 行拆两行（未注册 verb / 方法未导出）；新增 500 行 `路由冲突 | 500 | {"code":500,"msg":"route conflict: GET /v1/api/user/{id} declared in a/api.ts and b/api.ts"}`。
- §10 后补 query 解码迁移说明一句：`query 现按 form-urlencoded 解码（+→空格、%XX 解码），旧版不解码。`
- §11 补两行新样例（item/{id}、file/{*path}）。

- [ ] **Step 4: 全量测试 + 手册一致性自查（grep "no api file for route" 应只剩历史文档）+ Commit**
`cargo test --workspace && git commit -am "docs+sample: .route demos, global.d.ts, user-manual route params"`

---

## Self-Review 记录

- 规格覆盖：设计稿 §2 内省策略→Task 3+5；§3 语法→Task 1/2 测试；§4 建表→Task 2；§5 冲突→Task 2；§6 normalize/兜底/契约→Task 1/4；§7 参数/解码→Task 1/4；§8 清单→Tasks 1-6 逐行对应（`Cargo.toml`/routes/lib/mod/bootstrap/server_cmd/user-manual/sample 全覆盖）；§9 Demo→Task 6。无缺口。
- 类型一致：`introspect` 闭包签名、`Lookup`/`Entry`/`RouteRow`、`serve*` 参数序（table 在 actor 前）全计划统一。
- 已知边角：`{id}` 语法零翻译（无 `{{` 特判——matchit 原生处理）；root 级 `api.ts` 的 dir_base 无尾斜杠特判在 Task 2 `rel.is_empty()` 分支。
