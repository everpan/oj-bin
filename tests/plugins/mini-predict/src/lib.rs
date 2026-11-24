//! predict 网关 e2e 夹具：轴名 "predict" 经宿主 `predict:` 段收编（不进 `axis()`
//! 公开面，只可经 `predict.run` 网关触达）。单 op：
//! - `run`：echo 引擎——回 `{result:{echoPqlLen, sources}, ticket}`（ticket 原样
//!   回显，供网关回程校验）；PQL 以 `SLEEP<n>` 开头 → **异步**睡 n 毫秒再回
//!   （网关 timeout 腿——call 必须快速返回 FfiFuture，同步睡会阻塞网关线程令
//!   tokio 计时器失效）；PQL 以 `FAIL_ECHO …` 开头 → 错误消息回显 PQL 原文
//!   （网关脱敏腿）；`FAIL…` → 干净错误。
//!
//! 异步纪律照抄 oj-ldap/oj-db-jdbc 形态：自有 tokio runtime + spawn_ffi_future。

use oj_plugin_ffi::{FfiFuture, HostContext, RArc, RResult, RString, oj_plugin_entry};
use serde_json::Value;

static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    // multi_thread（1 worker）：spawn 的任务要在自有 worker 线程上自驱——
    // current_thread 没人 block_on 驱动，任务永不前进（宿主只会等到超时）。
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("mini-predict tokio runtime")
    })
}

fn engine_run(obj: Value) -> FfiFuture {
    let pql = obj
        .get("pql")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ticket = obj
        .get("ticket")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let sources = obj
        .get("dataScope")
        .and_then(|d| d.get("sources"))
        .cloned()
        .unwrap_or(Value::Null);
    oj_plugin_ffi::spawn_ffi_future(rt(), async move {
        // timeout 腿：PQL 形如 "SLEEP5000…" → 异步睡（网关侧 timeout_ms 截断）。
        if let Some(rest) = pql.strip_prefix("SLEEP") {
            let ms: u64 = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
        // 引擎错误腿（PQL 暗号——网关只转发 pql/dataScope/ticket，无旁路 args）：
        // "FAIL_ECHO…" → 错误消息回显**整条 PQL 原文**（网关脱敏腿的威胁模型：
        // 引擎把请求内容拼进错误文案）；"FAIL…" → 干净错误。
        if pql.starts_with("FAIL_ECHO") {
            return Err(format!("engine boom while evaluating: {pql}"));
        }
        if pql.starts_with("FAIL") {
            return Err("engine boom".to_string());
        }
        Ok(serde_json::json!({
            "result": { "engine": "mini", "echoPqlLen": pql.chars().count(), "sources": sources },
            "ticket": ticket,
        })
        .to_string()
        .into_bytes())
    })
}

extern "C" fn call(op: RString, args: RString) -> FfiFuture {
    // 网关 dispatch 的 args 是**裸对象**（{pql, dataScope, ticket}）——非 axis() 通道
    // 的数组包装约定。call 本体快速返回（耗时工作在插件 runtime 异步跑）。
    oj_plugin_ffi::catch_future(|| {
        let obj: Value = serde_json::from_slice(args[..].as_bytes()).unwrap_or(Value::Null);
        match &op[..] {
            "run" => engine_run(obj),
            other => {
                oj_plugin_ffi::ready_err(format!("mini-predict: unknown op '{other}' (known: run)"))
            }
        }
    })
}

static VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable { call };

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(oj_plugin_ffi::PluginDescriptor {
        name: RString::from("predict"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("predict 网关 e2e 夹具（echo 引擎，异步 sleep/FAIL 暗号）"),
    })
}

oj_plugin_entry!(init, generic(predict) => &VT);
