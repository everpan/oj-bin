# FFI 按轴 dlsym 重构实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 插件注册面从定长 `PluginRegistrations` 结构体改为按轴导出符号（`oj_plugin_axis_<name>`），加新轴时存量插件零改动、零重编译、ABI 不 bump；同批落地插件配置通用化（`plugins.<name>` 开放段 + 三级回落）与插件自描述查询（descriptor.desc + host 收集 + `GET {base}/plugins`）。

**Architecture:** 见 `docs/superpowers/specs/2026-09-05-ffi-axis-dlsym-design.md`。FFI 契约：`oj_plugin_abi_version` / `oj_plugin_init` 保留，`PluginDescriptor` 删 `register`，`PluginRegistrations` 整体删除；宿主 `load_one` 在 init 后对已知轴逐个 dlsym 探测。本次 ABI 6→7 是最后一次破坏性迁移。

**Tech Stack:** libloading / stabby / paste（宏内标识符拼接）/ serde_json。

## Global Constraints

- 只许 release 构建（`cargo build --release` / `cargo xtask build`）；测试内嵌 cargo 例外沿用现状（mini 夹具走 debug，见 `plugin_loader/tests.rs:15` 先例）。
- 插件 crate 保持 `panic = "unwind"`。
- `bootstrap.js` 不在本计划范围（不得触碰）。
- 门禁：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test --release --workspace`。
- ABI_VERSION 6→7 后必须 `cargo xtask build` 全量重建（bin/plugins 里 ABI 6 产物会让加载测试失败——Task 2-4 期间属预期中间态，门禁按 crate 范围收窄；Task 5 恢复全量）。
- cfg JSON 演化不改 ABI（既有规则）；插件 cfg schema 由插件在 init 校验 fail-fast。

## 关键背景（实现者必读）

- 符号拼接用 `paste` crate（dtolnay，零依赖）：`[<oj_plugin_axis_ $axis:lower>]` 生成小写符号。
- 宿主探测点：`src/bridge/plugin_loader.rs` 的 `load_one`（约 322 行）——现结构：dlsym `oj_plugin_abi_version` → ABI 门禁 → dlsym `oj_plugin_init` → 按 manifest 名/文件 stem 取 cfg → init → **读 `register()` 聚合（本计划删除此步）** → 构建 host 侧 `Registrations` 镜像。
- host 侧 `Registrations` 镜像结构（同文件，含 es/db/blob/bus/kv/auth 的 `Option<&'static XxxVtable>` 字段）**保留不动**，仅数据来源从 `register()` 返回值改为 dlsym 探测——下游 `es_backend`/`db_backend`/`auth_guard`/`kv_backend_connect`/`blob_backend_connect` 包装器（同文件 145-227 行）零改动。
- mini 夹具（`tests/plugins/mini`，包名 `oj-plugin-test-mini`）现为零轴插件（`PluginRegistrations::none()`），loader 测试（`src/bridge/plugin_loader/tests.rs`）首次使用时 `cargo build -p oj-plugin-test-mini` 按需编译——本计划把它正式化为「零轴」夹具，新增 `mini-kv`（单轴）夹具。「全轴」夹具不做：各轴探测相互独立，零轴 + 单轴已覆盖全部分支（有意简化）。
- 探测语义：符号缺失或返回 null = 不提供该轴（与旧 null 槽一致）；不产生 `SymbolMissing` 错误（该错误仅用于 abi_version/init 符号缺失）。
- 命名契约（spec「插件配置」节）：插件文件 stem == descriptor name == cfg 键；xtask 落盘已按 descriptor 命名。

---

### Task 1: oj-plugin-ffi 契约瘦身 + axes 宏（ABI 7）

**Files:**
- Modify: `oj-plugin-ffi/Cargo.toml`（[dependencies] 加 `paste = "1"`）
- Modify: `oj-plugin-ffi/src/lib.rs`

**Interfaces:**
- Produces（后续所有任务依赖）:
  - `ABI_VERSION: u32 = 7`
  - `PluginDescriptor { name, semver, abi_version, fingerprint, desc }`（无 register；`desc: RString` 为插件自描述，spec「插件自描述与查询」节）
  - `oj_plugin_entry!(init_expr)` 零轴 / `oj_plugin_entry!(init_expr, kv => &VT, auth => &VT2)` 多轴，生成 `oj_plugin_axis_<小写轴名>() -> *const c_void`

- [ ] **Step 1: lib.rs 契约改造**

删除 `pub mod` 之外的以下项：`PluginRegistrations` 结构体、其 `impl`（`none()` + 五个访问器）、`PluginDescriptor.register` 字段、文档中关于 register 的表述。`ABI_VERSION` 改 `7`，版本注释追加 `/// 7 = 按轴 dlsym（删 PluginRegistrations/register，加轴自此零破坏）。`

`PluginDescriptor` 增自描述字段（放在 `fingerprint` 之后，doc：`/// 人类可读描述（插件作者自述；host 收集并在 GET {base}/plugins 公开）。`）：

```rust
    /// 人类可读描述（插件作者自述；host 收集并在 GET {base}/plugins 公开）。
    pub desc: RString,
```

宏替换为（整体替换现 `oj_plugin_entry!`）：

```rust
/// 插件入口宏：生成 oj_plugin_abi_version / oj_plugin_init（catch_unwind 收敛）/
/// 每轴一个 `oj_plugin_axis_<name>` 导出符号（返回静态 vtable 指针，擦除为 *const c_void）。
/// 用法：
///   oj_plugin_entry!(init);                                          // 零轴
///   oj_plugin_entry!(init, kv => &KV_VTABLE);                        // 单轴
///   oj_plugin_entry!(init, kv => &KV_VTABLE, auth => &AUTH_VTABLE);  // 多轴
/// 轴标识写入符号前强制小写（宿主探测表全小写）；未提供的轴不导出符号 = 不提供该轴。
/// 注意：vtable 方法须在实现侧以 catch_value/catch_future 收敛 panic——宿主对
/// vtable 方法无 catch_unwind（本宏只保护 init）。
#[macro_export]
macro_rules! oj_plugin_entry {
    ($init:expr $(, $axis:ident => $vtable:expr)* $(,)?) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn oj_plugin_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn oj_plugin_init(
            host: $crate::RArc<$crate::HostContext>,
            cfg: $crate::RString,
        ) -> $crate::RResult<$crate::PluginDescriptor, $crate::RString> {
            let init: fn(
                $crate::RArc<$crate::HostContext>,
                $crate::RString,
            ) -> $crate::RResult<$crate::PluginDescriptor, $crate::RString> = $init;
            match ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| init(host, cfg))) {
                ::core::result::Result::Ok(r) => r,
                ::core::result::Result::Err(_) => {
                    $crate::RResult::Err($crate::RString::from("panic in plugin init"))
                }
            }
        }

        $(
            ::paste::paste! {
                #[unsafe(no_mangle)]
                pub extern "C" fn [<oj_plugin_axis_ $axis:lower>]() -> *const ::core::ffi::c_void {
                    $vtable as *const _ as *const ::core::ffi::c_void
                }
            }
        )*
    };
}
```

- [ ] **Step 2: 宏展开冒烟测试**（lib.rs `#[cfg(test)] mod tests` 追加）

```rust
    // 假 vtable：只需静态可寻址，不需要真实字段全贯通（贯通由 loader 测试覆盖）。
    #[repr(C)]
    struct FakeVt {
        _pad: u8,
    }
    static FAKE_A: FakeVt = FakeVt { _pad: 0 };
    static FAKE_B: FakeVt = FakeVt { _pad: 1 };

    #[cfg(test)]
    mod macro_smoke {
        // 用不与真实轴冲突的假轴名，避免测试二进制内符号相撞。
        oj_plugin_entry!(test_zero_init);
        oj_plugin_entry!(test_axes_init, fakea => &super::FAKE_A, fakeb => &super::FAKE_B);

        fn test_zero_init(
            _: $crate::RArc<$crate::HostContext>,
            _: $crate::RString,
        ) -> $crate::RResult<$crate::PluginDescriptor, $crate::RString> {
            unreachable!()
        }
        fn test_axes_init(
            _: $crate::RArc<$crate::HostContext>,
            _: $crate::RString,
        ) -> $crate::RResult<$crate::PluginDescriptor, $crate::RString> {
            unreachable!()
        }

        #[test]
        fn axis_symbols_export_vtable_pointers() {
            type Sym = unsafe extern "C" fn() -> *const std::ffi::c_void;
            let a: Sym = oj_plugin_axis_fakea;
            let b: Sym = oj_plugin_axis_fakeb;
            assert_eq!(a(), &super::FAKE_A as *const FakeVt as *const std::ffi::c_void);
            assert_eq!(b(), &super::FAKE_B as *const FakeVt as *const std::ffi::c_void);
        }
    }
```

注意：`$crate` 在同 crate 内测试也有效；若 `use super::*` 后路径报错，就地调整（用 `crate::` 全路径）。生成函数在测试模块内的 `#[unsafe(no_mangle)]` 若与宿主 crate 符号冲突（仅当轴名撞真名），假轴名已规避。

- [ ] **Step 3: 门禁（仅本 crate；workspace 此刻必然红——Task 2 起逐个恢复）**

Run: `cargo test --release -p oj-plugin-ffi && cargo fmt --check && cargo clippy -p oj-plugin-ffi --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add oj-plugin-ffi/
git commit -m "feat(ffi)!: 按轴 dlsym 契约——删 PluginRegistrations/register，axes 宏，ABI 7

unix@vip.qq.com ai"
```

---

### Task 2: 宿主探测 + mini 夹具迁移 + loader 测试

**Files:**
- Modify: `src/bridge/plugin_loader.rs`（`load_one` 探测化 + `AXES` 表）
- Modify: `tests/plugins/mini/src/lib.rs`（零轴形态）
- Create: `tests/plugins/mini-kv/Cargo.toml`、`tests/plugins/mini-kv/src/lib.rs`（单轴夹具）
- Modify: 根 `Cargo.toml`（members 加 `"tests/plugins/mini-kv"`）
- Modify: `src/bridge/plugin_loader/tests.rs`（探测用例）

**Interfaces:**
- Produces: `pub const AXES: &[&str] = &["es", "db", "blob", "bus", "kv", "auth"];`（plugin_loader.rs，pub 供 Task 5 子集断言）；`load_one` 内探测逻辑；mini-kv 夹具（descriptor name `"mini-kv"`，提供 kv 轴）。
- Consumes: Task 1 符号与 ABI 7。

- [ ] **Step 1: 写失败测试**（tests.rs 追加；`mini_plugin_dir` 逻辑复用，mini-kv 仿照编译）

```rust
// ---- 按轴探测（dlsym）----

/// mini-kv 编译产物目录（复用 mini_plugin_dir 的按需编译 + 幂等拷贝模式，
/// 包名/产物名替换为 oj-plugin-test-mini-kv / mini-kv）。
fn mini_kv_plugin_dir() -> PathBuf {
    // 实现照抄 mini_plugin_dir，替换三处：包名 "oj-plugin-test-mini-kv"、
    // 产物 "liboj_plugin_test_mini_kv.<ext>"、文件名 ffi::plugin_file_name("mini-kv")。
    # let () = (); // 占位防呆：照抄时删除
    todo!()
}

#[test]
fn probe_finds_declared_axis_and_misses_undeclared() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe { std::env::remove_var("MINI_FAKE_ABI") };
    unsafe { std::env::remove_var("MINI_PANIC") };
    // mini（零轴）：加载成功但所有轴 None。
    let mini = super::load_one(&mini_plugin_dir().join(ffi::plugin_file_name("mini")), None, host_context(), &no_cfg).unwrap();
    assert!(mini.registrations.kv.is_none());
    // mini-kv（单轴）：kv 有、auth 无。
    let mkv = super::load_one(&mini_kv_plugin_dir().join(ffi::plugin_file_name("mini-kv")), None, host_context(), &no_cfg).unwrap();
    assert!(mkv.registrations.kv.is_some());
    assert!(mkv.registrations.auth.is_none());
}
```

（若 `load_one`/`host_context`/`no_cfg` 的可见性或签名与上述调用不符，以文件现状为准调整——它们已在同文件既有测试中使用。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js probe_finds 2>&1 | tail -5`
Expected: FAIL（探测未实现 / mini-kv 不存在）

- [ ] **Step 3: 实现探测**

`src/bridge/plugin_loader.rs`：

```rust
/// 宿主认识的轴（加新轴 = 此表加一行 + 对应 vtable 类型 + Registrations 加字段；
/// 插件零感知、零重编译——spec「按轴 dlsym」）。
pub const AXES: &[&str] = &["es", "db", "blob", "bus", "kv", "auth"];

/// init 成功后逐轴 dlsym：`oj_plugin_axis_<name>() -> *const c_void`。
/// 缺符号或返回 null = 不提供该轴（非错误）。
unsafe fn probe_axes(lib: &libloading::Library) -> Registrations {
    let mut r = Registrations::default();
    for axis in AXES {
        let sym = format!("oj_plugin_axis_{axis}");
        let Ok(f) = lib.get::<unsafe extern "C" fn() -> *const std::ffi::c_void>(sym.as_bytes())
        else {
            continue;
        };
        let vt = f();
        if vt.is_null() {
            continue;
        }
        match *axis {
            "es" => r.es = Some(unsafe { &*(vt as *const EsBackendVtable) }),
            "db" => r.db = Some(unsafe { &*(vt as *const DataAccessorVtable) }),
            "blob" => r.blob = Some(unsafe { &*(vt as *const BlobBackendVtable) }),
            "bus" => r.bus = Some(unsafe { &*(vt as *const EventBrokerVtable) }),
            "kv" => r.kv = Some(unsafe { &*(vt as *const KVStoreVtable) }),
            "auth" => r.auth = Some(unsafe { &*(vt as *const AuthGuardVtable) }),
            _ => unreachable!("AXES 与 probe_axes 分支不同步"),
        }
    }
    r
}
```

`load_one` 中删除「调 `descriptor.register` 构建 Registrations」段，替换为 `let registrations = unsafe { probe_axes(&lib) };`（`lib` 的所有权/drop 时机按现文件 `load_forget` 的句柄泄漏语义处理——句柄进程级存活，vtable 指针永久有效）。`Registrations` 镜像结构与其 Default 保持不变（auth 字段已由 auth 计划加入）。vtable 类型 `use` 以文件顶部现状为准。

- [ ] **Step 4: mini 迁移 + mini-kv 新建**

`tests/plugins/mini/src/lib.rs`：删 `no_registrations` 与 `PluginRegistrations` 导入；descriptor 字面量删 `register:` 行、加 `desc: RString::from("loader 测试夹具（零轴）"),`；宏调用改 `oj_plugin_entry!(init);`（零轴，env 钩子 MINI_FAKE_ABI/MINI_PANIC 语义不变）。

`tests/plugins/mini-kv/Cargo.toml`（仿 mini，包名 `oj-plugin-test-mini-kv`）：

```toml
[package]
name = "oj-plugin-test-mini-kv"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
oj-plugin-ffi = { path = "../../../oj-plugin-ffi" }
```

`tests/plugins/mini-kv/src/lib.rs`：

```rust
//! 单轴测试夹具：只提供 kv 轴（假 vtable，方法一概 Err）——探测「有轴/无轴」的正例。
//! 真实 kv 实现见 plugins/oj-kv-redis；本夹具只服务 plugin_loader 探测测试。

use oj_plugin_ffi::{HostContext, KVStoreVtable, RArc, RResult, RString, oj_plugin_entry};

extern "C" fn connect(_cfg: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn get(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn set(_handle: u64, _key: RString, _value: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn del(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn expire(_handle: u64, _key: RString, _ttl: u64) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn incr(_handle: u64, _key: RString) -> oj_plugin_ffi::FfiFuture {
    oj_plugin_ffi::ready_err("mini-kv: not a real kv")
}
extern "C" fn close(_handle: u64) {}

static KV: KVStoreVtable = KVStoreVtable { connect, get, set, del, expire, incr, close };

fn init(_host: RArc<HostContext>, _cfg: RString) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("mini-kv"),
        semver: RString::from("0.1.0"),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("loader 测试夹具（单轴 kv）"),
    })
}

oj_plugin_entry!(init, kv => &KV);
```

注意：`FfiFuture`/`ready_err` 的真实签名以 `oj-plugin-ffi/src/future.rs` 为准调整（构造假 vtable 只求编译，不求可调用）；`KVStoreVtable` 字段名以 `oj-plugin-ffi/src/kv.rs` 为准（connect/get/set/del/expire/incr/close）。

根 `Cargo.toml` members 加 `"tests/plugins/mini-kv"`。

- [ ] **Step 5: 测试 + 门禁（only-js 与两个夹具）**

Run: `cargo test --release -p only-js plugin_loader 2>&1 | tail -3 && cargo fmt --check && cargo clippy -p only-js --all-targets -- -D warnings`
Expected: PASS（含新探测用例；bin/plugins 里 ABI 6 存量产物导致的加载类失败不在本任务范围）

- [ ] **Step 6: Commit**

```bash
git add src/bridge/plugin_loader.rs src/bridge/plugin_loader/tests.rs tests/plugins/ Cargo.toml Cargo.lock
git commit -m "feat(loader): 按轴 dlsym 探测（AXES 表）+ mini 零轴/mini-kv 单轴夹具

unix@vip.qq.com ai"
```

---

### Task 3: 8 个存量插件迁移 axes 形态

**Files:** 逐插件只改 `src/lib.rs`（+可能的 Cargo.toml 无改动）：
- `plugins/oj-es`（es 轴）、`plugins/oj-db-mysql` / `plugins/oj-db-postgres`（db 轴）、`plugins/oj-blob-s3`（blob 轴）、`plugins/oj-bus-kafka` / `plugins/oj-bus-rabbitmq`（bus 轴）、`plugins/oj-kv-redis`（kv 轴）、`plugins/oj-auth`（auth 轴）

**Interfaces:**
- Consumes: Task 1 宏。
- 语义零变化：同一 vtable、同一 cfg 契约、同一 descriptor；只换注册通道。

- [ ] **Step 1: 全局搜索残留**

Run: `grep -rn "PluginRegistrations\|register:" plugins/ --include="*.rs"`
Expected: 每插件命中 register 闭包/字面量若干处——全部为本任务清除对象。

- [ ] **Step 2: 逐插件迁移（模式统一）**

每插件四步：
1. 导入列表删 `PluginRegistrations`（保留其余）。
2. 删 `register` 闭包/函数与 descriptor 字面量中的 `register:` 行。
3. descriptor 加 `desc:` 行——取各插件 `Cargo.toml` 的 `description` 文案（如 oj-auth：`desc: RString::from("auth 轴守卫插件：JWT 验签 + 匿名路径匹配"),`）。
4. 宏调用改 axes 形态（vtable 静态名以各插件现状为准）：

```rust
// oj-es        → oj_plugin_entry!(init, es => &ES_VTABLE);
// oj-db-mysql  → oj_plugin_entry!(init, db => &MYSQL_VTABLE);
// oj-db-postgres → oj_plugin_entry!(init, db => &PG_VTABLE);
// oj-blob-s3   → oj_plugin_entry!(init, blob => &S3_VTABLE);
// oj-bus-kafka → oj_plugin_entry!(init, bus => &KAFKA_VTABLE);
// oj-bus-rabbitmq → oj_plugin_entry!(init, bus => &RABBITMQ_VTABLE);
// oj-kv-redis  → oj_plugin_entry!(init, kv => &KV_VTABLE);
// oj-auth      → oj_plugin_entry!(init, auth => &AUTH_VTABLE);
```

- [ ] **Step 3: 全 workspace 编译 + 插件测试**

Run: `cargo build --release --workspace && cargo test --release -p oj-auth -p oj-kv-redis 2>&1 | tail -3`
Expected: PASS（有单测的插件保持绿；此时 bin/plugins 仍是 ABI 6 旧产物，加载类集成测试的失败留待 Task 5 重建后消除——已知中间态）

- [ ] **Step 4: Commit**

```bash
git add plugins/
git commit -m "refactor(plugins)!: 8 个第一方插件迁移 axes 注册形态（语义零变化）

unix@vip.qq.com ai"
```

---

### Task 4: 插件配置通用化（plugins 开放段 + 三级回落）

**Files:**
- Modify: `src/config.rs`（`Config.plugins` 开放段）
- Modify: `oj/src/server_cmd.rs`（`plugin_cfg_json` → `plugin_cfg` 三级回落 + 子集断言测试）

**Interfaces:**
- Produces: `Config.plugins: HashMap<String, serde_json::Value>`（serde default）；`fn plugin_cfg(cfg: &Config, name: &str) -> String`（语义：`plugins.<name>` 原样透传 → 已知轴适配器 → `{}`）。
- Consumes: `plugin_loader::AXES`（Task 2 pub）。

- [ ] **Step 1: 写失败测试**（server_cmd.rs tests 追加）

```rust
    /// cfg 三级回落（spec「插件配置」）：开放段透传 → 轴适配器 → {}。
    #[test]
    fn plugin_cfg_fallback_chain() {
        let mut cfg = Config::default();
        cfg.auth = Some(serde_yaml::from_str("jwt_secret: \"s\"\n").unwrap());
        // 1) 开放段优先且原样透传（宿主不改写字段）
        cfg.plugins.insert(
            "auth".into(),
            serde_json::json!({ "jwt_secret": "override", "extra_field": 42 }),
        );
        assert_eq!(
            plugin_cfg(&cfg, "auth"),
            r#"{"extra_field":42,"jwt_secret":"override"}"#
        );
        // 2) 无开放段 → 轴适配器（auth 分支既有行为不变）
        let mut cfg2 = Config::default();
        cfg2.auth = Some(serde_yaml::from_str("jwt_secret: \"s\"\n").unwrap());
        let v: serde_json::Value = serde_json::from_str(&plugin_cfg(&cfg2, "auth")).unwrap();
        assert_eq!(v["jwt_secret"], "s");
        // 3) 未知插件 → {}
        assert_eq!(plugin_cfg(&cfg2, "vendor-thing"), "{}");
    }

    /// 适配器轴必须是宿主探测轴的子集（两表失步 = 适配器永远打空）。
    #[test]
    fn cfg_adapters_subset_of_probed_axes() {
        for name in ADAPTER_AXES {
            assert!(only_js::bridge::plugin_loader::AXES.contains(name), "{name}");
        }
    }
```

（断言串按 serde_json 键序可能带空格差异——若 `to_string()` 键序不定，改为解析回 `Value` 后逐字段断言。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj plugin_cfg 2>&1 | tail -5`
Expected: FAIL（`Config.plugins` / `plugin_cfg` / `ADAPTER_AXES` 不存在）

- [ ] **Step 3: 实现**

`src/config.rs`（`Config` 已 `#[serde(default)]`，直接加字段）：

```rust
    /// 插件自有配置（开放式；键 = descriptor name == 插件文件 stem）。
    /// 值 = 任意 JSON，宿主原样透传给插件 init，不做字段解释（spec「插件配置」）。
    pub plugins: HashMap<String, serde_json::Value>,
```

`oj/src/server_cmd.rs`：

```rust
/// 第一方轴适配器（cfg 顶层段 → 插件 cfg JSON）。加新第一方轴且需要读顶层段时
/// 在此加分支，并保持 keys ⊆ plugin_loader::AXES（下方测试锁死）。
const ADAPTER_AXES: &[&str] = &["es", "auth"];

/// cfg 三级回落：plugins.<name> 原样透传 → 轴适配器 → "{}"。
/// schema 归插件所有：插件在 init 校验，非法即 Err fail-fast（宿主不解释字段）。
fn plugin_cfg(cfg: &Config, name: &str) -> String {
    if let Some(v) = cfg.plugins.get(name) {
        return v.to_string();
    }
    match name {
        "es" => match &cfg.es {
            Some(es) => serde_json::json!({ "endpoint": es.endpoint }).to_string(),
            None => "{}".to_string(),
        },
        "auth" => match &cfg.auth {
            Some(a) => serde_json::json!({
                "jwt_secret": a.jwt_secret,
                "signing_method": a.signing_method,
                "anonymous_paths": a.anonymous_paths,
            })
            .to_string(),
            None => "{}".to_string(),
        },
        _ => "{}".to_string(),
    }
}
```

原 `plugin_cfg_json` 的全部调用点改为 `plugin_cfg`（grep 确认无残留）。注意保持 Task 6（auth 计划）既有行为：`scan` 模式 cfg_for 按 stem 命中、auth 未声明时传 `"{}"`（oj-auth 侧 jwt_secret `#[serde(default)]` 承接）。

- [ ] **Step 4: 测试 + 门禁（oj + only-js）**

Run: `cargo test --release -p oj plugin_cfg -p oj 2>&1 | tail -3 && cargo test --release -p oj cfg_adapters && cargo fmt --check && cargo clippy -p oj -p only-js --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/config.rs oj/src/server_cmd.rs
git commit -m "feat(config): 插件自有配置 plugins.<name> 开放段 + cfg 三级回落

unix@vip.qq.com ai"
```

---

### Task 5: 插件自描述收集 + `GET {base}/plugins` 查询端点

**Files:**
- Modify: `src/bridge/plugin_loader.rs`（`PluginInfo` 增 `description`）
- Modify: `oj/src/server_cmd.rs`（装配聚合 `Vec<PluginInfo>` 并传递）
- Modify: `oj/src/app.rs`（Extras.plugins 接真值 + AppState 传参）
- Modify: `server/src/lib.rs`（AppState 字段 + `{base}/plugins` route + 测试）

**Interfaces:**
- Produces:
  - `PluginInfo { name, semver, abi_version, fingerprint, description }`（`From<&LoadedPlugin>` 同步）
  - `GET {base}/plugins` → ok 信封 `{code:0, data:[{name, version, description, abi_version, fingerprint}, ...]}`；公共基础设施端点（与 `/health` 同位：无 Bearer、不受证书 GET 限制、GET only、保留路径遮蔽同名业务路由）
- Consumes: Task 3 的 `desc`。

- [ ] **Step 1: 写失败测试**（server/src/lib.rs tests 追加；仿 `health_endpoint_reports_cert_status` 的直接 handler 调用形态）

```rust
    /// GET {base}/plugins：公共查询端点——返回装配插件的自描述清单（ok 信封）。
    #[tokio::test]
    async fn plugins_endpoint_lists_self_descriptions() {
        let st = dummy_app_state();
        // 注入两条自描述（ AppState.plugins 为 Arc<Vec<PluginInfo>>）
        st.plugins = Arc::new(vec![
            only_js::bridge::PluginInfo {
                name: "auth".into(),
                semver: "0.1.0".into(),
                abi_version: 7,
                fingerprint: "fp".into(),
                description: "auth guard".into(),
            },
        ]);
        // handler 以 route 形态挂在 base 下，直接调（含 base 前缀拼接的路径校验在集成层）。
        let resp = crate::plugins_handler(axum::extract::State(st)).await;
        let body = body_text(resp).await;
        assert!(body.contains("\"name\":\"auth\""), "{body}");
        assert!(body.contains("\"description\":\"auth guard\""), "{body}");
        assert!(body.contains("\"abi_version\":7"), "{body}");
    }
```

（`PluginInfo` 真实字段名以 `src/bridge/plugin_loader.rs` 现状为准——若已有字段叫 `version` 而非 `semver`，测试随实现走；`plugins_handler` 签名照 `health_handler` 的 `State(AppState)` 形态。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p server plugins_endpoint 2>&1 | tail -5`
Expected: FAIL（`plugins_handler` / `AppState.plugins` 不存在）

- [ ] **Step 3: 实现**

1. `src/bridge/plugin_loader.rs`：`PluginInfo` 加 `pub description: String,`；`From<&LoadedPlugin>` 补 `description: p.descriptor.desc[..].to_string()`（`LoadedPlugin` 持 descriptor；字段取值路径以现状为准）。
2. `oj/src/server_cmd.rs`：`assemble_plugins` 已返回 loaded 列表——在 `App::from_config` 的调用侧聚合 `let plugin_infos: Vec<only_js::bridge::PluginInfo> = loaded.iter().map(PluginInfo::from).collect();`（若 assemble 吞掉所有权则改返回 `(infos, registries)` 或透传 loaded，取最小 diff）。两条消费：`Extras.plugins`（make_bridge 闭包里替换 `Vec::new()`，Arc 共享）与新 AppState 字段。
3. `server/src/lib.rs`：
   - `AppState` 加 `pub plugins: std::sync::Arc<Vec<only_js::bridge::PluginInfo>>,`；`app()` 加参数并注入（`dummy_app_state`/`serve_with_listener` 等构造点补 `Arc::default()`；测试用注入真值）。
   - 路由（紧邻 `health_path` 注册，同为先于 fallback 的真实 route——不走 Bearer/证书门禁，与 `/health` 同位的公共端点）：

```rust
    let plugins_path = format!("{}/plugins", base.trim_end_matches('/'));
    // ... Router::new()
        .route(&plugins_path, axum::routing::get(plugins_handler))
```

```rust
/// 插件清单查询（公共基础设施端点，与 /health 同位：监控/运维可匿名访问）。
/// 返回装配插件的自描述（name/version/description/abi/fingerprint）。
async fn plugins_handler(State(st): State<AppState>) -> Response {
    let data = serde_json::to_value(&*st.plugins).unwrap_or_else(|_| serde_json::Value::Null);
    let mut r = Response::new(axum::body::Body::from(only_js::bridge::ok(&data)));
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    r
}
```

4. `oj/src/app.rs`：`app(...)` 调用点传 `plugin_infos`。

- [ ] **Step 4: 测试 + 门禁**

Run: `cargo test --release -p server plugins_endpoint && cargo test --release -p only-js plugin_loader && cargo fmt --check && cargo clippy -p server -p oj -p only-js --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/bridge/plugin_loader.rs oj/src/server_cmd.rs oj/src/app.rs server/src/lib.rs
git commit -m "feat(plugins): 自描述收集（PluginInfo.description）+ GET {base}/plugins 查询端点

unix@vip.qq.com ai"
```

---

### Task 6: xtask 预检 + docs/CI + 全量门禁

**Files:**
- Modify: `tools/xtask/src/main.rs`（`--check` 预检走符号探测，打印 desc）
- Modify: `docs/plugin-architecture.md`、`docs/plugin-development.md`、`docs/builtin-api-auth.md`（新端点）、`CLAUDE.md`（本地，gitignore，同步 ABI 7/轴探测/插件配置/自描述端点）
- Modify: `.github/workflows/plugin-matrix.yml`（若引用 PluginRegistrations/register 表述则同步）
- 全量门禁 + 重建 bin/

**Interfaces:**
- Consumes: 全部前序。

- [ ] **Step 1: xtask `--check` 适配**

先读 `tools/xtask/src/main.rs` 的 check 实现（约 190-210 行一带，经 `PluginLoader`/`load_manifest` 预检）。变化点：descriptor 不再有 `register`，若 check 逻辑有「读 register 返回值/打印轴清单」步骤，改为对 `AXES` 逐轴探测并打印 `provided axes: [kv]`（探测辅助若未从 plugin_loader 导出，则导出 `pub fn probe_axes(lib) -> Registrations` 供复用，或由 `LoadedPlugin.registrations` 直接汇总——以最小改动为准）；预检输出追加 desc（`ok: auth 0.1.0 (abi 7) — auth 轴守卫插件：...`）。usage 文案若提及旧结构则同步。

- [ ] **Step 2: docs 同步**

- `docs/plugin-development.md`：ABI_VERSION 6→7（auth 计划 Task 8 已把 5 改 6——本计划再顶到 7）；删 PluginRegistrations/register 表述，改「`oj_plugin_entry!(init, kv => &VT)` 逐轴声明 + `oj_plugin_axis_<name>` 符号 + 缺符号 = 不提供该轴 + 加轴零破坏（ABI 规则表：既有轴形状变更才 bump）」；补「插件自有配置：`config.yaml` 的 `plugins.<name>` 开放段，宿主透传不解释，schema 插件自校验」与「自描述：descriptor.desc 必填，经 `GET {base}/plugins` 公开」。
- `docs/plugin-architecture.md`：注册机制章节同口径改写（探测流程：abi 门禁 → init → AXES 逐轴 dlsym；自描述收集与查询端点）。
- `docs/builtin-api-auth.md`：内置接口总览表加一行 `GET {base}/plugins`（公共，插件自描述清单）。
- `docs/devkit/api-manual.md`：831 行附近存量示例 `client.login("demo","demo1234")` 补第三参租户头（auth+tenant 用例与同节约束对齐；auth 计划复审遗留，随本任务 docs 重写一并修 + `cargo xtask build` 刷新 bin/devkit）。
- `CLAUDE.md`（本地文件，不入库）：插件系统段落同步（ABI 7、按轴符号、plugins 开放段、自描述端点）。

- [ ] **Step 3: 全量门禁**

```bash
cargo xtask build                      # 8 插件 ABI 7 全量重建入 bin/
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --release --workspace       # 含 start_cert_expired_test 等加载类（ABI 7 产物后应全绿）
cargo xtask plugin auth --check        # 预检走新符号探测，输出 provided axes
```
Expected: 全绿。

- [ ] **Step 4: Commit**

```bash
git add tools/xtask docs/ .github/ bin/
git commit -m "feat(xtask)+docs: 预检符号探测 + 按轴 dlsym 契约文档化（ABI 7 重建产物）

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- Spec 覆盖：契约瘦身/宏/desc 字段 → Task 1；探测/夹具 → Task 2；迁移（含 desc 填写）→ Task 3；插件配置三级回落/开放段/命名契约 → Task 4；自描述收集 + 查询端点 → Task 5；预检/docs/CI → Task 6。
- 类型一致性：`oj_plugin_axis_<name>() -> *const c_void`（Task 1 定义、Task 2 消费）；`AXES` pub（Task 2 定义、Task 4 断言）；`plugin_cfg(cfg, name) -> String`（Task 4 定义、Task 6 check 复用）；`PluginDescriptor.desc: RString`（Task 1 定义、Task 2/3 填写、Task 5 经 PluginInfo.description 暴露）；`Registrations` 镜像字段全程不变。
- 已知风险：① Task 1-3 之间 workspace 分层不可编译（破坏性契约变更固有；按序连续执行）；② mini-kv 假 vtable 的 FfiFuture/ready_err 签名以 future.rs 现状为准；③ serde_json 键序断言可能需解析回 Value 比对（Task 4 已注明）；④ `GET {base}/plugins` 为保留路径会遮蔽同名业务路由（spec 已声明，docs 落点 Task 6）。
