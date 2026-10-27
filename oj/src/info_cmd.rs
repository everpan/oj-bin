//! `oj info`：phpinfo 对应物 —— OjInfo 装配后的纯文本展示（诊断/排障入口）。
//! 装配面在 `serve_cmd::assemble_for_info`（与 serve 共用加载/config 解析路径，
//! 但不 connect、不监听）。

pub async fn run(config: Option<&str>) -> Result<(), String> {
    let info = crate::serve_cmd::assemble_for_info(config).await?;
    println!("{}", info.to_text());
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::serve_cmd::OjInfo;

    fn sample() -> OjInfo {
        OjInfo {
            build: serde_json::json!({"oj": env!("CARGO_PKG_VERSION"), "profile": "release"}),
            abi: serde_json::json!({"abi_version": oj_plugin_ffi::ABI_VERSION}),
            plugins: vec![],
            backends: serde_json::json!({"db_schemes": ["sqlite://"]}),
            config: serde_json::json!({"sections": ["server"], "unconsumed": []}),
            generic_axes: vec!["greet".to_string()],
            unconsumed_sections: vec![],
        }
    }

    #[test]
    fn text_report_has_all_sections_and_no_values() {
        let t = sample().to_text();
        for sec in ["build", "abi", "plugins", "backends", "config"] {
            assert!(t.contains(sec), "missing section {sec}");
        }
        // 零泄漏：config 段只出键名不出值（JSON 形态的 config 不含段值）：
        let v = serde_json::to_value(sample()).unwrap();
        assert!(v["config"]["sections"].is_array());
        assert!(v["config"].get("server").is_none());
    }

    /// T13 两出口对齐：db 轴插件的方言能力位在 `oj info` 文本里也要看得见（启动横幅有、
    /// info 没有 → 排障时选错工具）。非 db 轴插件（caps=None）不凭空造行。
    #[test]
    fn text_report_shows_db_caps_like_the_serve_banner() {
        use only_js::bridge::dialect_caps::{
            DialectCaps, DialectInfo, IdStrategy, PlaceholderStyle,
        };
        use only_js::bridge::plugin_loader::PluginInfo;
        let mk = |name: &str, caps: Option<DialectInfo>| PluginInfo {
            name: name.to_string(),
            semver: "0.1.0".to_string(),
            abi_version: oj_plugin_ffi::ABI_VERSION,
            fingerprint: "fp".to_string(),
            description: "d".to_string(),
            host_abi_version: oj_plugin_ffi::ABI_VERSION,
            caps,
        };
        let mut info = sample();
        info.plugins = vec![
            mk(
                "oj-db-postgres",
                Some(DialectInfo {
                    name: "postgres".to_string(),
                    caps: DialectCaps {
                        placeholder: PlaceholderStyle::Dollar,
                        id: IdStrategy::Returning,
                        stream: true,
                        cancel: false,
                    },
                }),
            ),
            mk("oj-kv-redis", None),
        ];
        let t = info.to_text();
        assert!(
            t.contains("caps: placeholder=$ id=returning stream"),
            "db 插件必须出 caps 行: {t}"
        );
        // 非 db 轴插件：只有一行 caps 行（不重复、不造假）。
        assert_eq!(t.matches("  caps: ").count(), 1, "{t}");
        // JSON 出口是同一套 describe 词汇（下游可与 GET /plugins 对账）。
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["plugins"][0]["caps"]["dialect"], "postgres");
        assert_eq!(
            v["plugins"][0]["caps"]["capabilities"][0],
            "placeholder_dollar"
        );
        assert!(v["plugins"][1]["caps"].is_null());
    }
}
