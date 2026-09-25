# `_name_` 目录段 → `{name}` 动态参数 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 文件系统目录段 `_name_` 映射为 URL 动态参数 `{name}`，覆盖 dev 建表、dev 兜底、oj build 三条链路，并同步 CHANGELOG 与 devkit 四件。

**Architecture:** 单 helper（`fs_seg_to_pattern`）+ 三个拼接点（`RouteTable::build`、`rel_pattern`、`Routes::resolve` 回溯下降）+ 构建期 pattern 试插校验。matchit / decode_params / 冲突裁决全部复用。设计文档：`docs/superpowers/specs/2026-09-25-underscore-route-params-design.md`。

**Tech Stack:** Rust（matchit 0.8、axum）、现有测试设施（`cargo test --release`）。

## Global Constraints

- **禁止 debug 构建/测试**：一律 `cargo build --release` / `cargo test --release` / `cargo clippy --release --all-targets -- -D warnings`。
- `cargo fmt` 门禁：每个任务提交前跑 `cargo fmt`。
- 提交信息末尾加属性行：`unix@vip.qq.com ai`。
- `bootstrap.js` 7-bit ASCII 红线：本计划不触碰 JS 运行时文件。
- 测试用 `tokio::test(flavor = "current_thread")`（如需要异步）。
- 版本即 v0.1.27（`oj/Cargo.toml` 从 0.1.26 递增）；**不打标签**，CHANGELOG 注明「未打标签」。
- `.route` 值内 `_name_` **不转换**（保持 matchit 语法），只做告警——勿在 rel_pattern 的 route 分支里调用转换。

---

### Task 1: `fs_seg_to_pattern` helper（转换谓词）

**Files:**
- Modify: `server/src/routes.rs`（`normalize` 函数之后插入）
- Test: `server/src/routes.rs` 的 `mod tests`

**Interfaces:**
- Produces:
  - `pub fn looks_like_underscore_param(seg: &str) -> bool` —— 宽松判定（仅用于告警）：`len>2 && starts_with('_') && ends_with('_')`。
  - `pub fn fs_seg_to_pattern(seg: &str) -> String` —— 严格转换：满足宽松判定，且 inner（`seg[1..len-1]`）不以 `_` 开头/结尾、不含 `{`/`}` → `"{inner}"`；否则原样返回。`oj` crate 经 `server::routes` 复用（已依赖）。

- [ ] **Step 1: 写失败测试**

在 `server/src/routes.rs` 的 `mod tests` 内追加：

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --release -p server fs_seg_conversion_rules
```
预期：编译失败（函数不存在）。

- [ ] **Step 3: 实现**

在 `normalize` 之后插入：

```rust
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
```

- [ ] **Step 4: 跑测试确认通过 + fmt + clippy**

```bash
cargo test --release -p server fs_seg_conversion_rules
cargo fmt && cargo clippy --release -p server --all-targets -- -D warnings
```

- [ ] **Step 5: 提交**

```bash
git add server/src/routes.rs
git commit -m "feat(routes): fs_seg_to_pattern——_name_ 整段目录映射 {name} 动态参数（宽松/严格双谓词）

unix@vip.qq.com ai"
```

---

### Task 2: `RouteTable::build` 转换 + `.route` `_name_` 告警

**Files:**
- Modify: `server/src/routes.rs`（`RouteTable::build` 的 dir_base 拼接 ~L194-204；`mod tests`）
- Modify: `oj/src/app.rs`（failures 打印循环 ~L876-884）

**Interfaces:**
- Consumes: Task 1 的 `fs_seg_to_pattern` / `looks_like_underscore_param`。
- Produces: 约定——`RouteTable::build` 的 `failures` 里以 `warning: ` 前缀的条目是**告警**（消费方 oj/src/app.rs 按前缀分流打印、不计入 skipped 数）；无前缀的仍是错误。后续 Task 不得破坏此前缀约定。

- [ ] **Step 1: 写失败测试**

在 `mod tests` 追加（复用现有 `tbl` helper 与 `kind` 函数）：

```rust
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
    assert!(matches!(t.lookup("/v1/api/u/1", "GET"), Lookup::Conflict(_)));
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
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --release -p server table_
```
预期：`table_underscore_dir_becomes_param` 失败（`_id_` 未转换，lookup 404）。

- [ ] **Step 3: 实现 build 转换**

`RouteTable::build` 中把：

```rust
            let dir_base = if rel.is_empty() {
                format!("/{b}")
            } else {
                format!("/{b}/{rel}")
            };
```

改为：

```rust
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
```

- [ ] **Step 4: 实现 `.route` 告警**

在 build 的 `let route = route.filter(|r| !r.is_empty());` 之后、`let pattern = ...` 之前插入：

```rust
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
```

- [ ] **Step 5: 改 app.rs 打印循环**

`oj/src/app.rs` L876-884 改为：

```rust
        for f in &failures {
            if let Some(w) = f.strip_prefix("warning: ") {
                eprintln!("warn: route: {w}");
            } else {
                eprintln!("error: route: {f}");
            }
        }
        let n_err = failures
            .iter()
            .filter(|f| !f.starts_with("warning: "))
            .count();
        if n_err > 0 {
            eprintln!("warn: {n_err} route declaration(s) skipped (see errors above)");
        }
```

- [ ] **Step 6: 跑测试确认通过 + fmt + clippy**

```bash
cargo test --release -p server table_
cargo test --release -p oj app   # 兜住 app.rs 改动（如相关用例存在）
cargo fmt && cargo clippy --release --all-targets -- -D warnings
```

- [ ] **Step 7: 提交**

```bash
git add server/src/routes.rs oj/src/app.rs
git commit -m "feat(routes): 建表期 _name_ 目录段转 {name} 参数 + .route _name_ 形态告警

unix@vip.qq.com ai"
```

---

### Task 3: `Routes::resolve` 回溯下降 + 参数提取

**Files:**
- Modify: `server/src/routes.rs`（`Routes::resolve` ~L32-51；`mod tests`）
- Modify: `server/src/lib.rs`（dev 兜底调用点 ~L421-441）

**Interfaces:**
- Consumes: Task 1 的 `fs_seg_to_pattern`；既有 `decode_params`。
- Produces: `Routes::resolve(&self, http_path: &str) -> Option<(PathBuf, HashMap<String, String>)>` —— **签名变更**，`server/src/lib.rs` 唯一调用点必须同步。返回的 params 已过 `decode_params`（走私/解码校验不过 → 整体 None）。

- [ ] **Step 1: 更新既有 resolve 测试 + 写新失败测试**

`mirrors_directory_tree_any_depth` / `missing_or_traversal_is_none` / `release_mode_maps_api_js` 三个既有测试的断言从 `Some(path)` 改为 `Some((path, _))`（用 `map(|(f, _)| f)` 或直接匹配元组），如：

```rust
        assert_eq!(
            r.resolve("/v1/api/user/account/").map(|(f, _)| f),
            Some(root.join("user/account/api.ts"))
        );
```

（`missing_or_traversal_is_none` 全是 None 断言，不用改。）

新增测试：

```rust
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
    // 字面 `_id_` URL 被参数吃掉（b="_id_"），不再是静态路由
    let (f, p) = r.resolve("/v1/api/a/_id_").unwrap();
    assert_eq!(f, root.join("a/_id_/api.ts"));
    assert_eq!(p["id"], "_id_");
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
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --release -p server resolve_
```
预期：新用例编译失败（`resolve` 返回类型变了，既有测试也要改）或断言失败（params 恒空 / 下降未实现 → None）。

- [ ] **Step 3: 实现 resolve 重写**

把 `Routes::resolve` 整体替换为：

```rust
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
        let file = self.root.join(segs[..].iter().collect::<PathBuf>()).join(api);
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
        let Some((name, sub)) = underscore_child(dir) else {
            return None;
        };
        raw.push((name, segs[0].to_string()));
        let out = self.descend(&sub, &segs[1..], api, raw);
        if out.is_none() {
            raw.truncate(mark);
        }
        out
    }
```

并在 `impl Routes` 之外（模块级）追加：

```rust
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
```

注意：顶部 `use` 需补 `std::collections::HashMap`（若尚未引入）。

- [ ] **Step 4: 改 lib.rs 调用点**

`server/src/lib.rs` L421-441 的兜底块改为：

```rust
    if let Some((file, params)) = st.fallback.as_ref().and_then(|fb| fb.resolve(uri.path())) {
        // 表内路径经过 canonicalize（macOS /var ↔ /private/var），对齐后再比对
        let file = file.canonicalize().unwrap_or(file);
        match crate::routes::method_name(verb) {
            Some(m) if !st.table.is_replaced(&file, m) => {
                return run_route(
                    &st,
                    &headers,
                    body,
                    verb,
                    parse_query(uri.query()),
                    path_no_base.as_deref(),
                    file,
                    params,
                )
                .await;
            }
            Some(_) => {} // replaced → 404
            None => return fail_response(405, &format!("method {verb} not mapped")),
        }
    }
```

（`run_route` 的 `params` 参数类型本就是 `HashMap<String, String>`，签名不用动。）

- [ ] **Step 5: 跑测试确认通过 + fmt + clippy**

```bash
cargo test --release -p server resolve_
cargo test --release -p server
cargo fmt && cargo clippy --release --all-targets -- -D warnings
```

- [ ] **Step 6: 提交**

```bash
git add server/src/routes.rs server/src/lib.rs
git commit -m "feat(routes): dev 兜底 _name_ 目录回溯下降 + 参数提取（decode_params 同防线）

unix@vip.qq.com ai"
```

---

### Task 4: `oj build` — `rel_pattern` 转换 + 构建期 pattern 校验 + 告警

**Files:**
- Modify: `server/src/routes.rs`（新增 `check_patterns` + 单测）
- Modify: `oj/src/build_cmd.rs`（`rel_pattern` ~L662；`build_one` 的 routes.js 生成段 ~L389-410；tests）

**Interfaces:**
- Consumes: Task 1 的 `fs_seg_to_pattern` / `looks_like_underscore_param`（经 `server::routes`）。
- Produces:
  - `pub fn check_patterns(patterns: &[String]) -> Result<(), String>`（routes.rs）——逐条插入临时 matchit Router（补首斜杠），非法/同位异名 → Err。
  - `rel_pattern(module, rel_dir, route)` 新语义：module 段与 rel_dir 逐段过 `fs_seg_to_pattern`；route 值**不转换**。

- [ ] **Step 1: 写失败测试**

`server/src/routes.rs` tests 追加：

```rust
#[test]
fn check_patterns_rejects_invalid_and_conflicting() {
    // 合法：重复 pattern（多方法）可合并
    assert!(check_patterns(&["u/{id}".into(), "u/{id}".into()]).is_ok());
    // 非法语法（matchit 混合段）
    assert!(check_patterns(&["u/{id}.json".into()]).is_err());
    // 同位异名参数（两个 _x_ 目录的产物形态）
    assert!(check_patterns(&["u/{aa}".into(), "u/{bb}".into()]).is_err());
}
```

`oj/src/build_cmd.rs` tests 追加（纯函数级，先不依赖构建夹具）：

```rust
#[test]
fn rel_pattern_converts_underscore_dirs_and_module() {
    // 目录镜像：rel_dir 与模块段逐段转换
    assert_eq!(rel_pattern("user", "_id_", None), "user/{id}");
    assert_eq!(rel_pattern("_mod_", "x", None), "{mod}/x");
    assert_eq!(rel_pattern("_mod_", "_id_", None), "{mod}/{id}");
    // 相对 .route 拼在转换后的目录后
    assert_eq!(rel_pattern("user", "_id_", Some("{sub}")), "user/{id}/{sub}");
    // 根级 .route 不吃目录转换
    assert_eq!(rel_pattern("user", "_id_", Some("/v2/x")), "v2/x");
    // .route 值不转换：`_name_` 在其中是字面段
    assert_eq!(rel_pattern("user", "item", Some("_id_")), "user/item/_id_");
    // 不转换的形态保持字面
    assert_eq!(rel_pattern("user", "_shared", None), "user/_shared");
    assert_eq!(rel_pattern("user", "__x__", None), "user/__x__");
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --release -p server check_patterns
cargo test --release -p oj rel_pattern
```
预期：编译失败（函数不存在）/`{id}` 断言失败（未转换）。

- [ ] **Step 3: 实现 `check_patterns`（routes.rs，`entries_from_value` 前插入）**

```rust
/// 构建期 pattern 校验（oj build fail-fast，v0.1.27）：逐条插入临时 matchit Router
/// （补首斜杠），非法语法 / 同位异名参数 → Err。此前这类错误要到部署启动才爆
/// （release 启动对 from_entries failures 硬失败）。重复 pattern（多方法）可合并。
pub fn check_patterns(patterns: &[String]) -> Result<(), String> {
    let mut r = matchit::Router::new();
    for p in patterns {
        let full = format!("/{}", p.trim_start_matches('/'));
        r.insert(full.clone(), ())
            .map_err(|e| format!("invalid route pattern {full:?}: {e}"))?;
    }
    Ok(())
}
```

- [ ] **Step 4: 实现 `rel_pattern` 转换（build_cmd.rs）**

整体替换为：

```rust
/// 相对 pattern（spec §2.1）：无首斜杠无 base，含模块名段。
/// None/空 route → 目录镜像（v0.1.27：模块段与 rel_dir 逐段过 `fs_seg_to_pattern`，
/// `_name_` → `{name}`）；相对声明 → 目录 + route；根级声明（/ 开头）→ 剥首斜杠
/// 不加模块段。route 值**不转换**（matchit 语法，`{name}` 才是参数）。
fn rel_pattern(module: &str, rel_dir: &str, route: Option<&str>) -> String {
    let m = routes::fs_seg_to_pattern(module);
    let conv_dir = |d: &str| {
        d.split('/')
            .map(routes::fs_seg_to_pattern)
            .collect::<Vec<_>>()
            .join("/")
    };
    match route.map(str::trim).filter(|r| !r.is_empty()) {
        None => {
            if rel_dir.is_empty() {
                m
            } else {
                format!("{m}/{}", conv_dir(rel_dir))
            }
        }
        Some(r) if r.starts_with('/') => r.trim_start_matches('/').to_string(),
        Some(r) => {
            if rel_dir.is_empty() {
                format!("{m}/{r}")
            } else {
                format!("{m}/{}/{r}", conv_dir(rel_dir))
            }
        }
    }
}
```

- [ ] **Step 5: `build_one` 生成段重构（build_cmd.rs L389-410）**

替换为（先收集→校验→告警→写盘）：

```rust
    // 3. 内省（内存库）→ 校验/告警 → routes.js：pattern 无 base 含模块段（rel_pattern），
    //    file = 同名产物相对版本目录根（正斜杠；根级 api.ts 为裸 api.js）
    let decls = introspect_module_files(src, &mdir, &files).await?;
    let n_api = decls.len();
    // v0.1.27 告警：`_name_` 形态模块名 → 路由参数段，同位异名双模块会在部署启动期结构冲突。
    if routes::looks_like_underscore_param(module) {
        eprintln!(
            "warn: module name {module:?} looks like `_name_` fs-param syntax; it becomes a route param segment (two such modules with different names at the same slot will conflict at deployment startup)"
        );
    }
    let mut rows_out: Vec<(String, String, String)> = Vec::new(); // (file, method, pattern)
    let mut pats: Vec<String> = Vec::new();
    for (dir, rows) in &decls {
        let file = if dir.is_empty() {
            "api.js".to_string()
        } else {
            format!("{dir}/api.js")
        };
        for (method, route) in rows {
            if let Some(r) = route.as_deref().map(str::trim).filter(|r| !r.is_empty())
                && r.split('/').any(routes::looks_like_underscore_param)
            {
                eprintln!("warn: {file}: .route value {r:?} looks like `_name_` fs-param syntax; in .route it stays a literal segment (use {{name}} for params)");
            }
            let pat = rel_pattern(module, dir, route.as_deref());
            pats.push(pat.clone());
            rows_out.push((file.clone(), method.clone(), pat));
        }
    }
    // 构建期 fail-fast：非法 pattern / 同位异名参数在 build 期报错，免得到部署启动才爆。
    routes::check_patterns(&pats)?;
    let mut js = String::from("// 由 oj build 生成；勿手改。\nexport default [\n");
    for (file, method, pat) in &rows_out {
        js.push_str(&format!(
            "  {{ method: {}, pattern: {}, file: {} }},\n",
            q(method),
            q(pat),
            q(file)
        ));
    }
    js.push_str("];\n");
    std::fs::write(vdir.join("routes.js"), js).map_err(|e| format!("write routes.js: {e}"))?;
```

- [ ] **Step 6: 跑测试确认通过 + fmt + clippy**

```bash
cargo test --release -p server check_patterns
cargo test --release -p oj rel_pattern
cargo fmt && cargo clippy --release --all-targets -- -D warnings
```

- [ ] **Step 7: 构建产物集成测试（build_cmd.rs tests）**

参照 tests 内现有构建夹具（`build_emits_routes_js_strips_route_then_release_serves` 附近的 TempDir 用法），新增：

```rust
#[test]
fn build_converts_underscore_dirs_and_rejects_bad_pattern() {
    // 夹具形态复制本文件内既有 build 测试的 src 摆法（manifest.yaml + api.ts），
    // 这里给关键断言：
    // 1) src/user/_id_/api.ts（get，无 .route）→ dist routes.js 含
    //    `"pattern": "user/{id}"`（或 q() 引号形态）且 `"file": "user/_id_/api.js"`；
    // 2) 另一模块或同模块 api.ts 写 `get.route = "{id}.json"`（matchit 非法）
    //    → build 返回 Err，msg 含 "invalid route pattern"；
    // 3) `.route = "_id_"`（字面段写法）→ 构建成功，routes.js pattern 为
    //    `user/_id_`（字面），证明 .route 值不转换。
}
```

具体夹具代码在实现时从相邻既有测试复制改造（保持 `#[tokio::test(flavor = "current_thread")]` 或同步测试形态与邻居一致）。

- [ ] **Step 8: 提交**

```bash
git add server/src/routes.rs oj/src/build_cmd.rs
git commit -m "feat(build): rel_pattern 转换 _name_ 目录 + 构建期 pattern 试插校验 fail-fast + _name_ 形态告警

unix@vip.qq.com ai"
```

---

### Task 5: e2e — dev 起服 + build→release round trip

**Files:**
- Modify: `oj/tests/e2e.rs`

**Interfaces:**
- Consumes: Task 2-4 全部。

- [ ] **Step 1: 读现有 e2e 形态**

```bash
grep -n "build_emits_routes_js_strips_route_then_release_serves\|fn dev_\|TempDir" oj/tests/e2e.rs | head -20
```
找到 dev 起服与 build→release 夹具的写法，复用其 harness（起服 helper、config 生成、curl 断言形态）。

- [ ] **Step 2: 写失败测试**

新增用例（形态对齐邻居；关键断言如下）：

```rust
// dev 模式：src 内 user/_id_/api.ts，handler 返回 http.param("id")；
// curl {base}/user/42 → 200 且 body 含 "42"；curl {base}/user/_id_ 同 handler（b="_id_"）。
// build 同一 src → release 直载 → curl {base}/user/42 → 同等 body。
```

handler 源码示例：

```ts
function get() {
  json.ok({ id: http.param("id") });
}
export default { get };
```

- [ ] **Step 3: 跑测试确认失败/通过**

```bash
cargo test --release -p oj --test e2e underscore
```
Task 2-4 已完成时新用例应直接通过（特性已就位）；若先写测试则此处应失败。保持 red-green 记录。

- [ ] **Step 4: fmt + clippy + 提交**

```bash
cargo fmt && cargo clippy --release --all-targets -- -D warnings
git add oj/tests/e2e.rs
git commit -m "test(e2e): _name_ 目录参数 dev 起服 + build→release round trip

unix@vip.qq.com ai"
```

---

### Task 6: 文档与版本（CHANGELOG + devkit 四件 + dev-guide + sample）

**Files:**
- Modify: `CHANGELOG.md`（头部加 v0.1.27 节）
- Modify: `oj/Cargo.toml`（version 0.1.26 → 0.1.27）
- Modify: `docs/devkit/api-manual.md`（§361 路由章 + §13 限制表）
- Modify: `docs/devkit/SKILL.md`（常见陷阱速查表）
- Modify: `docs/devkit/scenarios.md`（追加新场景，编号顺延现有最大编号）
- Modify: `docs/devkit/README.md`（可见变更同步）
- Modify: `docs/dev-guide.md`（L189 管线 + L219-221 目录镜像节）
- Modify: `sample/src/user/_id_/api.ts`（新建）+ `sample/README.md`（一行 curl）
- 归置：bin/devkit/（cargo xtask build 产出，不手改）

**Interfaces:**
- Consumes: Task 1-5 全部已提交。

- [ ] **Step 1: 版本递增 + CHANGELOG**

`oj/Cargo.toml` version 改为 `0.1.27`（Cargo.lock 随下次构建自动同步，提交时一并 add）。

`CHANGELOG.md` 在 `## v0.1.26` 之前插入：

```markdown
## v0.1.27（2026-09-25）

> 版本分界按仓库约定落在 `oj/Cargo.toml` 的递增提交上（本版 `0.1.26 → 0.1.27`）。**未打标签**（发布点标签待补）。上一版：`v0.1.26` → `b91a611`。

**特性**

- **`_name_` 目录段即路径参数（v0.1.27）**：文件系统中的动态参数目录段用 `_name_`
  整段表示（避免 `{}` 进文件路径带来的 shell 转义成本），映射 URL 时转换为
  `{name}`。例：`src/user/_id_/api.ts` → `/v1/api/user/{id}`；
  `src/_aa_/bb/_cc_/api.ts` → `/{aa}/bb/{cc}`。模块段同样适用。
  - dev 建表与 dev 目录镜像兜底、release（`oj build` 生成的 routes.js）三链路同口径；
    兜底经 `_x_` 段下降时同样提取参数（复用 `decode_params` 走私防线）。
  - 谓词：整段 `_name_`（首尾各一个下划线、内部名不以 `_` 开头/结尾、不含 `{}`）。
    `__x__`/`___`/`_a{b}_` 保持字面；`_shared`/`_platform` 等无尾下划线目录不受影响。
  - **`oj build` 构建期 pattern 试插校验**：非法 pattern / 同位异名参数在 build 期即
    失败（此前到部署启动才爆）。
  - **告警**：`.route` 值匹配 `_name_` 形态（在 `.route` 中它是**字面段**，参数写
    `{name}`）与 `_name_` 形态模块名（同位异名双模块部署启动会结构冲突）——
    dev 启动与 oj build 均打 warn。

**升级注意（breaking-adjacent）**

- 存量项目若已有**字面目**的 `_x_` 目录（如 `_v1_/`），升级后其段将变为参数段：
  URL 仍可达（实参 `_v1_` 落进参数），但 handler 会收到非空 `http.params`，
  启动 banner 的路由表会显示 `{x}` 形态。确有字面需要的目录请避免首尾下划线写法。
```

- [ ] **Step 2: api-manual.md 路由章**

§361 的 `.route` 规则列表附近追加一条一级规则：

```markdown
- **目录段 `_name_` 即路径参数（v0.1.27）**：目录名写成整段 `_name_`（首尾各一个
  下划线），URL 中即 `{name}`。例：`user/_id_/api.ts` → `/v1/api/user/{id}`，
  `http.param("id")` 取值。模块段同样适用（`_aa_/bb/api.ts` → `/{aa}/bb`）。
  谓词：整段、内部名非空且不以 `_` 开头/结尾、不含 `{}`——`__x__`/`_a{b}_` 保持
  字面；`_shared`（无尾下划线）不受影响。可与 `.route` 自由组合：
  `_id_/api.ts` 挂 `.route = "{sub}"` → `{id}/{sub}`。URL 字面写 `_id_` 会被
  `{id}` 当实参吃掉（不再有静态 `_id_` 路由）。
```

§13「v0.2 已知限制全表」追加三行（表格列形态对齐邻居：`| 限制 | 说明 / 绕行 |`）：

```markdown
| `_name_` 必须整段且参数名合法 | `__x__`/`___`/`_a{b}_` 不转换（保持字面）；转换由 `oj build` 期 pattern 试插校验把关，非法即构建失败 |
| 同层异名 `_x_` 目录是结构性冲突 | `_aa_/` 与 `_bb_/` 并存 → 后者启动丢弃并告警（matchit 同位异名规则，与 `.route` 同） |
| WS 目录镜像不转换 `_name_` | `_name_/ws.ts` 暴露字面 URL；WS 侧暂不支持动态段（v0.2 评估） |
```

- [ ] **Step 3: SKILL.md 陷阱速查**

「常见陷阱速查」表追加两行：

```markdown
| `.route = "_id_"` 没参数化 | `.route` 值是 matchit 语法：`_name_` 在其中是**字面段**；参数写 `{name}`——`_name_` 是**目录**写法（dev/build 会对该形态打 warn） |
| 请求 `/…/_aa_/…` 落进了参数路由 | `_aa_` 目录转换后不再有静态 `_aa_` 路由；URL 里写 `_aa_` 会被 `{aa}` 当实参吃掉（实参值就是 `"_aa_"`） |
```

- [ ] **Step 4: scenarios.md 追加场景**

读文件尾确定下一个编号（现有至场景 7），追加「场景 N：路径参数路由——`_name_` 目录 vs `.route`」：

```markdown
## 场景 N：路径参数路由——`_name_` 目录 vs `.route`

### ① 路由文件

src 下 `user/_id_/api.ts`：

​```ts
function get() {
  json.ok({ id: http.param("id") });
}
export default { get };
​```

URL：`/v1/api/user/42` → `{ id: "42" }`。目录段 `_id_`（首尾各一个下划线）即声明
参数；深层同理（`user/_id_/item/_sku_/api.ts` → `/user/{id}/item/{sku}`）。

### ② 与 .route 组合

`_id_/api.ts` 内 `get.route = "{sub}"` → `/user/{id}/{sub}`；`.route` 值本身用
matchit 语法（`{name}`），`_name_` 写进 `.route` 是字面段（会打 warn）。

### ③ 常见坑

- `__x__`/`_a{b}_` 不转换（保持字面）；`_shared` 无尾下划线，不受影响。
- 同层 `_aa_/` 与 `_bb_/` 并存 = 同位异名结构冲突，后者被丢弃并告警。
```

- [ ] **Step 5: devkit README + dev-guide + sample**

- `docs/devkit/README.md`：按该文件既有的版本同步段形态补 v0.1.27 一行可见变更（先读该文件确认形态）。
- `docs/dev-guide.md` L219-221 目录镜像节补一句：「目录段 `_name_` 映射 `{name}` 动态参数（v0.1.27，谓词见 api-manual §路由）」；L189 管线描述「dev 目录镜像兜底」处注明兜底亦可经 `_x_` 下降并提取参数。
- sample：读 `sample/src/user/item/api.ts` 与 `sample/README.md` 的形态，新建
  `sample/src/user/_id_/api.ts`（handler `json.ok({ id: http.param("id") })`，风格对齐 item 示例），README 加一行：
  `curl http://localhost:9778/v1/api/user/42`（与既有 curl 例子同列）。
  然后重建该模块产物：`./bin/oj build -d sample/src -o sample/dist user`（若 CLI 不支持单模块参数则整量 `-d sample/src -o sample/dist`；按 `oj build --help` 实际形态）。

- [ ] **Step 6: 归置 + 全量校验**

```bash
cargo xtask build          # 构建 oj + 插件，归置 bin/ 与 bin/devkit/
cargo test --release -p xtask   # devkit 契约用例：产物与源一致
cargo test --release --workspace
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings
```

全部通过。若 xtask 契约测试报错，按报错把源与产物改到一致（通常是漏归置或文本不一致）。

- [ ] **Step 7: 提交**

```bash
git add -A
git commit -m "feat(v0.1.27): _name_ 目录段→{name} 动态参数 + devkit 四件同步

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec 覆盖**：§2 helper→T1；§3 挂接点 1/2→T2、3/4→T3、build 侧→T4；§4 校验/告警→T2+T4；§6 测试→T2/T3/T4/T5；§7 文档→T6。无缺口。
- **类型一致**：`resolve` 新签名 `(PathBuf, HashMap<String,String>)` 在 T3 实现与 lib.rs 改法中一致；`warning: ` 前缀约定在 T2 生产、app.rs 消费；`check_patterns(&[String]) -> Result<(), String>` 在 T4 定义与消费一致。
- **占位符**：T4 Step 7 的集成测试允许从邻居夹具复制（文件内既有模式，非 TBD）；T6 Step 5 要求先读目标文件确认形态——均为"读既有模式"而非留空。
