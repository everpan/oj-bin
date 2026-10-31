use super::*;

fn risk_of(sql: &str) -> Risk {
    classify(sql).risk
}

fn dang(sql: &str) -> bool {
    classify(sql).dangerous
}

#[test]
fn classification_matrix() {
    assert_eq!(risk_of("select * from t where id = ?"), Risk::ReadOnly);
    assert_eq!(risk_of("insert into t (id) values (1)"), Risk::Write);
    assert_eq!(risk_of("update t set a = 1 where id = 1"), Risk::Write);
    assert_eq!(risk_of("delete from t where id = 1"), Risk::Write);
    assert_eq!(risk_of("truncate table t"), Risk::Ddl);
    assert_eq!(risk_of("alter table t add column x integer"), Risk::Ddl);
    assert_eq!(risk_of("begin"), Risk::Transaction);
    assert_eq!(risk_of("commit"), Risk::Transaction);
    // 方言特例
    assert_eq!(risk_of("replace into t (id) values (1)"), Risk::Write);
    assert_eq!(
        risk_of("insert into t (id) values (1) on conflict (id) do update set a = 1"),
        Risk::Write
    );
    assert_eq!(risk_of("select * from t returning id"), Risk::ReadOnly);
    // 只读型诊断语句
    assert_eq!(risk_of("show tables"), Risk::ReadOnly);
    assert_eq!(risk_of("explain select 1"), Risk::ReadOnly);
    assert_eq!(risk_of("pragma table_info(t)"), Risk::ReadOnly);
    // MySQL KILL（REQ-10 一期）：会话/语句取消是写语义，但无 WHERE 面 → 非 dangerous。
    assert_eq!(risk_of("kill 42"), Risk::Write);
    assert_eq!(risk_of("KILL QUERY 42"), Risk::Write);
    assert_eq!(risk_of("kIlL 1"), Risk::Write);
}

/// 对抗性探测：每条都是「试图把写/DDL 伪装成只读」的击穿尝试。
#[test]
fn adversarial_samples_never_degrade_to_readonly() {
    // 1) 反斜杠吞句（PG/SQLite 标准串下是真两语句）→ 不判 ReadOnly，交 Unknown。
    assert_eq!(
        risk_of("SELECT 'a\\'; DROP TABLE users"),
        Risk::Unknown,
        "串未闭合必须 fail-closed，不得判只读"
    );
    // 2) 数据修改型 CTE
    assert_eq!(
        risk_of("WITH d AS (DELETE FROM orders RETURNING id) SELECT count(*) FROM d"),
        Risk::Write
    );
    // 3) MySQL 可执行注释（内容保留 → 认出 DROP）
    assert_eq!(risk_of("/*!40001 DROP TABLE t */"), Risk::Ddl);
    // 4) 注释断词（不得判只读）
    assert_eq!(risk_of("DR/**/OP TABLE t"), Risk::Unknown);
    assert_eq!(risk_of("DEL/**/ETE FROM t"), Risk::Unknown);
    // 5) MySQL `#` 行注释
    assert_eq!(risk_of("select 1 # drop table t\n"), Risk::ReadOnly);
    assert_eq!(risk_of("# drop table t\ndelete from t"), Risk::Write);
    // 6) 副作用 SELECT / 落文件 / 全局变量
    assert_eq!(risk_of("select 1 into outfile '/tmp/x'"), Risk::Write);
    assert_eq!(risk_of("select nextval('s')"), Risk::Write);
    assert_eq!(risk_of("select * from t for update"), Risk::Write);
    assert_eq!(risk_of("select * from t lock in share mode"), Risk::Write);
    assert_eq!(risk_of("copy t to '/tmp/x'"), Risk::Write);
    assert_eq!(risk_of("set global max_connections = 1"), Risk::Write);
    assert_eq!(risk_of("set names utf8"), Risk::Unknown, "裸 SET 不猜");
    // 7) 多语句取最严（EXPLAIN 前缀同理：classify 看整句，第二段抬级生效）
    assert_eq!(risk_of("select 1; drop table t"), Risk::Ddl);
    assert_eq!(risk_of("explain select 1; drop table t"), Risk::Ddl);
    // 8) 引用标识符里的表名不影响分类
    assert_eq!(risk_of("update `t` set a = 1 where id = 1"), Risk::Write);
    assert_eq!(risk_of(r#"update "t" set a = 1 where id = 1"#), Risk::Write);
    // 9) 可执行注释不影响租户守卫共享函数（回归锚：strip_literals 未被改动）
    assert_eq!(risk_of("select * from t"), Risk::ReadOnly);
}

#[test]
fn dangerous_write_rules() {
    // 主判据 = 有无 WHERE。
    assert!(dang("delete from t"));
    assert!(dang("update t set a = 1"));
    assert!(!dang("delete from t where id = 1"));
    assert!(!dang("update t set a = 1 where id = 1"));
    // 恒真谓词（收紧）
    assert!(dang("delete from t where 1 = 1"));
    assert!(dang("update t set a = 1 where a = a"));
    assert!(dang("delete from t where id is null or id is not null"));
    // 多表
    assert!(dang("update a, b set a.x = 1 where a.id = b.id"));
    assert!(dang("delete a from a join b on a.id = b.id where a.id = 1"));
    // insert 不算危险写（无法毁数据）
    assert!(!dang("insert into t (id) values (1)"));
    // DDL 恒危险
    assert!(dang("truncate table t"));
    assert!(dang("create table t (id integer)"));
}

#[test]
fn grant_is_union_of_declared_dimensions() {
    let p = |m: Option<&[&str]>, r: Option<&[&str]>| DbAccess {
        allow: AllowList {
            modules: m.map(|v| v.iter().map(|s| s.to_string()).collect()),
            roles: r.map(|v| v.iter().map(|s| s.to_string()).collect()),
            tenants: None,
            sources: None,
        },
        ..Default::default()
    };
    fn ctx<'a>(module: Option<&'a str>, roles: &[&str]) -> AccessCtx<'a> {
        AccessCtx {
            module,
            roles: roles.iter().map(|s| s.to_string()).collect(),
            tenant: None,
            source: "http",
        }
    }
    // 未声明任何维度 = 不限制
    assert!(granted(&p(None, None).allow, &ctx(None, &[])));
    // 声明 modules：命中才过
    let a = p(Some(&["sales"]), None);
    assert!(granted(&a.allow, &ctx(Some("sales"), &[])));
    assert!(!granted(&a.allow, &ctx(Some("ops"), &[])));
    assert!(!granted(&a.allow, &ctx(None, &[])), "无身份不得放行");
    // 并集：roles 命中即可（声明多维不提高强度）
    let b = p(Some(&["sales"]), Some(&["admin"]));
    assert!(granted(&b.allow, &ctx(Some("ops"), &["admin"])));
    assert!(!granted(&b.allow, &ctx(Some("ops"), &["user"])));
    // 声明空数组 = 无人可过（fail-closed，不是「不限制」）
    let c = p(Some(&[]), None);
    assert!(!granted(&c.allow, &ctx(Some("sales"), &[])));
}

#[test]
fn evaluate_policy_bits() {
    let ctx = AccessCtx {
        module: None,
        roles: vec![],
        tenant: None,
        source: "http",
    };
    let base = DbAccess::default();
    assert_eq!(evaluate(&base, &ctx, &classify("select 1")), Verdict::Allow);
    // read_only 拦 Write 与 Ddl
    let ro = DbAccess {
        read_only: true,
        ..Default::default()
    };
    assert_eq!(
        evaluate(&ro, &ctx, &classify("update t set a=1 where id=1")),
        Verdict::Violation(Reason::ReadOnly)
    );
    assert_eq!(
        evaluate(&ro, &ctx, &classify("drop table t")),
        Verdict::Violation(Reason::ReadOnly)
    );
    // KILL 升 Write 后：普通别名放行（旁路取消配方），read_only 仍拒。
    assert_eq!(evaluate(&base, &ctx, &classify("kill 42")), Verdict::Allow);
    assert_eq!(
        evaluate(&ro, &ctx, &classify("kill query 42")),
        Verdict::Violation(Reason::ReadOnly)
    );
    // DDL 门
    let noddl = DbAccess::default();
    assert_eq!(
        evaluate(&noddl, &ctx, &classify("truncate table t")),
        Verdict::Violation(Reason::DdlDenied)
    );
    // 危险写门
    assert_eq!(
        evaluate(&noddl, &ctx, &classify("delete from t")),
        Verdict::Violation(Reason::DangerousWrite)
    );
    // Unknown → fail-closed
    assert_eq!(
        evaluate(&noddl, &ctx, &classify("select 'a\\'; drop table t")),
        Verdict::Violation(Reason::UnknownRisk)
    );
    // 授权优先于策略位
    let locked = DbAccess {
        allow: AllowList {
            modules: Some(vec!["sales".into()]),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(
        evaluate(&locked, &ctx, &classify("select 1")),
        Verdict::Violation(Reason::NotGranted)
    );
}

#[test]
fn config_validation_is_loud() {
    assert!(
        DbAccess {
            max_rows: Some(0),
            ..Default::default()
        }
        .validate("x")
        .is_err()
    );
    assert!(
        DbAccess {
            max_rows: Some(MAX_ROWS_CAP + 1),
            ..Default::default()
        }
        .validate("x")
        .is_err()
    );
    assert!(
        DbAccess {
            read_only: true,
            allow_ddl: true,
            ..Default::default()
        }
        .validate("x")
        .is_err()
    );
    assert!(DbAccess::default().validate("x").is_ok());
}

#[test]
fn quota_config_parses_merges_and_validates() {
    // Given defaults 声明三键、某库只覆盖 on_exceed
    let s: DbAccessSection = serde_yaml::from_str(
            "defaults:\n  statement_timeout_ms: 30000\n  max_result_bytes: 65536\nrep:\n  on_exceed: truncate\n",
        )
        .unwrap();
    // When policy_for，Then 逐键继承：rep 拿到 defaults 的两键 + 自己的 on_exceed
    let p = s.policy_for("rep");
    assert_eq!(p.statement_timeout_ms, Some(30_000));
    assert_eq!(p.max_result_bytes, Some(65_536));
    assert_eq!(p.on_exceed, Some(OnExceed::Truncate));
    // 未覆盖库全取 defaults；on_exceed 缺省 None（fail 语义在计量层归一）
    let d = s.policy_for("other");
    assert_eq!(d.statement_timeout_ms, Some(30_000));
    assert_eq!(d.on_exceed, None);
    // 未知枚举 fail-loud
    let bad = serde_yaml::from_str::<DbAccessSection>("a:\n  on_exceed: chop\n");
    assert!(bad.is_err(), "{bad:?}");
    // 校验走 merge 后策略（装配期口径，app.rs 同款）：0 / 超界语句超时、0 字节上限 → 点名键
    for (yaml, key) in [
        ("a:\n  statement_timeout_ms: 0\n", "statement_timeout_ms"),
        (
            "a:\n  statement_timeout_ms: 3600001\n",
            "statement_timeout_ms",
        ),
        ("a:\n  max_result_bytes: 0\n", "max_result_bytes"),
    ] {
        let s: DbAccessSection = serde_yaml::from_str(yaml).unwrap();
        let e = s.by_db["a"]
            .merge(&s.defaults)
            .validate("a")
            .expect_err("must reject");
        assert!(e.contains(key), "{e} must name {key}");
    }
    // on_exceed 单配（无 max_result_bytes）无意义 → 拒（merge 后口径）
    let s: DbAccessSection = serde_yaml::from_str("a:\n  on_exceed: truncate\n").unwrap();
    assert!(s.by_db["a"].merge(&s.defaults).validate("a").is_err());
}

#[test]
fn section_parsing_and_lookup() {
    let s: DbAccessSection = serde_yaml::from_str(
        "defaults:\n  enforce: warn\nanalytics:\n  read_only: true\n  max_rows: 50\n",
    )
    .unwrap();
    assert_eq!(s.policy_for("whatever").enforce, Enforce::Warn);
    assert!(s.policy_for("analytics").read_only);
    assert_eq!(s.policy_for("analytics").max_rows, Some(50));
    // 逐键继承：只声明 read_only/max_rows，**不得**把 defaults 的 enforce 抹掉
    // （整条覆盖会让「全局设防 + 局部微调」静默退化成不设防）
    assert_eq!(s.policy_for("analytics").enforce, Enforce::Warn);
    // allow 逐维覆盖：defaults 的 roles 不能因某库只写 modules 而消失
    let s: DbAccessSection = serde_yaml::from_str(
        "defaults:\n  allow:\n    roles: [admin]\nanalytics:\n  allow:\n    modules: [sales]\n",
    )
    .unwrap();
    let a = s.policy_for("analytics").allow;
    assert_eq!(a.roles.as_deref(), Some(&["admin".to_string()][..]));
    assert_eq!(a.modules.as_deref(), Some(&["sales".to_string()][..]));
    // 未知键 fail-loud
    let bad = serde_yaml::from_str::<DbAccessSection>("a:\n  nope: 1\n");
    assert!(bad.is_err(), "{bad:?}");
}

#[test]
fn redact_masks_literals_and_credentials() {
    assert_eq!(
        redact_sql("select * from t where a = 'secret'"),
        "select * from t where a = ?"
    );
    assert!(!redact_sql("select * from t -- drop\n").contains("drop"));
    let s = redact_sql("mysql://u:pw@host/db?password=hunter2&x=1");
    assert!(s.contains("password=***"), "{s}");
    let s = redact_sql("grant all on *.* to u identified by 'x'; token=abc");
    assert!(s.contains("token=***"), "{s}");
    assert!(!redact_sql("select password_hash from t").contains("***"));
    let long = "a".repeat(1000);
    // 截断口径是 512 字符 + 1 个省略号（按字符计，非字节）
    assert_eq!(redact_sql(&long).chars().count(), 513);
}
