    use super::*;
    use oj_plugin_ffi::{FfiFuture, MailAttachment, MailVtable, RString, RVec};
    use only_js::bridge::RequestInfo;
    use only_js::bridge::mail::{CallCtx, MailMode, handle_send};

    /// 假 mail vtable：submit 回固定信封（装配测试零网络、零插件）。jobId 回显 key，
    /// 证明 profile 名确实过线（不是被适配器吞掉）。控制报文（`__ctl`）回排空信封，
    /// 并把收到的 req 记进 [`SEEN_REQS`]（A4 停机排空用）。
    static SEEN_REQS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    extern "C" fn fake_submit(
        key: RString,
        req: RString,
        _atts: RVec<MailAttachment>,
    ) -> FfiFuture {
        let req = req[..].to_string();
        let is_ctl = req.contains("__ctl");
        SEEN_REQS.lock().unwrap().push(req);
        if is_ctl {
            return oj_plugin_ffi::ready_ok(
                br#"{"code":0,"msg":"ok","data":{"drained":true,"workers":2}}"#.to_vec(),
            );
        }
        oj_plugin_ffi::ready_ok(
            format!(
                r#"{{"code":0,"msg":"ok","data":{{"jobId":"j-{}"}}}}"#,
                &key[..]
            )
            .into_bytes(),
        )
    }
    static FAKE_MAIL: MailVtable = MailVtable {
        submit: fake_submit,
    };

    /// 「控制报文却回 pending」的坏插件（A4：宿主必须 fail-loud，不挂死）。
    extern "C" fn pending_submit(
        _key: RString,
        _req: RString,
        _atts: RVec<MailAttachment>,
    ) -> FfiFuture {
        extern "C" fn poll(_s: *mut std::ffi::c_void) -> i32 {
            0
        }
        extern "C" fn take(
            _s: *mut std::ffi::c_void,
        ) -> oj_plugin_ffi::RResult<oj_plugin_ffi::RBytes, RString> {
            oj_plugin_ffi::RResult::Err(RString::from("not ready"))
        }
        extern "C" fn free(_s: *mut std::ffi::c_void) {}
        FfiFuture {
            state: std::ptr::null_mut(),
            poll,
            take,
            free,
        }
    }
    static PENDING_MAIL: MailVtable = MailVtable {
        submit: pending_submit,
    };

    /// 顶层 `smtp:` 段：多 profile（默认/本地落盘）+ 白名单 + 并发参数。
    fn smtp_cfg() -> Config {
        Config {
            smtp: Some(
                serde_yaml::from_str(
                    "workers: 2\n\
                     queue_capacity: 8\n\
                     default:\n  host: smtp.example.com\n  port: 465\n  tls: tls\n  \
                     mechanism: login\n  user: u\n  pass: p\n  \
                     allowed_from: [noreply@x.com]\n  allowed_recipients: [\"@x.com\"]\n\
                     mock:\n  host: localhost\n  port: 25\n  tls: none\n  allow_none_tls: true\n  \
                     mechanism: login\n  file_transport: /tmp/oj-mail-assembly-eml\n",
                )
                .unwrap(),
            ),
            ..Default::default()
        }
    }

    /// Given: 顶层 `smtp:` 段（多 profile + 白名单）+ oj-mail 插件在册；
    /// When: 装配期构造 mail 后端；
    /// Then: `Extras.mail` 注入可用后端 —— 宿主校验面与插件 cfg **同源**，
    /// JS 全局 `mail.send` 走通「宿主校验 → vtable」全链（越白名单仍 code:5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_smtp_configured_when_assemble_then_mail_backend_injected() {
        let cfg = smtp_cfg();
        let bus: Arc<dyn EventBroker> = Arc::new(only_js::bridge::Bus::new());
        let mb = build_mail_backend(&cfg, &serde_json::Value::Null, Some(&FAKE_MAIL), bus)
            .unwrap()
            .expect("smtp 段 + 插件在册 → 必须注入 mail 后端");
        // 非密钥面：profile 名单 / 白名单（宿主校验依据）。
        assert_eq!(mb.config().profile_keys(), vec!["default", "mock"]);
        let p = mb.config().profile("default").unwrap();
        assert_eq!(p.allowed_from, vec!["noreply@x.com".to_string()]);
        assert_eq!(p.allowed_recipients, vec!["@x.com".to_string()]);
        // mock profile 未声明白名单 → 空表（fail-closed 的判定依据）。
        assert!(mb.config().profile("mock").unwrap().allowed_from.is_empty());

        // 经 Extras → StableState → JS 全局：装配产物真的能服务 `mail.*`。
        let b = Bridge::with_dbs_and_loader(
            std::collections::HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras {
                mail: Some(mb.clone()),
                ..Default::default()
            },
        );
        let cap = b
            .run_with(
                r#"(async () => {
                    const ok = await mail.send({ from: "noreply@x.com", to: ["a@x.com"], text: "hi" });
                    const bad = await new Mail("default").send({ from: "evil@y.com", to: ["a@x.com"], text: "hi" });
                    json.ok({ ok, bad });
                  })().catch((e) => json.ok({ err: String(e) }));
                  "#,
                RequestInfo::default(),
            )
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
        assert!(v["data"].get("err").is_none(), "{v}");
        assert_eq!(v["data"]["ok"]["code"], 0, "{v}");
        assert_eq!(v["data"]["ok"]["data"]["jobId"], "j-default", "{v}");
        assert_eq!(v["data"]["bad"]["code"], 5, "{v}");
        assert!(
            v["data"]["bad"]["msg"]
                .as_str()
                .unwrap()
                .contains("allowed_from"),
            "{v}"
        );

        // 直接编排（同一后端）：白名单放行的那封同样到插件（key 过线）。
        let env = handle_send(
            mb.clone(),
            Arc::new(BlobRegistry::new()),
            None,
            "mock",
            &serde_json::json!({
                "from": "anyone@localhost", "to": ["a@localhost"], "text": "x",
            })
            .to_string(),
            MailMode::Send,
            &CallCtx::default(),
        )
        .await;
        // mock profile 白名单为空 → fail-closed（即便 profile 存在也拒绝）。
        assert_eq!(env["code"], 5, "{env}");
        assert!(
            env["msg"].as_str().unwrap().contains("allowed_from"),
            "{env}"
        );
    }

    /// Given: 未配 `smtp:`（或空段）／配了但插件未加载；
    /// Then: 不挂后端（None）——`mail.*` 调用报 "mail not configured"
    /// （与 es/auth 的「未配置」语义一致；不静默降级成空 profile 的谜之 code:5）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_no_smtp_or_no_plugin_when_assemble_then_no_backend() {
        let bus = || -> Arc<dyn EventBroker> { Arc::new(only_js::bridge::Bus::new()) };
        // 段缺省。
        assert!(
            build_mail_backend(
                &Config::default(),
                &serde_json::Value::Null,
                Some(&FAKE_MAIL),
                bus()
            )
            .unwrap()
            .is_none()
        );
        // 空段（`smtp: {}`，零 profile）= 未配置。
        let empty = Config {
            smtp: Some(Default::default()),
            ..Default::default()
        };
        assert!(
            build_mail_backend(&empty, &serde_json::Value::Null, Some(&FAKE_MAIL), bus())
                .unwrap()
                .is_none()
        );
        // 配了段但 oj-mail 未加载（`Registrations.mail = None`）。
        assert!(
            build_mail_backend(&smtp_cfg(), &serde_json::Value::Null, None, bus())
                .unwrap()
                .is_none()
        );
    }

    /// Given: 配置写错（`allowed_from` 是数字数组，经 `plugins.mail` 透传进来）；
    /// When: 装配；Then: **装配期** Err —— 不静默变成「无白名单 → 全部拒绝」。
    /// （透传与 `smtp:` 适配器走同一份 JSON，故两条路的校验面同源。）
    #[tokio::test(flavor = "current_thread")]
    async fn given_malformed_whitelist_when_assemble_then_err() {
        let mut cfg = Config::default();
        cfg.plugins.insert(
            "mail".into(),
            serde_json::json!({
                "default": {
                    "host": "h", "port": 25, "tls": "none", "allow_none_tls": true,
                    "mechanism": "login", "allowed_from": [1, 2],
                }
            }),
        );
        let e = match build_mail_backend(
            &cfg,
            &serde_json::Value::Null,
            Some(&FAKE_MAIL),
            Arc::new(only_js::bridge::Bus::new()),
        ) {
            Ok(_) => panic!("allowed_from 写错必须在装配期报错"),
            Err(e) => e,
        };
        assert!(e.contains("allowed_from"), "{e}");
    }

    // ---- ldap 装配测试 ----

    /// 假 ldap vtable：call 回固定结果（装配测试零网络、零插件）。
    extern "C" fn fake_ldap_call(req: RString) -> FfiFuture {
        let r = req[..].to_string();
        let out = if r.contains("\"op\":\"bind\"") {
            "true"
        } else {
            "\"dn:fake\""
        };
        oj_plugin_ffi::ready_ok(out.as_bytes().to_vec())
    }
    static FAKE_LDAP: oj_plugin_ffi::LdapVtable = oj_plugin_ffi::LdapVtable {
        call: fake_ldap_call,
    };

    /// 顶层 `ldap:` 段（不透明 yaml map）：双实例（default 服务账号 / ad ldaps）。
    fn ldap_cfg() -> Config {
        Config {
            ldap: Some(serde_yaml::from_str(
                "default:\n  url: ldap://dc.example.com:389\n  bind_dn: cn=svc,dc=example,dc=com\n  bind_pw: s\n  timeout_ms: 3000\nad:\n  url: ldaps://ad.internal:636\n",
            )
            .unwrap()),
            ..Config::default()
        }
    }

    /// Given: 未配 `ldap:`（或空段）／配了但插件未加载；
    /// Then: 不挂后端（None）——`ldap.*` 报 "ldap not configured"（与 mail 同语义）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_no_ldap_or_no_plugin_when_assemble_then_no_backend() {
        assert!(
            build_ldap_backend(
                &Config::default(),
                &serde_json::Value::Null,
                Some(&FAKE_LDAP)
            )
            .unwrap()
            .is_none()
        );
        let empty = Config {
            ldap: Some(serde_yaml::Value::Mapping(Default::default())),
            ..Default::default()
        };
        assert!(
            build_ldap_backend(&empty, &serde_json::Value::Null, Some(&FAKE_LDAP))
                .unwrap()
                .is_none()
        );
        assert!(
            build_ldap_backend(&ldap_cfg(), &serde_json::Value::Null, None)
                .unwrap()
                .is_none()
        );
    }

    /// Given: 合法 `ldap:` 段 + 插件在册；Then: 后端就位，实例表与段一致，
    /// 且调用确实经 vtable 过线（bind → true）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_ldap_section_when_assemble_then_backend_serves_calls() {
        let b = build_ldap_backend(&ldap_cfg(), &serde_json::Value::Null, Some(&FAKE_LDAP))
            .unwrap()
            .expect("ldap 段 + 插件在册 → 后端就位");
        assert_eq!(
            b.config().instance_keys(),
            vec!["ad".to_string(), "default".to_string()]
        );
        let v = b
            .call(serde_json::json!({"op":"bind","key":"default","dn":"u","pw":"p"}))
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(true));
    }

    /// Given: 配置写错（未知键/坏 url，经 `plugins.ldap` 透传进来）或双配置源皆非空；
    /// Then: 装配期 Err —— 不静默忽略拼错的配置，不在透传/适配器间静默遮蔽。
    #[tokio::test(flavor = "current_thread")]
    async fn given_bad_ldap_cfg_when_assemble_then_err() {
        let mut cfg = Config::default();
        cfg.plugins.insert(
            "ldap".into(),
            serde_json::json!({"default": {"url": "http://x"}}),
        );
        assert!(build_ldap_backend(&cfg, &serde_json::Value::Null, Some(&FAKE_LDAP)).is_err());
        // 双配置源：`ldap:` 段与 `plugins.ldap` 皆非空 → Err（二选一）。
        let mut both = ldap_cfg();
        both.plugins.insert(
            "ldap".into(),
            serde_json::json!({"other": {"url": "ldap://x:389"}}),
        );
        let e = match build_ldap_backend(&both, &serde_json::Value::Null, Some(&FAKE_LDAP)) {
            Ok(_) => panic!("双配置源必须在装配期报错"),
            Err(e) => e,
        };
        assert!(e.contains("pick one"), "{e}");
    }

    /// A4：停机排空 —— `drain_mail_backend` 必须把**控制报文**发给插件
    /// （`{"__ctl":"drain","timeout_ms":N}`），并把插件的排空信封解回。
    #[tokio::test(flavor = "current_thread")]
    async fn given_mail_backend_when_drain_then_control_message_sent_and_envelope_returned() {
        let bus: Arc<dyn EventBroker> = Arc::new(only_js::bridge::Bus::new());
        let mb = build_mail_backend(&smtp_cfg(), &serde_json::Value::Null, Some(&FAKE_MAIL), bus)
            .unwrap()
            .expect("smtp 段 + 插件在册 → 后端就位");
        SEEN_REQS.lock().unwrap().clear();

        let v = drain_mail_backend(&mb, Duration::from_millis(1234));
        assert_eq!(v["code"], 0, "{v}");
        assert_eq!(v["data"]["drained"], true, "{v}");
        assert_eq!(v["data"]["workers"], 2, "{v}");

        let reqs = SEEN_REQS.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "停机排空只发一次控制报文");
        let req: serde_json::Value = serde_json::from_str(&reqs[0]).unwrap();
        assert_eq!(req["__ctl"], "drain", "{req}");
        assert_eq!(req["timeout_ms"], 1234, "超时必须过线（插件据此等）：{req}");
    }

    /// A4：坏插件（控制报文回 pending）→ 排空失败收敛为 `code:1` 信封（停机路径只告警，
    /// **不 panic、不挂死**、不阻断进程退出）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_broken_plugin_when_drain_then_failure_becomes_envelope() {
        let bus: Arc<dyn EventBroker> = Arc::new(only_js::bridge::Bus::new());
        let mb = build_mail_backend(
            &smtp_cfg(),
            &serde_json::Value::Null,
            Some(&PENDING_MAIL),
            bus,
        )
        .unwrap()
        .expect("后端就位");
        let v = drain_mail_backend(&mb, Duration::from_millis(10));
        assert_eq!(v["code"], 1, "{v}");
        assert_eq!(v["data"]["drained"], false, "{v}");
        assert!(
            v["msg"].as_str().unwrap().contains("drain 调用失败"),
            "文案须点明排空失败：{v}"
        );
    }
}

