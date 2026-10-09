# 插件轴清单自报 + 泛型轴通道 + oj_info 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 插件自报轴清单/配置键 + 泛型轴通道（新轴零宿主改动）+ `oj info` CLI 与 JS `ojInfo()`（phpinfo 对应物）。

**Architecture:** 三个层次——FFI 层新增 `AxisDecl`/`GenericVtable` 类型与 `oj_plugin_axes()`/`oj_plugin_config_key()` 导出生成（宏双发，ABI 保持 11）；宿主装配层 `probe_axes` 改自报优先 + 旧符号回退、`cfg_for` 签名穿透 + 两-pass config extra；运行时层 `op_axis_call` + bootstrap `axis()` Proxy + `OjInfo` 注入 StableState。

**Tech Stack:** Rust / stabby FFI（RString/RVec/FfiFuture，见 `src/bridge/ffi.rs:339` 的 RVec `.iter()` 先例）/ deno_core op2 / serde_yaml 两-pass。

**设计 spec（权威）:** `docs/superpowers/specs/2026-10-09-plugin-auto-register-and-oj-info-design.md`（含双专家评审修订记录）。冲突时以 spec 为准。

## Global Constraints

- **禁止 debug 构建**：一切 cargo 命令 `--release`；唯一例外是测试夹具插件沿用既有 `tests/plugins` 模式（`cargo build -p <夹具>` 默认 profile，见 `plugin_loader/tests.rs:15-20`，既有约定不扩大）。
- 测试一律 `cargo test --release`；异步测试 `#[tokio::test(flavor = "current_thread")]`。
- **ABI_VERSION 保持 11**（oj-plugin-ffi/src/lib.rs:61）：只允许新增独立 repr(C) 类型与新导出符号，禁止改动既有 repr(C) 结构字段。
- 所有插件 profile 保持 `panic = "unwind"`；vtable 方法宿主不 catch_unwind（插件侧 catch_future 收敛）。
- `bootstrap.js` 必须 7-bit ASCII。
- JsRuntime 是 `!Send`：op 内 FFI await 走 `ffi::await_ffi`（`src/bridge/ffi.rs:151`），运行 current_thread。
- commit message 末尾加 `unix@vip.qq.com ai` 行。
- 版本 bump：**v0.1.54**（仅 T9 执行；T1–T8 不动 CHANGELOG/版本号）。

---

### Task 1: FFI — AxisDecl/GenericVtable 类型 + 宏生成自报符号

**Files:**
- Modify: `oj-plugin-ffi/src/lib.rs`（类型 ~26-60 区间；宏 ~108-142；测试 mod ~160-）
- Test: `oj-plugin-ffi/src/lib.rs` 内 `mod tests`

**Interfaces:**
- Consumes: 既有 `RString`/`RVec`/`FfiFuture`（`pub type RVec<T> = stabby::vec::Vec<T>`，lib.rs:38）
- Produces（T3/T5 依赖）:
  - `#[repr(C)] pub struct AxisDecl { pub name: RString, pub vtable: *const core::ffi::c_void }`
  - `#[repr(C)] pub struct GenericVtable { pub call: extern "C" fn(op: RString, args: RString) -> FfiFuture }`
  - 宏生成 `oj_plugin_axes() -> RVec<AxisDecl>`（始终导出，零轴 = 空表）
  - 宏可选生成 `oj_plugin_config_key() -> RString`（语法 `config: "键名"`）

- [ ] **Step 1: 写失败的宏测试**

在 `oj-plugin-ffi/src/lib.rs` 的 `mod tests` 追加：

```rust
#[test]
fn entry_generates_axes_list_and_config_key() {
    mod fake {
        use super::super::*;
        pub static VT_A: FakeVt = FakeVt { _pad: 0 };
        pub static VT_B: FakeVt = FakeVt { _pad: 1 };
        // 既有 FakeVt 单字节占位即可（本测试只验清单内容，不调用 vtable）。
        oj_plugin_entry!(init, config: "cache", cache => &VT_A, search => &VT_B);
        fn init(
            _host: RArc<HostContext>,
            _cfg: RString,
        ) -> RResult<PluginDescriptor, RString> {
            unreachable!()
        }
    }
    let axes = fake::oj_plugin_axes();
    let names: Vec<String> = axes.iter().map(|d| d.name[..].to_string()).collect();
    assert_eq!(names, vec!["cache".to_string(), "search".to_string()]);
    // vtable 指针必须指向声明的静态 vtable（与 per-axis 符号同址）。
    assert_eq!(axes.iter().next().unwrap().vtable, &raw const fake::VT_A);
    assert!(!fake::oj_plugin_axis_cache().is_null()); // 双发：per-axis 符号仍在
    assert_eq!(&fake::oj_plugin_config_key()[..], "cache");
}

#[test]
fn entry_without_config_exports_no_config_key_symbol() {
    // 零轴零 config 形态：不导出 oj_plugin_config_key（由 dlsym 失败判定，
    // 单测只验宏可展开 + 空清单）。
    mod fake2 {
        use super::super::*;
        oj_plugin_entry!(init);
        fn init(
            _host: RArc<HostContext>,
            _cfg: RString,
        ) -> RResult<PluginDescriptor, RString> {
            unreachable!()
        }
    }
    assert_eq!(fake2::oj_plugin_axes().iter().count(), 0);
}
```

注意：`&raw const fake::VT_A` 取静态地址；若 toolchain 较旧改用 `&fake::VT_A as *const _`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj-plugin-ffi entry_generates`
Expected: FAIL（编译错误：`oj_plugin_axes`/`oj_plugin_config_key` 未生成）

- [ ] **Step 3: 实现类型与宏**

`oj-plugin-ffi/src/lib.rs`，在 `RString` 等类型定义之后加：

```rust
/// 轴声明（自报清单条目）。RString 不可 const 构造 → 清单调用期经 RVec 返回
///（评审 M1：不做 static 数组）。
#[repr(C)]
pub struct AxisDecl {
    pub name: RString,
    pub vtable: *const core::ffi::c_void,
}

/// 泛型轴 vtable：op 名 + JSON 参数 → JSON bytes 结果。
/// FfiFuture 与全部既有 vtable 同形（错误经 future Err 透传）。
#[repr(C)]
pub struct GenericVtable {
    pub call: extern "C" fn(op: RString, args: RString) -> FfiFuture,
}
```

宏改签名（把原 `$($axis:ident => $vtable:expr)*` 臂替换，**config 臂必须在前**）：

```rust
#[macro_export]
macro_rules! oj_plugin_entry {
    ($init:expr $(, config: $ck:literal)? $(, $axis:ident => $vtable:expr)* $(,)?) => {
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
            $crate::paste::paste! {
                #[unsafe(no_mangle)]
                pub extern "C" fn [<oj_plugin_axis_ $axis:lower>]() -> *const ::core::ffi::c_void {
                    $vtable as *const _ as *const ::core::ffi::c_void
                }
            }
        )*

        /// 轴自报清单（宿主优先路径；旧宿主无此符号 → 回退逐轴 dlsym）。
        /// 宏双发：per-axis 符号保留至回退路径退役（spec 评审 M3）。
        #[unsafe(no_mangle)]
        pub extern "C" fn oj_plugin_axes() -> $crate::RVec<$crate::AxisDecl> {
            let mut v = $crate::RVec::new();
            $(
                v.push($crate::AxisDecl {
                    name: $crate::RString::from(stringify!($axis)),
                    vtable: $vtable as *const _ as *const ::core::ffi::c_void,
                });
            )*
            v
        }

        $(
            #[unsafe(no_mangle)]
            pub extern "C" fn oj_plugin_config_key() -> $crate::RString {
                $crate::RString::from($ck)
            }
        )?
    };
}
```

轴标识小写约定：宏文档注释补一句「轴标识必须小写（stringify 原样进清单，无 :lower 兜底）」。

- [ ] **Step 4: 跑测试确认通过 + 全 crate 回归**

Run: `cargo test --release -p oj-plugin-ffi`
Expected: PASS（含既有宏测试——既有 `oj_plugin_entry!(init, kv => ...)` 调用形态全部兼容）

- [ ] **Step 5: Commit**

```bash
git add oj-plugin-ffi/src/lib.rs
git commit -m "feat(ffi): 轴清单自报 AxisDecl/GenericVtable + 宏生成 oj_plugin_axes/config_key

ABI 保持 11（仅新增类型与符号）。宏双发 per-axis 符号保反向兼容。

unix@vip.qq.com ai"
```

---

### Task 2: 测试夹具 — mini-legacy（手写符号）与 mini-generic（泛型轴）

**Files:**
- Create: `tests/plugins/mini-legacy/`（Cargo.toml + src/lib.rs，手写符号不经过宏）
- Create: `tests/plugins/mini-generic/`（Cargo.toml + src/lib.rs，宏声明泛型轴 "greet"）
- Modify: `src/bridge/plugin_loader/tests.rs`（新增两个 fixture dir 助手 + 加载用例）

**Interfaces:**
- Consumes: T1 宏；既有夹具模式（`tests/plugins/mini-nosym` 手写符号先例、`tests/plugins/mini-kv` 单轴夹具结构）
- Produces（T3 依赖）:
  - crate `oj-plugin-test-mini-legacy` → 库文件短名 `mini-legacy`，导出 `oj_plugin_abi_version`/`oj_plugin_init`/`oj_plugin_axis_kv`（**不导出** `oj_plugin_axes`），init 认 kv vtable 形状
  - crate `oj-plugin-test-mini-generic` → 短名 `mini-generic`，宏 `oj_plugin_entry!(init, greet => &GREET_VT)`（**不在** 9 个类型化轴名内 → 泛型）；`greet` op：`call("greet", args)` 返回 `Ok(serde_json::json!({"hello": <args 里的 name>}))` bytes
  - `tests.rs` 助手 `mini_legacy_plugin_dir()` / `mini_generic_plugin_dir()`（各自独立 subdir，沿用 `fixture_plugin_dir` 模式）

- [ ] **Step 1: 建 mini-legacy 夹具（先写，无失败测试可跑——夹具即交付物）**

`tests/plugins/mini-legacy/Cargo.toml` 复制 `tests/plugins/mini-nosym/Cargo.toml` 改 `name = "oj-plugin-test-mini-legacy"`。`src/lib.rs` 镜像 mini-nosym 的手写符号结构（`#[no_mangle] pub extern "C" fn oj_plugin_abi_version/init`），但**补上** `oj_plugin_axis_kv`（照抄 mini-kv 的 kv vtable 静态与符号函数；照抄时连同其 catch_future 包装）。关键：全文件不出现 `oj_plugin_axes` 字样。init 行为对齐 mini-kv（识别 `kv` 探针）。

- [ ] **Step 2: 建 mini-generic 夹具**

`tests/plugins/mini-generic/Cargo.toml` 复制 mini-kv 的改 name。`src/lib.rs`：

```rust
//! 泛型轴夹具：轴名 "greet" 不在宿主 9 个类型化轴内 → 泛型通道。
use oj_plugin_ffi::{FfiFuture, PluginDescriptor, RArc, RResult, RString, HostContext, oj_plugin_entry};

static GREET_VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable { call: greet };

extern "C" fn greet(op: RString, args: RString) -> FfiFuture {
    oj_plugin_ffi::catch_future(async move {
        if &op[..] != "greet" {
            return Err(format!("greet axis: unknown op '{}'", &op[..]));
        }
        let name = serde_json::from_slice::<serde_json::Value>(&args.to_bytes())
            .ok()
            .and_then(|v| v.as_array().and_then(|a| a[0].as_str().map(String::from)))
            .unwrap_or_else(|| "world".to_string());
        Ok(serde_json::json!({ "hello": name }).to_string().into_bytes())
    })
}

fn init(_host: RArc<HostContext>, _cfg: RString) -> RResult<PluginDescriptor, RString> {
    // descriptor 形状照抄 mini-kv 的 init（name="mini-generic" 之类）。
    // ...
}

oj_plugin_entry!(init, greet => &GREET_VT);
```

注：`catch_future`/`to_bytes` 的确切名字以 `oj_plugin_ffi` 导出与 mini-kv 用法为准（实现时打开 `tests/plugins/mini-kv/src/lib.rs` 对照抄改）；若 oj-plugin-ffi 无 `catch_future` 导出，用 mini-kv 同款 future 构造助手。

- [ ] **Step 3: 写加载失败测试（T3 的行为此刻尚未实现，先钉期望）**

`src/bridge/plugin_loader/tests.rs` 追加：

```rust
fn mini_legacy_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        fixture_plugin_dir(
            "oj-plugin-test-mini-legacy",
            "oj_plugin_test_mini_legacy",
            "mini-legacy",
            "test-plugins-legacy",
        )
    })
    .clone()
}

fn mini_generic_plugin_dir() -> PathBuf {
    static ONCE: OnceLock<PathBuf> = OnceLock::new();
    ONCE.get_or_init(|| {
        fixture_plugin_dir(
            "oj-plugin-test-mini-generic",
            "oj_plugin_test_mini_generic",
            "mini-generic",
            "test-plugins-generic",
        )
    })
    .clone()
}

#[test]
fn legacy_plugin_without_axes_symbol_loads_via_fallback() {
    let dir = mini_legacy_plugin_dir();
    let loaded = load_scanned(&dir, host_context(), &no_cfg).expect("legacy scan load");
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].registrations.kv.is_some(), "kv via fallback dlsym");
}

#[test]
fn generic_axis_plugin_reports_greet_axis() {
    let dir = mini_generic_plugin_dir();
    let loaded = load_scanned(&dir, host_context(), &no_cfg).expect("generic scan load");
    assert_eq!(loaded.len(), 1);
    let names: Vec<&str> = loaded[0].generic_axes.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["greet"]);
    assert!(loaded[0].registrations.kv.is_none());
}
```

（`no_cfg` 签名将在 T4 变为 `Fn(&str, Option<&str>)`；本步先按现状写，T4 统一改。）

- [ ] **Step 4: 跑测试确认按预期失败/编译错**

Run: `cargo test --release -p only-js generic_axis_plugin_reports`
Expected: FAIL（`generic_axes` 字段不存在——T3 实现）

- [ ] **Step 5: Commit（夹具与测试先行入库，红测保留）**

```bash
git add tests/plugins/mini-legacy tests/plugins/mini-generic src/bridge/plugin_loader/tests.rs Cargo.toml
git commit -m "test(plugins): mini-legacy 手写符号 + mini-generic 泛型轴夹具（红测，T3 转绿）

unix@vip.qq.com ai"
```

（若 workspace 根 Cargo.toml 需 `members`/`exclude` 登记新夹具 crate，一并加入。）

---

### Task 3: 宿主 loader — probe_axes 自报优先 + 回退 + 泛型/unknown 分类

**Files:**
- Modify: `src/bridge/plugin_loader.rs`（Registrations ~94-106、LoadedPlugin ~108、load_one ~329-429、AXES ~431-466、provides ~468-487）
- Test: `src/bridge/plugin_loader/tests.rs`（T2 红测转绿 + 新增）

**Interfaces:**
- Consumes: T1 类型/符号；T2 夹具
- Produces（T4/T5/T6 依赖）:
  - `pub struct LoadedPlugin { pub descriptor, pub registrations, pub generic_axes: Vec<(String, &'static oj_plugin_ffi::GenericVtable)>, pub unknown_axes: Vec<String>, pub config_key: Option<String> }`
  - `pub struct PluginInfo { ..., pub unknown_axes: Vec<String> }`（op_plugins / `GET {base}/plugins` 输出增量字段）
  - `pub(crate) const TYPED_AXES: &[&str]`（原 AXES 改名降级；loader 内部 + 回退探测用）

- [ ] **Step 1: 先跑 T2 红测确认仍红**

Run: `cargo test --release -p only-js generic_axis_plugin_reports`
Expected: FAIL（字段不存在）

- [ ] **Step 2: 实现 LoadedPlugin 扩展 + probe 重写**

`plugin_loader.rs`：

```rust
/// 宿主认识的类型化轴（9 个）。清单按名匹配的右值表 + 回退逐轴 dlsym 共用。
/// pub(crate)：crate 内部实现细节（评审 M2/M4——非公共契约，xtask 不再 import）。
pub(crate) const TYPED_AXES: &[&str] =
    &["es", "db", "blob", "bus", "kv", "auth", "mq", "mail", "ldap"];

pub struct LoadedPlugin {
    pub descriptor: PluginDescriptor,
    pub registrations: Registrations,
    /// 泛型轴：清单轴名不在 TYPED_AXES（vtable 按 GenericVtable 解释；撞保留名
    /// 的按 typed 解释，故此处必然是非保留名）。
    pub generic_axes: Vec<(String, &'static oj_plugin_ffi::Genericvtable_stub)>, // 实现时用 oj_plugin_ffi::GenericVtable
    /// 自报了但宿主既不认识、泛型也不收的轴名（理论上仅宏被绕过等异常）。
    pub unknown_axes: Vec<String>,
    /// init 前探测的声明配置段键（oj_plugin_config_key；旧插件 None）。
    pub config_key: Option<String>,
}
```

（上面 stub 注释仅为提示——实际写 `&'static oj_plugin_ffi::GenericVtable`。）

`load_one` 重排（spec Part 3 时序）：

```rust
// abi 门禁之后、init 之前：
let config_key = unsafe {
    lib.get::<extern "C" fn() -> RString>(b"oj_plugin_config_key")
        .ok()
        .map(|f| f()[..].to_string())
};
let r = init_sym(host, RString::from(cfg_for(&probe, config_key.as_deref()).as_str()));
// init/descriptor/指纹/身份门禁不变……
let (registrations, generic_axes, unknown_axes) = unsafe { probe_axes(lib, &probe) };
```

`probe_axes` 重写：

```rust
unsafe fn probe_axes(
    lib: &libloading::Library,
    plugin_name: &str,
) -> (Registrations, Vec<(String, &'static oj_plugin_ffi::GenericVtable)>, Vec<String>) {
    if let Ok(f) = lib.get::<unsafe extern "C" fn() -> oj_plugin_ffi::RVec<oj_plugin_ffi::AxisDecl>>(b"oj_plugin_axes") {
        let decls = unsafe { f() };
        return classify_axes(plugin_name, decls.iter());
    }
    eprintln!("[oj-plugin] '{plugin_name}': no oj_plugin_axes symbol, falling back to per-axis dlsym (deprecated; rebuild plugin)");
    // 回退：逐轴 dlsym（原逻辑，填 Registrations）。
    let mut r = Registrations::default();
    for axis in TYPED_AXES {
        let sym = format!("oj_plugin_axis_{axis}");
        let Ok(f) = (unsafe { lib.get::<unsafe extern "C" fn() -> *const std::ffi::c_void>(sym.as_bytes()) }) else { continue };
        let vt = unsafe { f() };
        if vt.is_null() { continue }
        fill_typed_slot(&mut r, axis, vt); // 原 match 臂抽成函数，classify 复用
    }
    (r, Vec::new(), Vec::new())
}

fn classify_axes<'a>(
    plugin_name: &str,
    decls: impl Iterator<Item = &'a oj_plugin_ffi::AxisDecl>,
) -> (Registrations, Vec<(String, &'static oj_plugin_ffi::GenericVtable)>, Vec<String>) {
    let mut r = Registrations::default();
    let mut generic = Vec::new();
    let mut unknown = Vec::new();
    for d in decls {
        let name = d.name[..].to_string();
        if TYPED_AXES.contains(&name.as_str()) {
            fill_typed_slot(&mut r, &name, d.vtable);
        } else {
            // 安全前提：插件按 GenericVtable 形状构造该 vtable（信任边界同既有轴）。
            generic.push((name.clone(), unsafe { &*(d.vtable as *const oj_plugin_ffi::GenericVtable) }));
        }
    }
    let _ = plugin_name;
    (r, generic, unknown)
}
```

`fill_typed_slot`：把原 `probe_axes` 里 9 个 match 臂搬进去（`"es" => r.es = Some(&*(vt as *const ...))` 等），`classify_axes` 与回退循环共用（DRY）。

注：`unknown_axes` 在 classify 中恒空——保留字段是为「撞保留名却按 typed cast 失败」等未来防御与 spec 对称；宏路径产生的清单若含非法指针，行为与既有 vtable 信任边界一致。为本任务计，加一个真实 unknown 场景：清单中轴名大小写不符（如 `Cache`）——不进 TYPED_AXES 也不该静默成泛型？不：非保留名即泛型是设计。unknown 保持恒空实现，测试断言空。`provides()` 不变（沿用 Registrations）。

- [ ] **Step 3: PluginInfo 增 unknown_axes**

```rust
pub struct PluginInfo {
    // ...既有字段...
    pub unknown_axes: Vec<String>,
}
impl From<&LoadedPlugin> for PluginInfo { /* 补 unknown_axes: p.unknown_axes.clone() */ }
```

- [ ] **Step 4: 跑测试转绿 + 全量回归**

Run: `cargo test --release -p only-js plugin_loader`
Expected: PASS（T2 两个新用例 + 既有 loader 测试；回退路径被既有 mini 夹具覆盖——它们重编译后经宏获得 oj_plugin_axes，走自报路径；**legacy 用例钉回退**）

- [ ] **Step 5: Commit**

```bash
git add src/bridge/plugin_loader.rs src/bridge/plugin_loader/tests.rs
git commit -m "feat(loader): probe_axes 自报清单优先 + 旧符号回退 + 泛型轴/unknown_axes 分类

TYPED_AXES 降级 pub(crate)（评审 M2/M4）。宏双发保证旧宿主兼容。

unix@vip.qq.com ai"
```

---

### Task 4: cfg 通路 — cfg_for 签名穿透 + 两-pass extra + 三级解析 + 未消费段诊断

**Files:**
- Modify: `src/config.rs`（load_from ~1020-1068、Config ~940）
- Modify: `src/bridge/plugin_loader.rs`（load_one/load_manifest/load_scanned 签名）
- Modify: `oj/src/serve_cmd.rs`（cfg_for 闭包 ~835、plugin_cfg ~633-690、assemble_plugins ~811）
- Modify: `tools/xtask/src/main.rs`（cfg_for ~243、AXES import ~21 与渲染 ~253-261）
- Test: `src/config.rs` tests、`oj/src/serve_cmd.rs` tests（~1016 起 plugin_cfg 族）

**Interfaces:**
- Consumes: T3 的 `LoadedPlugin.config_key`
- Produces（T5/T6 依赖）:
  - `pub struct LoadedConfig { pub config: Config, pub top: serde_json::Value }`（top = 解密后顶层 mapping 的 JSON 形）
  - `pub fn load_with_extra(path: &Path, dir: &Path) -> Result<LoadedConfig, String>`；既有 `load_from` 变为薄包装 `.map(|c| c.config)`
  - `pub fn known_top_level_keys() -> &'static [&'static str]`（与 Config 字段一一对应，注释要求新增字段必须同步）
  - `cfg_for: &dyn Fn(&str, Option<&str>) -> String`（全仓库 4 个调用点）
  - `pub(crate) fn plugin_cfg(cfg: &Config, top: &serde_json::Value, name: &str, config_key: Option<&str>, es_profile: Option<&str>) -> Result<String, String>`
  - `pub(crate) fn unconsumed_sections(top: &serde_json::Value, cfg: &Config, loaded: &[LoadedPlugin]) -> Vec<String>`

- [ ] **Step 1: 写失败测试**

`src/config.rs` tests 追加：

```rust
#[test]
fn load_with_extra_splits_known_and_unknown_top_level() {
    let dir = std::env::temp_dir();
    let p = dir.join("oj-test-extra-config.yaml");
    std::fs::write(&p, r#"
server: { host: "127.0.0.1", port: 9778 }
vars: { PORT: 3000 }
cache:
  backend: memory
  ttl: 60
"#).unwrap();
    let loaded = load_with_extra(&p, &dir).expect("load_with_extra");
    // vars 数字标量仍按字面读成串（既有宽松行为不可回归）：
    assert_eq!(loaded.config.vars.get("PORT").map(String::as_str), Some("3000"));
    // 未知段进 extra（已知段不进）：
    assert!(loaded.top.get("cache").is_some());
    assert!(loaded.top.get("server").is_none());
    assert!(loaded.top.get("vars").is_none());
    std::fs::remove_file(&p).ok();
}

#[test]
fn known_top_level_keys_covers_all_typed_fields() {
    // 对账单测（评审 H1/S3）：每个已知键都能被 Config 解析接受，
    // 且列表不含重复。新增 Config 字段必须同步此表。
    let keys = known_top_level_keys();
    assert_eq!(keys.len(), keys.iter().collect::<std::collections::HashSet<_>>().len());
    for k in keys {
        let yaml = format!("{k}: null\n");
        let v: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let extra = split_extra(&v);
        assert!(extra.get(*k).is_none(), "known key '{k}' leaked into extra");
    }
}
```

`oj/src/serve_cmd.rs` tests 追加：

```rust
#[test]
fn plugin_cfg_config_key_reads_top_level_section() {
    let cfg = Config::default();
    let top = serde_json::json!({ "cache": { "ttl": 60 }, "plugins": {} });
    let s = plugin_cfg(&cfg, &top, "oj-cache", Some("cache"), None).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&s).unwrap()["ttl"], 60);
}

#[test]
fn plugin_cfg_passthrough_still_wins() {
    let mut cfg = Config::default();
    cfg.plugins.insert("oj-cache".into(), serde_json::json!({ "ttl": 1 }));
    let top = serde_json::json!({ "cache": { "ttl": 60 } });
    let s = plugin_cfg(&cfg, &top, "oj-cache", Some("cache"), None).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(&s).unwrap()["ttl"], 1);
}

#[test]
fn plugin_cfg_legacy_es_arm_unchanged() {
    // 无 config_key → 走遗留臂（既有 es/auth/mail/ldap 测试族保持绿即可，此处只钉回退）：
    let cfg = Config::default();
    let top = serde_json::json!({});
    let s = plugin_cfg(&cfg, &top, "es", None, None).unwrap();
    assert_eq!(s, "{}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js load_with_extra ; cargo test --release -p oj plugin_cfg_config_key`
Expected: FAIL（`load_with_extra`/`known_top_level_keys`/`plugin_cfg` 新签名不存在）

- [ ] **Step 3: 实现两-pass extra（config.rs）**

在 `load_from` 旁新增（**不改**既有 `from_str` 主路径的标量语义；解密回经文本行为保留）：

```rust
/// 与 Config 字段一一对应的顶层键清单。**新增 Config 字段必须同步此表**
///（对账单测 known_top_level_keys_covers_all_typed_fields 防漂移）。
pub fn known_top_level_keys() -> &'static [&'static str] {
    &["server", "db", "redis", "tenant", "auth", "oidc", "blob", "fs", "smtp",
      "ldap", "es", "broker", "plugins", "plugins_dir", "kafkas", "rabbits",
      "vars", "db_query", "db_trace", "tasks", "secrets", "input_contracts",
      "ws", "mq", "cert", "finish"] // 实现时按 Config 实际字段逐一核对，禁止遗漏
}

/// 顶层 Value 减去已知键 = 未消费候选（serde_yaml::Value 版，供 load 内使用）。
fn split_extra_yaml(v: &serde_yaml::Value) -> serde_yaml::Value {
    let Some(m) = v.as_mapping() else { return serde_yaml::Value::Null };
    let kept: serde_yaml::Mapping = m.iter()
        .filter(|(k, _)| !k.as_str().is_some_and(|s| known_top_level_keys().contains(&s)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_yaml::Value::Mapping(kept)
}

pub struct LoadedConfig {
    pub config: Config,
    /// 解密后顶层 mapping 的 JSON 形（已知 + 未知段全量；插件 config key 查找面）。
    pub top: serde_json::Value,
}

pub fn load_with_extra(path: &Path, dir: &Path) -> Result<LoadedConfig, String> {
    // 复用既有读文件 + ENC 解密逻辑：把原 load_from 主体抽为返回 (text_or_reserialized)；
    // 此处对解密后/原文文本先解析 serde_yaml::Value 得 top，
    // 再按既有路径 from_str::<Config>（保证 vars 等宽松行为逐字节不变）。
    let (config, top) = /* 抽取实现：既有逻辑 + split_extra_yaml → serde_json::to_value */;
    Ok(LoadedConfig { config, top })
}
```

实现要点：把 `load_from` 现有主体（读文本 → 有 ENC 则解密并回经文本 → `from_str`）抽成内部函数返回最终文本；`load_with_extra` 对最终文本 `serde_yaml::from_str::<serde_yaml::Value>` → `serde_json::to_value` 得全量 top（JSON 形供插件 cfg；段值保持 YAML 类型化的 JSON 形态，数字仍是数字——与 typed Config 的裸标量读串行为分层，互不影响）。`load_from` 改薄包装（DRY）。

- [ ] **Step 4: cfg_for 签名穿透（4 个调用点）**

`plugin_loader.rs`：`load_one`/`load_manifest`/`load_scanned` 的 `cfg_for: &dyn Fn(&str) -> String` 全部改 `&dyn Fn(&str, Option<&str>) -> String`；`load_one` 内调用点改 `cfg_for(&probe, config_key.as_deref())`。

`serve_cmd.rs`：cfg_for 闭包改

```rust
let cfg_for = |name: &str, config_key: Option<&str>| -> String {
    plugin_cfg(cfg, &top, name, config_key, es_profile).unwrap_or_else(|e| panic!("plugin_cfg: {e}"))
};
```

（`run`/`assemble_plugins` 需要拿到 `top`：把 `load_app_config` 的调用点改为 `load_with_extra` 并向下传 `top`；签名怎么传由编译器驱动——所有 `cfg_for` 类型错就是改动清单。）

`plugin_cfg` 新三级（passthrough 与遗留臂代码原样保留，只加第 2 级）：

```rust
pub(crate) fn plugin_cfg(
    cfg: &Config,
    top: &serde_json::Value,
    name: &str,
    config_key: Option<&str>,
    es_profile: Option<&str>,
) -> Result<String, String> {
    if let Some(v) = cfg.plugins.get(name)
        && v.as_object().is_some_and(|o| !o.is_empty())
    {
        return Ok(v.to_string());
    }
    // 第 2 级：插件自报 config key → 全量顶层 Value 查找（含宿主已知段——
    // 段是读不是占；撞已知名是合法场景，评审 H2 的静默 {} 洞由此消解）。
    if let Some(k) = config_key {
        if let Some(v) = top.get(k) {
            return serde_json::to_string(v).map_err(|e| format!("plugin cfg section '{k}': {e}"));
        }
        return Ok("{}".to_string()); // 段可选是既有语义
    }
    match name { /* 既有 es/auth/mail/ldap 臂原样 */ }
}
```

`tools/xtask/src/main.rs`：cfg_for 改 `|_name, _key| "{}"`（预检语义不变）；AXES import 改——`--check` 渲染不再遍历 `AXES`（它已 pub(crate)），改为直接输出 `loaded[0].generic_axes` 轴名 + `registrations` 非 None 的槽位（经 `provides` 逐键查询需要轴名表——用 `["es","db","blob","bus","kv","auth","mq","mail","ldap"]` 本地常量仅供预检渲染，注释标注与 plugin_loader::TYPED_AXES 对账；或把 TYPED_AXES 保持 `pub` 并 #[doc(hidden)]——**二选一，取后者更懒**：TYPED_AXES 保持 pub，xtask 继续用，仅注释从「公共契约」改为「内部实现细节，勿依赖」）。

- [ ] **Step 5: 未消费段诊断**

`serve_cmd.rs`：

```rust
/// 装配后顶层 Value 中未被任何已加载插件消费的段（config key 或 plugins:<name> 非空）。
pub(crate) fn unconsumed_sections(
    top: &serde_json::Value,
    cfg: &Config,
    loaded: &[LoadedPlugin],
) -> Vec<String> {
    let consumed: std::collections::HashSet<&str> = loaded.iter()
        .map(|p| p.config_key.as_deref())
        .flatten()
        .chain(cfg.plugins.iter()
            .filter(|(_, v)| v.as_object().is_some_and(|o| !o.is_empty()))
            .map(|(k, _)| k.as_str()))
        .collect();
    top.as_object().map(|m| m.keys()
        .filter(|k| !consumed.contains(k.as_str()))
        .cloned().collect()).unwrap_or_default()
}
```

装配完成处（`assemble_plugins` 返回后 / `run` 内）：非空则 `eprintln!("[oj-serve] unconsumed config sections: {list:?} (typo? or plugin not loaded)")`。serve_cmd tests：`unconsumed_sections` 单测（撞名/透传/正常三形态）。

- [ ] **Step 6: 全量回归（plugin_cfg 既有测试族必须全绿）**

Run: `cargo test --release -p only-js ; cargo test --release -p oj ; cargo test --release -p xtask`
Expected: PASS（`plugin_cfg_fallback_chain` / `plugin_cfg_es_profile_selection` / `plugin_cfg_mail_serializes_top_level_smtp_section` 等保持绿）

- [ ] **Step 7: Commit**

```bash
git add src/config.rs src/bridge/plugin_loader.rs oj/src/serve_cmd.rs tools/xtask/src/main.rs
git commit -m "feat(config): 两-pass extra + 插件自报 config key 三级解析 + 未消费段诊断

flatten 方案被评审否决（全字段经 Content 缓冲会丢 serde_yaml 宽松语义）；
改用 Value 层切分，既有解析路径零变更。

unix@vip.qq.com ai"
```

---

### Task 5: 泛型轴运行时 — 冲突检查 + op_axis_call + bootstrap axis() + e2e

**Files:**
- Modify: `oj/src/serve_cmd.rs`（build_registries ~700：泛型轴收集与冲突）
- Modify: `src/bridge/mod.rs`（StableState ~136、Extras ~205、bridge_ext ops 列表 ~406）
- Modify: `src/bridge/bootstrap.js`（全局装配，~950 行 globalThis 区）
- Modify: `src/bridge/mq.rs` 或新建 `src/bridge/generic_axis.rs`（op；**新建小文件更符合单一职责**，bridge/mod.rs 加 `pub mod generic_axis;`）
- Test: `src/bridge/generic_axis.rs` 单测；`oj/tests/e2e.rs` 追加 e2e

**Interfaces:**
- Consumes: T3 `LoadedPlugin.generic_axes`、T4 无依赖
- Produces（T6/T7 依赖）:
  - `Registries.generic: Vec<(String /*axis*/, String /*plugin*/, &'static oj_plugin_ffi::GenericVtable)>`
  - `StableState.generic_axes: Arc<HashMap<String, GenericAxisHandle>>`，`GenericAxisHandle { plugin: String, vt: &'static GenericVtable }`
  - `Extras.generic_axes: Option<Arc<HashMap<String, GenericAxisHandle>>>`
  - `#[op2] pub async fn op_axis_call(state: Rc<RefCell<OpState>>, #[string] name: String, #[string] op: String, #[string] args: String) -> Result<serde_json::Value, deno_core::error::AnyError>`（未知轴错误列可用轴名）
  - bootstrap: `globalThis.axis = (name) => Proxy`

- [ ] **Step 1: 写失败测试**

`src/bridge/generic_axis.rs`（新文件）单测——不依赖 v8，直测调用协议与查表：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fake_vt(fail: bool) -> oj_plugin_ffi::GenericVtable {
        extern "C" fn call(op: RString, _args: RString) -> FfiFuture {
            let ok = &op[..] == "ok";
            crate::bridge::ffi::test_util::ready_future( // 若 ffi 无此 helper，见 Step 3 注
                if ok { Ok(br#"{"r":1}"#.to_vec()) } else { Err("boom".into()) },
            )
        }
        oj_plugin_ffi::GenericVtable { call }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn call_dispatches_and_parses_json() {
        let vt = fake_vt(false);
        let out = call_axis(&vt, "ok", "[]").await.unwrap();
        assert_eq!(out["r"], 1);
        let err = call_axis(&vt, "bad", "[]").await.unwrap_err();
        assert!(err.to_string().contains("boom"));
    }
}
```

e2e（`oj/tests/e2e.rs`，照既有 e2e 形态）：扫描 `mini-generic` 夹具目录 + `axis("greet").greet("oj")` → `{"hello":"oj"}`；未知轴 `axis("nope").x()` 报错且消息含 "greet"（可用轴名列表）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p only-js call_dispatches`
Expected: FAIL（`generic_axis` 模块不存在）

- [ ] **Step 3: 实现 op 模块**

`src/bridge/generic_axis.rs`：

```rust
//! 泛型轴通道：插件自报的非类型化轴经 op_axis_call + bootstrap axis() Proxy 暴露给 JS。
//! 信任边界与既有 vtable 相同：插件必须按 GenericVtable 形状构造 vtable、
//! 经 catch_future 收敛 panic（panic=unwind 红线，宿主侧不 catch_unwind）。
use deno_core::{OpState, op2};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub struct GenericAxisHandle {
    pub plugin: String,
    pub vt: &'static oj_plugin_ffi::GenericVtable,
}

pub type GenericAxisRegistry = Arc<HashMap<String, GenericAxisHandle>>;

/// 单次泛型轴调用：JSON args 进 → JSON Value 出；future Err 原样透传为 JS 异常。
pub(crate) async fn call_axis(
    vt: &'static oj_plugin_ffi::GenericVtable,
    op: &str,
    args: &str,
) -> Result<serde_json::Value, deno_core::error::AnyError> {
    let fut = (vt.call)(RString::from(op), RString::from(args));
    let bytes = super::ffi::await_ffi(fut).await.map_err(deno_core::error::generic_error)?;
    serde_json::from_slice(&bytes).map_err(|e| deno_core::error::generic_error(format!(
        "axis op '{op}': plugin returned non-JSON ({e})"
    )))
}

#[op2]
pub async fn op_axis_call(
    state: Rc<RefCell<OpState>>,
    #[string] name: String,
    #[string] op: String,
    #[string] args: String,
) -> Result<serde_json::Value, deno_core::error::AnyError> {
    let reg = {
        let s = state.borrow();
        s.borrow::<super::StableState>().generic_axes.clone()
    };
    let handle = reg.get(&name).ok_or_else(|| {
        let avail: Vec<&str> = reg.keys().map(String::as_str).collect();
        deno_core::error::generic_error(format!(
            "unknown generic axis '{name}' (available: {avail:?})"
        ))
    })?;
    call_axis(handle.vt, &op, &args).await
}
```

注：测试用的 `ready_future` helper 若 `ffi` 无现成的，在 `generic_axis.rs` tests 内自建一个最小 FfiFuture（照 `ffi.rs` 内 mock 的 future 构造，~1360 行有先例）。

- [ ] **Step 4: 注册表接线（mod.rs + serve_cmd）**

`mod.rs`：`StableState` 与 `Extras` 各加 `generic_axes` 字段（`Extras` 缺省 `Some(空表)`——与 `vars` 同哲学：未注入 = 空 registry 而非 None，op 报 unknown axis）；`bridge_ext` 的 ops 列表加 `generic_axis::op_axis_call`；`Bridge::new` 两处 `StableState { ... }` 构造（~680、~2002）从 Extras 填入（编译器会强制补全所有构造点）。

`serve_cmd.rs` `build_registries` 末尾收集：

```rust
// 泛型轴：跨插件轴名冲突 fail-fast（与「不静默跳过」哲学一致）。
let mut seen: HashMap<&str, &str> = HashMap::new();
let mut generic = Vec::new();
for p in loaded {
    for (axis, vt) in &p.generic_axes {
        if let Some(prev) = seen.insert(axis, &p.descriptor.name[..]) {
            return Err(format!(
                "plugins conflict: generic axis '{axis}' provided by both '{prev}' and '{}' \
                 (generic axes are single-provider per axis)",
                &p.descriptor.name[..]
            ));
        }
        generic.push((axis.clone(), p.descriptor.name[..].to_string(), *vt));
    }
}
```

`start()` 装配处把 `Registries.generic` 转成 `HashMap<String, GenericAxisHandle>` 注入 Extras。

- [ ] **Step 5: bootstrap.js axis() Proxy**

在 `globalThis.plugins` 装配附近（保持 7-bit ASCII）：

```js
// Generic-axis channel: axis("cache").get("k") -> op_axis_call("cache","get",...).
// ojStringify is BigInt-safe (plain JSON.stringify throws on BigInt).
globalThis.axis = (name) => new Proxy({}, {
  get: (_, op) => {
    if (typeof op !== "string" || op === "then") return undefined;
    return (...args) => op_axis_call(name, op, ojStringify(args));
  },
});
```

`op_axis_call` 经 `ext:core/ops` 导入挂入（照 bootstrap.js 顶部既有 `op_*` 导入模式）。

- [ ] **Step 6: 全量构建 + 测试转绿**

Run: `cargo build --release && cargo test --release -p only-js generic_axis ; cargo test --release -p oj --test e2e generic`
Expected: PASS（单测 + e2e 两形态）

- [ ] **Step 7: Commit**

```bash
git add src/bridge/generic_axis.rs src/bridge/mod.rs src/bridge/bootstrap.js oj/src/serve_cmd.rs oj/tests/e2e.rs
git commit -m "feat(axis): 泛型轴通道——op_axis_call + axis() Proxy + 同名冲突 fail-fast

新轴开发范式：插件自报轴名（不在 9 个类型化轴内即泛型），宿主零改动。

unix@vip.qq.com ai"
```

---

### Task 6: oj_info — OjInfo 数据装配 + `oj info` CLI

**Files:**
- Modify: `oj/src/serve_cmd.rs`（`Registries` ~536、`build_registries`、新增 `OjInfo`/`assemble_for_info`，~700-830 区间）
- Create: `oj/src/info_cmd.rs`
- Modify: `oj/src/args.rs`（Commands enum ~223 加 `Info` 变体）
- Modify: `oj/src/main.rs`（dispatch ~18-25 区间）
- Modify: `oj/src/lib.rs`（`pub mod info_cmd;`）
- Test: `oj/src/info_cmd.rs` 内单测（打印快照）

**Interfaces:**
- Consumes: T3 `PluginInfo.unknown_axes`、T4 `LoadedConfig.top` 与 `unconsumed_sections`、T5 `Registries.generic`
- Produces（T7 依赖）:
  - `#[derive(serde::Serialize)] pub struct OjInfo { pub build: ..., pub abi: ..., pub plugins: Vec<PluginInfo>, pub backends: serde_json::Value, pub config: serde_json::Value, pub generic_axes: Vec<String>, pub unconsumed_sections: Vec<String> }`
  - `pub async fn assemble_for_info(cfg_path: &str) -> Result<OjInfo, String>`（load_with_extra → resolve_plugins_dir → assemble_plugins → build_registries；**不 connect、不监听**）
  - `impl OjInfo { pub fn to_text(&self) -> String }`（php -i 风格分段纯文本）

- [ ] **Step 1: 写失败测试**

`oj/src/info_cmd.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj text_report_has_all_sections`
Expected: FAIL（OjInfo 不存在）

- [ ] **Step 3: 实现 OjInfo + assemble_for_info + to_text**

`serve_cmd.rs`（装配域归 serve_cmd，与 Registries 同家）：

```rust
/// oj_info（phpinfo 对应物）单一事实源。两出口（CLI / JS ojInfo()）共用。
/// 只报告声明面：不 connect 库/broker；config 只出段名/键名，值一律不出。
#[derive(serde::Serialize)]
pub struct OjInfo {
    pub build: serde_json::Value,
    pub abi: serde_json::Value,
    pub plugins: Vec<only_js::bridge::plugin_loader::PluginInfo>,
    pub backends: serde_json::Value,
    pub config: serde_json::Value,
    pub generic_axes: Vec<String>,
    pub unconsumed_sections: Vec<String>,
}

/// `oj info` 与 JS ojInfo() 的共同装配面（评审 L8：assemble_plugins 只 init
/// 不 connect，天然可独立）。副作用 = 执行插件 init 代码，信任边界同 serve。
pub async fn assemble_for_info(cfg_path: &str) -> Result<OjInfo, String> {
    let config_dir = std::path::Path::new(cfg_path)
        .parent().map(|p| if p.as_os_str().is_empty() { std::path::Path::new(".") } else { p })
        .unwrap_or(std::path::Path::new(".")).to_path_buf();
    let loaded_cfg = only_js::config::load_with_extra(std::path::Path::new(cfg_path), &config_dir)
        .map_err(|e| format!("config: {e}"))?;
    let cfg = &loaded_cfg.config;
    let dir = resolve_plugins_dir(&config_dir, cfg.plugins_dir.as_deref())
        .map_err(|e| format!("plugins dir: {e}"))?;
    let host = only_js::bridge::plugin_loader::host_context();
    let loaded = match &dir {
        Some(d) if !cfg.plugins.is_empty() => {
            let mut entries: Vec<PluginManifestEntry> = cfg.plugins.keys()
                .map(|name| PluginManifestEntry { name: name.clone(), semver_pin: None })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            load_manifest(d, &entries, host, &|name, key| {
                plugin_cfg(cfg, &loaded_cfg.top, name, key, None)
                    .unwrap_or_else(|e| panic!("plugin_cfg: {e}"))
            }).map_err(|e| format!("plugins manifest: {e}"))?
        }
        Some(d) => load_scanned(d, host, &|name, key| {
                plugin_cfg(cfg, &loaded_cfg.top, name, key, None)
                    .unwrap_or_else(|e| panic!("plugin_cfg: {e}"))
            }).map_err(|e| format!("plugins scan: {e}"))?,
        None => vec![],
    };
    let reg = build_registries(cfg, &loaded)?;
    // backends 声明面：db schemes、blob 有/无、broker kinds、kv/auth/es/mail/ldap/mq 槽位。
    let backends = serde_json::json!({
        "db_schemes": { "declared": cfg.db.keys().collect::<Vec<_>>() },
        "blob_configured": !cfg.blob.is_none() || !cfg.blobs_empty_marker(),
        "kv_plugin": reg.kv.is_some(),
        "auth_plugin": reg.auth.is_some(),
        "mail_plugin": reg.mail.is_some(),
        "ldap_plugin": reg.ldap.is_some(),
        "es_plugin": reg.es.is_some(),
        "mq_plugins": reg.mq.iter().map(|(n, _)| n).collect::<Vec<_>>(),
        "bus_kinds": reg.bus.kinds_snapshot(), // 若 BusBackendRegistry 无此 API，改用 debug 名清单
        "dbs_registered": reg.dbs.registered_debug(),
    });
    let config = serde_json::json!({
        "sections": known_section_keys(&loaded_cfg.top),
        "unconsumed": unconsumed_sections(&loaded_cfg.top, cfg, &loaded),
    });
    Ok(OjInfo {
        build: serde_json::json!({
            "oj": env!("CARGO_PKG_VERSION"),
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "host_triple": only_js::bridge::ffi::triple(),
            "deno_core": env!("CARGO_PKG_VERSION"), // 实现时从 deno_core 依赖版本取（可用 cargo metadata 或硬编码注释）
            "v8": deno_core_v8_version(),
            "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            "config_path": cfg_path,
        }),
        abi: serde_json::json!({
            "abi_version": oj_plugin_ffi::ABI_VERSION,
            "host_fingerprint": oj_plugin_ffi::HOST_FINGERPRINT,
        }),
        plugins: loaded.iter().map(PluginInfo::from).collect(),
        backends,
        config,
        generic_axes: reg.generic.iter().map(|(a, _, _)| a.clone()).collect(),
        unconsumed_sections: unconsumed_sections(&loaded_cfg.top, cfg, &loaded),
    })
}
```

辅助函数：`known_section_keys(top) -> Vec<String>`（顶层键排序清单；实现放 serve_cmd.rs 或复用 config::known_top_level_keys + top.keys 并集）；`deno_core_v8_version()` = `deno_core::v8::V8::get_version().to_string()`（包一层以便测试替换）。注：`reg.blobs_empty_marker`/`kinds_snapshot`/`registered_debug` 等若不存在，用现有公共 API 取等价信息（BusBackendRegistry/DbBackendRegistry 的既有方法；实现时打开对应 struct 对齐，禁止为 oj_info 给它们加方法——宁可字段少一点）。**YAGNI 校准**：backends 能拿到什么放什么，缺失的字段砍。

`to_text()`：逐段 `## build` + `key: value` 行 + 插件表（name/semver/abi/desc 列对齐），plugins 段之后附 `unknown_axes` 与 `generic_axes`、`unconsumed_sections` 列表。

- [ ] **Step 4: CLI 接线**

`args.rs` Commands enum 加：

```rust
/// phpinfo 风格诊断：版本/ABI/插件/后端注册/配置段（值不出）
Info {
    /// 配置文件路径（相对 CWD）
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
},
```

`main.rs` dispatch 加 `Command::Info(a) => match oj::info_cmd::run(&a.config).await { ... }`（照 Serve 分支的错误打印形态）。`oj/src/lib.rs` 加 `pub mod info_cmd;`。

`info_cmd.rs`：

```rust
pub async fn run(config: &str) -> Result<(), String> {
    let info = crate::serve_cmd::assemble_for_info(config).await?;
    println!("{}", info.to_text());
    Ok(())
}
```

- [ ] **Step 5: 冒烟（零插件 + 有插件两形态）**

Run: `cargo build --release && ./bin/oj info -c sample/config.yaml`（经 `cargo xtask bin` 归置后）；再 `OJ_PLUGINS_DIR=... ./bin/oj info -c sample/config.yaml`（指向含 mini-generic 的目录）——肉眼确认：插件表出现 mini-generic、generic_axes 含 greet、config 段无数值泄漏。

- [ ] **Step 6: 单测转绿 + Commit**

Run: `cargo test --release -p oj text_report`
Expected: PASS

```bash
git add oj/src/serve_cmd.rs oj/src/info_cmd.rs oj/src/args.rs oj/src/main.rs oj/src/lib.rs
git commit -m "feat(info): OjInfo 装配面 + oj info CLI（phpinfo 对应物，声明面零连接）

unix@vip.qq.com ai"
```

---

### Task 7: JS ojInfo() — StableState 注入 + 三入口 e2e

**Files:**
- Modify: `src/bridge/mod.rs`（StableState/Extras 加 `oj_info` 字段；两处 StableState 构造）
- Modify: `src/bridge/plugins_op.rs`（加 `op_oj_info`，与 op_plugins 同文件——自省 op 同家）
- Modify: `src/bridge/bootstrap.js`（`globalThis.ojInfo`）
- Modify: `oj/src/serve_cmd.rs`（start() 把 OjInfo 注入 Extras）
- Test: `oj/tests/e2e.rs` 三入口用例；`src/bridge` 单测

**Interfaces:**
- Consumes: T6 `OjInfo`（serde Serialize → Value 固化）
- Produces: `StableState.oj_info: Arc<serde_json::Value>`（缺省空对象 `{}`）；`op_oj_info() -> serde_json::Value`；bootstrap `globalThis.ojInfo`

- [ ] **Step 1: 写失败 e2e**

`oj/tests/e2e.rs`（照既有 e2e 的 Bridge 构造形态；三个用例对应三入口——HTTP 池走 `Bridge::run_module`，tasks 与 `oj test` 路径若 e2e 覆盖成本过高则：**HTTP 池 e2e 必做**；tasks/oj-test 入口用「Bridge 构造即注入」的单元级断言替代（构造两个 Bridge 变体验证 StableState.oj_info 可用），理由：三入口共享同一 StableState 构造路径，注入点由编译器强制）。

```rust
// e2e（HTTP 池入口）：
#[tokio::test(flavor = "current_thread")]
async fn ojinfo_global_available_in_handler() {
    let mut bridge = /* 既有 e2e 的测试 Bridge 构造 */;
    bridge.set_oj_info_for_test(serde_json::json!({"abi": {"abi_version": oj_plugin_ffi::ABI_VERSION}}));
    let out = bridge
        .run_module("export default { get() { json.ok(ojInfo()); } }", "file:///tmp/m.js")
        .await
        .unwrap();
    let body = String::from_utf8(out.body).unwrap();
    assert!(body.contains("\"abi_version\""), "ojInfo() body: {body}");
}
```

（`set_oj_info_for_test`：Extras 注入点既有测试模式；若无对应模式，直接给测试用 Bridge 的 Extras 字段赋值。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --release -p oj --test e2e ojinfo_global`
Expected: FAIL（ojInfo 未定义 / op 不存在）

- [ ] **Step 3: 实现注入与 op**

`mod.rs`：`StableState` 加 `pub oj_info: Arc<serde_json::Value>`；`Extras` 加 `pub oj_info: Option<Arc<serde_json::Value>>`（缺省 None → 空对象，与 vars 同哲学）。两处 StableState 字面构造补字段（编译器强制找全）。

`plugins_op.rs`：

```rust
/// ojInfo() 数据源：装配期固化的 OjInfo（v0.1.54）。值在装配期已脱敏
///（config 只含段名/键名），op 层零泄漏面。
#[op2]
pub fn op_oj_info(state: Rc<RefCell<OpState>>) -> serde_json::Value {
    let s = state.borrow();
    s.borrow::<crate::StableState>().oj_info.as_ref().clone()
}
```

（`#[op2]` 返回 serde_json::Value 需 `#[serde]` 标注与否照 op_plugins 的既有写法——打开 plugins_op.rs 对齐。）`bridge_ext` ops 列表注册。

`bootstrap.js`（globalThis 装配区，ASCII）：

```js
// ojInfo(): assembly-frozen diagnostics (see `oj info`). Values are names only.
globalThis.ojInfo = () => op_oj_info();
```

`serve_cmd.rs` `start()`：装配 OjInfo（`assemble_for_info` 的内部非 CLI 版本——把 T6 的装配体抽为 `assemble_ojinfo(cfg, &loaded_cfg, &loaded, &reg) -> OjInfo` 供 CLI 与 start 共用，DRY）→ `serde_json::to_value` → Extras 注入。

- [ ] **Step 4: 测试转绿 + 全 workspace 回归**

Run: `cargo test --release -p oj --test e2e ojinfo ; cargo test --release --workspace`
Expected: PASS（全 workspace——StableState 字段新增会逼出所有构造点遗漏）

- [ ] **Step 5: Commit**

```bash
git add src/bridge/mod.rs src/bridge/plugins_op.rs src/bridge/bootstrap.js oj/src/serve_cmd.rs oj/tests/e2e.rs
git commit -m "feat(info): JS ojInfo()——StableState 注入 + op_oj_info

与 oj info CLI 同源（assemble_ojinfo 共用）。框架不提供公共 HTTP 端点。

unix@vip.qq.com ai"
```

---

### Task 8: tools/plugin-template — 泛型轴插件开发模板

**Files:**
- Create: `tools/plugin-template/`（Cargo.toml、src/lib.rs、README.md）

**Interfaces:**
- Consumes: T1 宏/GenericVtable、T5 JS 调用形态
- Produces: 可复制起手的骨架 crate（不进 workspace members——模板非构建目标；README 说明 `cp -r` 起手）

- [ ] **Step 1: 骨架（无独立测试——以「模板可被 cargo build 通过」为验收）**

`Cargo.toml`：cdylib，path 依赖 `../../oj-plugin-ffi`（照 plugins/ 下任一插件）。

`src/lib.rs`：最小泛型轴插件 `cache` 形态——两个 op（`get`/`set`，进程内 HashMap 即可，仅作范式载体）、catch_future 包装（照 oj-db-mysql 的 future 助手）、`oj_plugin_entry!(init, config: "cache", cache => &VT)`（演示 config key 语法）、init 里对 cfg 做键白名单校验演示（未知键 `RResult::Err` fail-fast，ldap 哲学）。

`README.md`（中文）：
- 范式四契约：轴名小写且避开 9 个保留名（es/db/blob/bus/kv/auth/mq/mail/ldap）；vtable 必须静态可寻址；vtable 方法必须 catch_future 收敛 panic；错误经 future Err 透传。
- 配置：config key 声明 + 用户写顶层段（示例 yaml）。
- JS 调用：`axis("cache").get("k")`。
- 构建：`cargo xtask plugin <name>`；预检：`cargo xtask plugin <name> --check`。
- 宿主最低版本提示（desc 里注明，评审 L6 已知限制）。

- [ ] **Step 2: 验收**

Run: `cargo build --release --manifest-path tools/plugin-template/Cargo.toml`
Expected: PASS（独立 manifest 构建通过）

- [ ] **Step 3: Commit**

```bash
git add tools/plugin-template
git commit -m "feat(template): tools/plugin-template 泛型轴插件开发范式

unix@vip.qq.com ai"
```

---

### Task 9: 文档红线 + v0.1.54 bump

**Files:**
- Modify: `CHANGELOG.md`、`Cargo.toml`（根 + oj/ 版本）、`docs/devkit/` 四件、`docs/api/`（新增 ojinfo 篇）、`docs/plugins/plugin-architecture.md`、`docs/plugins/plugin-development.md`

**Interfaces:**
- Consumes: T1–T8 全部用户可见变更
- Produces: v0.1.54 发行面

- [ ] **Step 1: 版本 bump + CHANGELOG**

根 `Cargo.toml` 与 `oj/Cargo.toml` 的 version → `0.1.54`（其余 crate 按仓库既有 bump 惯例跟随）。`CHANGELOG.md` 顶部加 v0.1.54 段：`feat: 插件轴清单自报 + 泛型轴通道（新轴零宿主改动）+ 插件自报配置键 + oj info CLI / JS ojInfo()`（含「插件作者向/运维向」两小节，说明宏双发与旧插件兼容）。

- [ ] **Step 2: devkit 四件同步（CLAUDE.md 红线）**

- `api-manual.md`：新增 `axis(name)` 与 `ojInfo()` 章节 + 错误/限制表（unknown axis 错误文案、泛型轴信任边界、ojInfo 无公共端点）；
- `SKILL.md`：陷阱速查加三条——泛型轴名避开保留名；config key 段未配置给 `{}`；unconsumed section 告警含义；
- `scenarios.md`：照抄场景「写一个泛型轴插件（cache）」；
- `README.md`：版本与能力列表对齐。
然后 `cargo xtask build` 归置 `bin/devkit/`，`cargo test --release -p xtask` 的 devkit 契约用例校验。

- [ ] **Step 3: docs/api + plugins 文档**

`docs/api/20-ojinfo.md`：ojInfo()/oj info 篇（结构照既有 19 篇；内容同源 spec Part 4）；`docs/api/README.md` 索引加行。
`docs/plugins/plugin-architecture.md`：自报清单 + 双发/回退 + 泛型轴通道 + config key 通路机制章节。
`docs/plugins/plugin-development.md`：新轴范式指向 `tools/plugin-template`。
`docs/api/05-plugins.md` 若有轴清单描述，同步自报机制一句。

- [ ] **Step 4: 全量终验**

Run: `cargo fmt --check && cargo clippy --release --all-targets -- -D warnings && cargo test --release --workspace && cargo xtask smoke --bin bin/oj`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "release: v0.1.54——插件自报轴/配置 + 泛型轴通道 + oj_info

unix@vip.qq.com ai"
```

---

## Self-Review 记录

- **Spec 覆盖**：Part 1→T1/T3；Part 2→T5；Part 3→T4；Part 4→T6/T7；模板→T8；文档红线→T9；评审 findings 全部落位（两-pass=T4 Step 3；RVec=T1；宏双发=T1；cfg_for=T4 Step 4；mini-legacy=T2；AXES 降级=T3；撞名消解=T4 Step 4 第 2 级；ojStringify/then=T5 Step 5；三入口=T7 Step 1；已知限制=T8 README）。
- **占位符扫描**：`backends` JSON 中 `blob_configured`/`kinds_snapshot`/`registered_debug` 为 YAGNI 校准点，已注明「用现有公共 API 取等价信息，缺则砍」——实现者据此自行定夺，非 TBD。
- **类型一致性**：`LoadedPlugin.generic_axes`（T3 产）= `Vec<(String, &'static GenericVtable)>`，T5 消费同名同型；`config_key: Option<String>`（T3）→ `cfg_for(&str, Option<&str>)`（T4）→ `plugin_cfg(..., config_key, ...)` 链路一致；`OjInfo`（T6）→ `StableState.oj_info: Arc<serde_json::Value>`（T7）经 `serde_json::to_value` 固化。
