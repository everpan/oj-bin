    use super::*;
    use oj_plugin_ffi::{FfiFuture, MqVtable, RString};

    // 假 mq vtable：connect 恒 {"handle":7}，call 回显 payload（装配测试零网络）。
    extern "C" fn fake_connect(cfg: RString) -> FfiFuture {
        let _ = cfg;
        oj_plugin_ffi::ready_ok(br#"{"handle":7}"#)
    }
    extern "C" fn fake_call(_h: u64, m: RString, p: RString) -> FfiFuture {
        let out = format!(r#"{{"method":"{}","payload":{}}}"#, &m[..], &p[..]);
        oj_plugin_ffi::ready_ok(out.into_bytes())
    }
    extern "C" fn fake_close(_h: u64) {}
    static FAKE_MQ: MqVtable = MqVtable {
        connect: fake_connect,
        call: fake_call,
        close: fake_close,
    };

    fn mq_table(names: &[&str]) -> Vec<(String, &'static MqVtable)> {
        names.iter().map(|n| (n.to_string(), &FAKE_MQ)).collect()
    }

    fn cfg_with(section: serde_json::Value, which: &str) -> Config {
        let mut cfg = Config::default();
        let map: std::collections::HashMap<String, serde_json::Value> =
            serde_json::from_value(section).unwrap();
        if which == "kafkas" {
            cfg.kafkas = map;
        } else {
            cfg.rabbits = map;
        }
        cfg
    }

    /// Given: kafkas.default 声明 + bus-kafka 插件；Then: registry 命名实例就位。
    #[tokio::test(flavor = "current_thread")]
    async fn given_kafkas_section_with_kafka_plugin_when_build_then_registered() {
        let cfg = cfg_with(
            serde_json::json!({ "default": { "brokers": ["b:9092"] } }),
            "kafkas",
        );
        let (kafkas, rabbits) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]), None, None)
            .await
            .unwrap();
        assert!(kafkas.get("default").is_some());
        assert!(rabbits.get("default").is_none());
        // kind 注入核验：cfg 透传含 kind=kafka（fake connect 忽略，仅验通路不 panic）
    }

    /// Given: kafkas 声明但无 kafka 插件；Then: fail-fast 文案带 kind（评审 F1/N2）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_kafkas_without_matching_plugin_when_build_then_err() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "kafkas");
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-rabbitmq"]), None, None).await
        else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("no mq plugin for kind 'kafka'"), "{e}");
    }

    /// Given: rabbits 声明只装 kafka 插件（负例，评审 N2）；Then: fail-fast。
    #[tokio::test(flavor = "current_thread")]
    async fn given_rabbits_with_only_kafka_plugin_when_build_then_err() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "rabbits");
        let Err(e) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]), None, None).await else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("no mq plugin for kind 'rabbit'"), "{e}");
    }

    /// Given: 两插件同名 kind 冲突；Then: fail-fast（评审 S5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_duplicate_kind_plugins_when_build_then_err_conflict() {
        let cfg = cfg_with(serde_json::json!({ "default": {} }), "kafkas");
        let Err(e) =
            build_mq_registries(&cfg, &mq_table(&["bus-kafka", "bus-kafka"]), None, None).await
        else {
            panic!("expected fail-fast");
        };
        assert!(e.contains("multiple mq plugins serve kind 'kafka'"), "{e}");
    }

    /// Given: 两段都空；Then: 空 registry（不报错，JS undefined 语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_empty_sections_when_build_then_empty_registries() {
        let cfg = Config::default();
        let (kafkas, rabbits) = build_mq_registries(&cfg, &mq_table(&["bus-kafka"]), None, None)
            .await
            .unwrap();
        assert!(kafkas.is_empty() && rabbits.is_empty());
    }
}

