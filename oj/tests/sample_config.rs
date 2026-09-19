//! 发行样例配置的解析回归：`sample/config.yaml` 与 `sample/config.docker.yaml` 是随仓库
//! 分发、被 CI（`sample-tests`）与用户直接照抄的产物——它们必须能被当前代码解析。
//!
//! 动机（v0.1.23）：`anonymous_paths` 引入条目对象形态 `{ path, one_layer }` 后，样例里
//! 的 YAML 一旦写错（缩进、键名、标记位置），错误只会在**装配期**暴露，且 `oj build` 读
//! 配置时是宽容的（`unwrap_or` 吞错）——没有本测试就只能靠人肉跑 server 才发现。

use std::path::PathBuf;

fn sample_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("sample")
}

fn load(name: &str) -> only_js::config::Config {
    let dir = sample_dir();
    only_js::config::load_from(&dir, Some(name))
        .unwrap_or_else(|e| panic!("sample/{name} 必须能被当前代码解析：{e}"))
}

#[test]
fn sample_configs_parse_and_pass_anon_path_validation() {
    for name in ["config.yaml", "config.docker.yaml"] {
        let c = load(name);
        // 装配期校验（`one_layer` 只许标在尾 "/*" 条目上）必须过——样例是照抄源，
        // 它自己先得合法。
        only_js::config::validate_anon_paths(&c)
            .unwrap_or_else(|e| panic!("sample/{name} 的 anonymous_paths 校验失败：{e}"));

        // 归一化：两种形态都要落到路径字符串（server Pipeline / oj-auth 插件 cfg 的输入）。
        let tenant = only_js::config::anon_paths(&c.tenant.anonymous_paths);
        let auth = only_js::config::anon_paths(
            &c.auth.as_ref().expect("sample 配了 auth").anonymous_paths,
        );
        for p in ["/oidc/*", "/idp/*", "/idp/.well-known/*"] {
            assert!(tenant.iter().any(|x| x == p), "{name}: tenant 缺 {p}");
            assert!(auth.iter().any(|x| x == p), "{name}: auth 缺 {p}");
        }

        // `/idp/*` 在样例里是**显式确认的有意一层**（discovery 由 `/idp/.well-known/*` 单列，
        // 豁免面最小）→ 装配期不会再对它打迁移 WARN。
        let idp = c
            .tenant
            .anonymous_paths
            .iter()
            .find(|p| p.path() == "/idp/*")
            .unwrap_or_else(|| panic!("{name}: tenant 缺 /idp/*"));
        assert!(
            idp.one_layer(),
            "{name}: /idp/* 必须带 one_layer: true（否则启动会打迁移 WARN）"
        );
        // `/oidc/*` 无更深路由 → 影响面判定为静默，用字符串简写即可（也顺带钉住简写形态）。
        let oidc = c
            .tenant
            .anonymous_paths
            .iter()
            .find(|p| p.path() == "/oidc/*")
            .unwrap_or_else(|| panic!("{name}: tenant 缺 /oidc/*"));
        assert!(!oidc.one_layer(), "{name}: /oidc/* 不该有 one_layer");
    }
}
