    use super::*;

    /// Given: ext_boot.js 合法（仅赋值全局）；When: prewarm_boot；Then: Ok —— boot 模块
    /// 借真实 runtime 加载执行成功（与生产同路径：独立线程 + current_thread runtime）。
    #[test]
    fn given_valid_ext_boot_when_prewarm_then_ok() {
        let dir = std::env::temp_dir().join(format!("oj-prewarm-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ext_boot.js"), "globalThis.__oj_booted = true;\n").unwrap();
        let spec = ext_boot_spec(&dir).unwrap().unwrap();
        let loader = Arc::new(LoaderShared {
            project_root: dir.canonicalize().unwrap(),
            ts: true,
        });
        let r = prewarm_boot(move || {
            Bridge::with_dbs_and_loader(
                std::collections::HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                Some(loader.clone()),
                Extras {
                    boot: Some(spec.clone()),
                    ..Default::default()
                },
            )
        });
        assert!(r.is_ok(), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Given: boot 指向不存在的模块；When: prewarm_boot；Then: Err 点名 ext_boot 并附
    /// 重启提示（boot 错误前移到装配期 = 真·启动失败，而非「路由全空、服务照常监听」）。
    #[test]
    fn given_missing_boot_module_when_prewarm_then_err_named_ext_boot() {
        let r = prewarm_boot(|| {
            Bridge::with_dbs_and_loader(
                std::collections::HashMap::new(),
                Arc::new(InMemoryKV::new()),
                SchemaRegistry::new(),
                false,
                None,
                Extras {
                    boot: Some("file:///oj/ext_boot_missing_deadbeef.js".into()),
                    ..Default::default()
                },
            )
        });
        let Err(e) = r else {
            panic!("expected prewarm failure for missing boot module");
        };
        assert!(e.contains("ext_boot"), "{e}");
        assert!(e.contains("restart the process"), "{e}");
    }

    /// Given: make_bridge 闭包 panic；When: prewarm_boot；Then: Err 收敛（线程 join
    /// 接住 panic，宿主不 abort，文案点名 panicked）。
    #[test]
    fn given_bridge_factory_panics_when_prewarm_then_err() {
        let r = prewarm_boot(|| panic!("boom"));
        let Err(e) = r else {
            panic!("expected panic-to-err convergence");
        };
        assert!(e.contains("panicked"), "{e}");
    }
