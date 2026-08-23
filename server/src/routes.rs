//! 目录镜像路由：URL = base 之后的目录路径 → `<root>/<path>/api.(ts|js)`。
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
        Self { base, root: root.into(), ts }
    }

    /// 解析 HTTP 路径 → api 文件绝对路径；目录不存在/越界/非文件 → None。
    pub fn resolve(&self, http_path: &str) -> Option<PathBuf> {
        let rel = http_path.strip_prefix(self.base.as_str())?;
        let rel = rel.trim_matches('/');
        if rel.is_empty() {
            return None;
        }
        // 安全：拒绝空段与越界段（目录穿越按 404 处理）。
        if rel.split('/').any(|s| {
            s.is_empty() || s == ".." || s == "." || s.contains('\\') || s.contains('\0')
        }) {
            return None;
        }
        let file = self.root.join(rel).join(if self.ts { "api.ts" } else { "api.js" });
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
    if t[1..].split('/').any(|s| s.is_empty() || s == "." || s == "..") {
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
        let v = percent_encoding::percent_decode_str(&raw).decode_utf8().ok()?;
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

/// 收集 root 下全部路由相对路径（启动时打印路由表用，UC-8）。
pub fn route_table(root: &Path, ts: bool) -> Vec<String> {
    let ext = if ts { "api.ts" } else { "api.js" };
    let mut out = Vec::new();
    walk(root, ext, &mut Vec::new(), &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, ext: &str, rel: &mut Vec<String>, acc: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            rel.push(name);
            walk(&p, ext, rel, acc);
            rel.pop();
        } else if name == ext {
            acc.push(rel.join("/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(files: &[&str]) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "oj-routes-{}-{:p}", std::process::id(), files as *const _
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
        assert_eq!(r.resolve("/v1/api/user/account/"), Some(root.join("user/account/api.ts")));
        assert_eq!(r.resolve("/v1/api/user/account"), Some(root.join("user/account/api.ts")));
        assert_eq!(r.resolve("/v1/api/user/profile/detail/"),
                   Some(root.join("user/profile/detail/api.ts")));
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
        assert!(Routes::new("/v1/api", &root, false).resolve("/v1/api/user/account/").is_some());
        assert!(Routes::new("/v1/api", &root, true).resolve("/v1/api/user/account/").is_none());
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
        assert_eq!(normalize("/v1/api/user/account/"), Some("/v1/api/user/account".into()));
        assert_eq!(normalize("/v1/api/user/account"), Some("/v1/api/user/account".into()));
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
}
