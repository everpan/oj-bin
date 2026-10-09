//! `oj info`：phpinfo 对应物 —— OjInfo 装配后的纯文本展示（诊断/排障入口）。
//! 装配面在 `serve_cmd::assemble_for_info`（与 serve 共用加载/config 解析路径，
//! 但不 connect、不监听）。

pub async fn run(config: &str) -> Result<(), String> {
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
}
