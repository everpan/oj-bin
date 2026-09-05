# oj 插件开发指南（第三方）

插件系统把外部分布式后端（db 方言、blob 驱动、bus 消息、kv 存储、es 引擎）抽为
**动态链接库**，宿主按平台目录扫描/按清单装配。本文档面向第三方插件作者：
FFI 契约、ABI_VERSION 纪律、开发/构建/调试全流程。宿主侧装配语义见
`dev-manual.md` §9，配置见 `user-manual.md` §3。

## 1. 一句话模型

- 插件 = 一个 **cdylib**，导出 `oj_plugin_abi_version` + `oj_plugin_init`，以及**每提供一轴**
  一个 `oj_plugin_axis_<name>` 符号（全部由入口宏生成，禁止手写 `#[no_mangle]` 绕过）。
- 宿主与插件只通过 `oj-plugin-ffi` crate 的类型跨边界（**唯一允许**）；tokio/tracing 等
  运行时类型绝不过线。
- `ABI_VERSION`（u32，**严格相等**）是唯一硬门禁；构建指纹（rustc/契约 crate 版本/triple）
  仅诊断，不匹配告警不拒绝。
- 注册是**按轴 dlsym 探测**：宿主 `init` 返回后（abi 门禁通过）对探测表 `AXES`（es/db/
  blob/bus/kv/auth）逐轴 `dlsym("oj_plugin_axis_<axis>")`——查到符号即该插件提供该轴
  （符号返回静态 vtable 指针），**缺符号 = 不提供该轴**。vtable 指向的静态表在 init 时
  就绪即可；加新轴/加插件不再改动共享槽位结构。

## 2. 契约 crate 类型面

所有跨界类型都在 `oj-plugin-ffi`（宿主与插件依赖同一 crate，保证布局一致）：

| 类型 | 说明 |
|------|------|
| `RString` | stabby `String`，`repr(C)`，`&s[..]` 取 `&str` |
| `RBytes` | stabby `Vec<u8>` |
| `RResult<T,E>` | stabby `Result`；构造用 `RResult::Ok(v)` / `Err(e)`，消费侧 `std::result::Result::from(r)` 转标准 Result 后 match（**不能模式匹配**，`?` 对 stabby Result 无效） |
| `RArc<T>` | stabby `Arc`（`HostContext` 的载体） |
| `FfiFuture` | `{ state, poll, take, free }` 异步句柄（见 §4） |
| `HostContext` | 宿主回调集：`log(level, msg)`、`deliver(topic, payload)` |
| `PluginDescriptor` | `{ name, semver, abi_version, fingerprint, desc }`（见 §7 自描述） |

各轴 vtable 见 `oj-plugin-ffi/src/{es,db,blob,bus,kv,auth}.rs`。方法签名形态：
同步函数返回 `FfiFuture`；`connect` 产 handle（`{"handle":N}` JSON），`close` 释放。

各轴 vtable 见 `oj-plugin-ffi/src/{es,db,blob,bus,kv,auth}.rs`。方法签名形态：
同步函数返回 `FfiFuture`；`connect` 产 handle（`{"handle":N}` JSON），`close` 释放。

**auth 轴特例**（`AuthGuardVtable`，唯一同步轴，不返回 `FfiFuture`）：`verify(path_no_base,
authorization) -> RResult<RString, RString>`，ok 值 JSON `null` = 匿名路径放行、对象 =
注入 `http.user`（`{"id","roles","claims"}`），Err = 401 消息。请求级热路径，参考第一方
`plugins/oj-auth`。

## 3. ABI_VERSION 纪律

- `ABI_VERSION` 当前 **7**（按轴 dlsym 契约）。**加轴零破坏**——ABI 规则表：

| 变更 | 是否 bump ABI |
|------|--------------|
| 新增一条轴（新 `oj_plugin_axis_<name>` 符号 + 新 vtable 类型） | **否**（探测式，缺符号 = 不提供） |
| 既有轴 vtable 的 `repr(C)` 形状变更（加方法、改签名） | **是** |
| `PluginDescriptor` / `HostContext` 字段变更 | **是** |
| cfg JSON 新增可选键（schema 归插件所有） | **否**（向后兼容演进走 cfg 字段） |

- 宿主严格 `plugin_abi == ABI_VERSION` 才加载；不相等 → `plugin ABI mismatch: plugin=N host=M`。
- 插件构建须对当前 `oj-plugin-ffi` 版本；升级宿主与插件顺序：**先升插件到新 ABI 并验证，
  再升宿主**（或同版本原子升级）。`cargo xtask plugin <name> --check` 复用宿主 `PluginLoader`
  做 ABI/身份/semver/按轴符号预检（输出 desc 与 provided axes）。

## 4. FfiFuture 异步桥（唯一异步路径）

插件内自建 tokio runtime（`#[tokio::main]` 不经用——插件 init 在宿主线程调用）。推荐形态
（见第一方插件 `oj-kv-redis` / `oj-blob-s3`）：

```rust
struct CallState {
    rx: tokio::sync::oneshot::Receiver<Result<Vec<u8>, String>>,
    result: Option<Result<Vec<u8>, String>>,
}
// poll: try_recv → 1 成功 / -1 错误 / 0 未就绪（宿主 yield_now 轮询）
// take: result.take() → RResult<RBytes, RString>
// free: Box::from_raw 释放（幂等，null 安全）
```

`spawn_call`：异步工作 `spawn` 到插件 runtime，oneshot 收结果，返回 `FfiFuture`。

**返回编码约定**：结构化返回值走 JSON 字节（如 `get` → `"value"`/`null`、`expire` → `true`，
`connect` → `{"handle":N}`）；空操作返回空字节。时长跨线以秒计（Redis EXPIRE 整秒契约）。

## 5. 插件须自包含

- 插件依赖自己 + 各自后端 SDK（sqlx、rdkafka、object_store、redis、reqwest…）——
  **不依赖宿主 crate**，可脱离宿主 workspace 单独编译、独立仓库发版。
- 系统级依赖（如 rdkafka 的 librdkafka、openssl）要么静态/vendored 链接，要么在插件
  README **显式声明**运行环境要求（部署侧 glibc 基线见 CI 矩阵）。
- **Windows 上 `oj-bus-kafka` 的构建特例**：`rdkafka` 默认特性（ssl/sasl/zstd/lz4/libz）
  依赖 OpenSSL/zlib/SASL 等原生库，Windows CI runner 不提供，曾导致 `rdkafka-sys` 构建失败。
  现 `plugins/oj-bus-kafka/Cargo.toml` 以 `[target.'cfg(windows)'.dependencies]` 关闭 rdkafka
  默认特性、仅保留 `tokio` 并显式开启 `cmake-build`：`rdkafka-sys` 用 cmake 从源码构建
  librdkafka（不链接任何外部库），`windows-latest`（自带 cmake + MSVC）即可编译。Windows 非
  部署目标，故该平台 kafka 插件不携带 SSL/SASL；Linux/macOS（部署目标）保留完整默认特性。
  插件源码仅用核心异步 API（`ClientConfig`/`StreamConsumer`/`FutureProducer`/`Message`），无需改动。
- 共享逻辑不抽公共运行时——复制可接受的（决策记录：接受复制）。

## 6. panic=unwind（禁止 abort）

- 插件 **必须** `panic=unwind` profile（不得覆盖为 abort）。入口宏内建
  `catch_unwind(AssertUnwindSafe(..))`，init 内 panic 收敛为 `RResult::Err`，**不 unwind 跨界**。
- 运行期异步任务内 panic 会中止该任务（不拖垮插件进程——插件与宿主同进程，插件自身线程
  被 panic 终止；宿主经 panic hook 归因）。
- 宿主的 panic hook（装配首个插件前安装）：输出 `[oj-plugin] panic while loading plugin
  '<name>' (host fingerprint: …)` 后透传原始 panic。

## 7. 入口宏用法（按轴声明）

```rust
use oj_plugin_ffi::{ABI_VERSION, HostContext, PluginDescriptor, RArc, RResult, RString,
    oj_plugin_entry};

fn init(host: RArc<HostContext>, cfg: RString) -> RResult<PluginDescriptor, RString> {
    // 幂等：重复 init 返回已有 descriptor（OnceLock/get_or_init 兜底）
    if PLUGIN.get().is_some() { return RResult::Ok(descriptor()); }
    // init 建立插件 runtime + 单例状态；cfg 为装配期配置（无则忽略）
    let _ = PLUGIN.set(state);
    RResult::Ok(descriptor())
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name: RString::from("my-plugin"),
        semver: RString::from("0.1.0"),
        abi_version: ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        // 必填自描述：宿主收集后在 GET {base}/plugins 公开（运维/监控辨识用）。
        desc: RString::from("db 轴 mysql 插件：sqlx 单方言连接工厂"),
    }
}

// init 后宿主对 AXES 逐轴 dlsym：提供哪个轴就在宏尾声明哪个轴（轴标识强制小写）。
// 缺轴声明 = 不导出该符号 = 不提供该轴；多轴用逗号并列。
oj_plugin_ffi::oj_plugin_entry!(init, db => &VTABLE);
// oj_plugin_ffi::oj_plugin_entry!(init, kv => &KV_VTABLE, auth => &AUTH_VTABLE);  // 多轴
// oj_plugin_ffi::oj_plugin_entry!(init);                                          // 零轴（纯自描述）
```

- 命名：插件 `descriptor.name` 决定存放文件名（`lib<name>.dylib` / `<name>.dll`）；crate 名
  `oj-<name>` → 构建产物 `liboj_<name>.<ext>`。扫描模式按文件名加载、严格清单模式按
  `plugins:` 键核对名字与 `@semver` pin。
- 插件不得手写 `#[no_mangle] pub extern "C" fn oj_plugin_*` 绕过宏（descriptor 内 abi_version
  二次校验兜底）。
- **自描述**：`descriptor.desc` 必填（一句人话，写清轴 + 驱动）。宿主在装配后收集全部插件
  的 `{name, semver, abi_version, fingerprint, description}`，经公共端点
  `GET {base}/plugins`（ok 信封）公开，供运维/监控辨识当前进程装配了什么。

### 7.1 插件配置：`plugins:` 一段三用

`config.yaml` 的 `plugins:` 段统一为 **map**（旧 list 写法 `plugins: [a, b]` 已废弃，
解析报错 fail-fast）：

```yaml
plugins:
  kv-redis: {}                        # 键 = 要加载的插件名；空对象 = 透传回落轴适配器
  auth:
    jwt_secret: "change-me"           # 非空对象 = 原样透传为该插件 init cfg（跳过回落）
  my-plugin:
    endpoint: "http://127.0.0.1:9200" # schema 归插件所有（init 校验，非法 Err fail-fast）
```

- **键 = 严格清单**：非空 map 只装配列出的插件（沿用清单门禁：缺文件/身份/`@semver`
  pin 不符 fail fast）；**缺省/空 map = 扫描模式**（加载 `plugins_dir` 全部）。
- **值 = cfg 透传**：非空对象原样透传给插件 `init(cfg)`；**空对象 `{}` = 跳过透传，回落
  轴适配器**（第一方插件由宿主把对应 config 段映射成 cfg，如 es→`cfg.es`、auth→
  `cfg.auth`；无适配器的轴回落 `"{}"`）。

## 8. 构建与调试

```bash
# 本地构建 + 拷入 bin/plugins/<host-triple>/（xtask 用 release）
cargo xtask bin                                # 主程序 → bin/oj
cargo xtask plugin <name>
cargo xtask plugin <name> --check   # PluginLoader 预检（ABI/身份/semver/按轴符号，打印 desc + provided axes）

# 运行宿主（dev），加载扫描
cargo run -p oj -- serve ...
```

**panic 归因**：panic hook 已输出插件名 + 宿主指纹。若需源码级调试，在 `bin/plugins/<triple>/`
旁保留对应构建的符号文件（`symbols/` 目录），`lldb`/`gdb` 附加后 `bt` 定位。

### 8.1 Windows 构建踩坑记录

**xtask.exe 被锁（Access is denied, os error 5）**（2026-08-28，已修复）：
`cargo run -p xtask -- plugin <name>` 以 `-p` 归一化链接并启动 `xtask.exe`；xtask 内部再调
`cargo build --workspace --release` 时，`--workspace` 与 `-p` 的 feature 归一化不同，cargo 会
认为 xtask 需要 relink，尝试删除**运行中的** `xtask.exe` —— Windows 锁定运行中的 exe，报
`os error 5`。修复：xtask 的 `build_workspace_release()` 固定带 `--exclude xtask`
（`tools/xtask/src/main.rs`）。**不可移除**该参数：运行中的程序不得要求 cargo 重编自身；
xtask 也不是发行产物（`bin/` 只放 oj + 插件），排除不影响其余成员的 feature 归一化。

**LNK4098（defaultlib 'libcmt.lib' conflicts）警告**：良性，无需处理。预编译 rusty_v8
静态库按 `/MT`（静态 CRT）构建，与 Rust 默认 `/MD`（动态 CRT）混链产生该警告，链接实际
成功。勿为消音加 `/NODEFAULTLIB:libcmt` —— 会改变 CRT 解析方式，风险大于噪音。

## 9. 第一方插件清单（参照模板）

| 插件 | 轴 | 驱动 | 迁移来源 |
|------|-----|------|---------|
| `oj-es` | es | reqwest | core `bridge/es.rs` |
| `oj-db-mysql` / `oj-db-postgres` | db | sqlx Any 单方言 | core `accessor_sqlx.rs` |
| `oj-blob-s3` | blob | object_store aws | core `bridge/blob.rs` S3Blob |
| `oj-bus-kafka` / `oj-bus-rabbitmq` | bus | rdkafka / lapin | core `bridge/broker/` |
| `oj-kv-redis` | kv | redis | core `bridge/kv.rs` RedisKV |
| `oj-auth` | auth | jsonwebtoken | core `bridge/auth.rs`（守卫；auth 端点已 JS 化） |

> 所有第一方插件源码统一位于 `plugins/`；构建产物（cdylib）归置 `bin/plugins/<triple>/`，由 `.gitignore` 忽略。
