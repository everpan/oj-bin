//! oj-predict：REQ-PREDICT（v0.1.145）预测执行引擎插件——泛型轴 `predict`。
//!
//! 网关（宿主 `bridge::predict` 的 `op_predict_run`）负责合规三闸（dataScope 白名单
//! / consent 授权表 / 审计）并按 `predict.axis` **收编**本轴——`axis()` 公开面不可达，
//! 只可经 `predict.run` 触达。本插件只实现引擎契约：
//!
//! - `run`（args 为网关下发的**裸对象** `{pql, dataScope:{sources,maxRows}, ticket}`）
//!   → `{"result":{"engine":"mock","rows":[{period,value}…],"deterministic":true},
//!   "ticket":<回显>}`。rows 由 sha256(pql) 播种的线性趋势生成（同 pql 恒同值），
//!   条数 = dataScope.maxRows（1..=100，缺省 3）——**确定性 mock**：无外呼、不触
//!   租户数据，供离线联调与部署冒烟。真实引擎（KumoRFM 等）由部署侧按本契约自备
//!   插件实现（上游裁定具体形态）。
//! - 未知 op → ready Err 列合法 op。
//!
//! descriptor name 必须是**插件名** `predict`（非 crate 名 oj-predict）——宿主按
//! 文件 stem/清单键索引（oj-ldap 先例，测试钉死）。

use oj_plugin_ffi::{FfiFuture, HostContext, RArc, RResult, RString, oj_plugin_entry};
use serde_json::Value;
use sha2::{Digest, Sha256};

extern "C" fn call(op: RString, args: RString) -> FfiFuture {
    // mock 引擎全同步（确定性纯函数）——call 快速返回 ready future，无自有 runtime。
    oj_plugin_ffi::catch_future(|| {
        let obj: Value = serde_json::from_slice(args[..].as_bytes()).unwrap_or(Value::Null);
        match &op[..] {
            "run" => run(obj),
            other => oj_plugin_ffi::ready_err(format!(
                "oj-predict: unknown op '{other}' (known: run)"
            )),
        }
    })
}

/// 引擎契约 run：确定性线性趋势预测。rows 条数取 dataScope.maxRows（1..=100，缺省 3）。
fn run(obj: Value) -> FfiFuture {
    let pql = obj.get("pql").and_then(Value::as_str).unwrap_or("");
    if pql.trim().is_empty() {
        return oj_plugin_ffi::ready_err("oj-predict: pql must be a non-empty string");
    }
    let ticket = obj.get("ticket").and_then(Value::as_str).unwrap_or("");
    let horizon = obj
        .get("dataScope")
        .and_then(|d| d.get("maxRows"))
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .clamp(1, 100) as usize;
    // sha256(pql) 前 8 字节播种：基线 + 斜率都从种子派生（同 pql 恒同值）。
    let digest = Sha256::digest(pql.as_bytes());
    let seed = u64::from_be_bytes(digest[0..8].try_into().expect("8 bytes"));
    let base = (seed % 10_000) as i64;
    let slope = ((seed >> 32) % 900) as i64 + 1;
    let rows: Vec<Value> = (0..horizon)
        .map(|i| {
            serde_json::json!({
                "period": format!("t+{i}"),
                "value": base + slope * i as i64,
            })
        })
        .collect();
    oj_plugin_ffi::ready_ok(
        serde_json::json!({
            "result": {
                "engine": "mock",
                "deterministic": true,
                "rows": rows,
            },
            "ticket": ticket,
        })
        .to_string()
        .into_bytes(),
    )
}

static PREDICT_VT: oj_plugin_ffi::GenericVtable = oj_plugin_ffi::GenericVtable { call };

fn descriptor() -> oj_plugin_ffi::PluginDescriptor {
    oj_plugin_ffi::PluginDescriptor {
        name: RString::from("predict"),
        semver: RString::from(env!("CARGO_PKG_VERSION")),
        abi_version: oj_plugin_ffi::ABI_VERSION,
        fingerprint: RString::from(oj_plugin_ffi::HOST_FINGERPRINT),
        desc: RString::from("REQ-PREDICT 预测引擎（内置确定性 mock；外部引擎按契约自备插件）"),
    }
}

fn init(
    _host: RArc<HostContext>,
    _cfg: RString,
) -> RResult<oj_plugin_ffi::PluginDescriptor, RString> {
    RResult::Ok(descriptor())
}

oj_plugin_entry!(init, config: "predict", generic(predict) => &PREDICT_VT);

#[cfg(test)]
mod tests {
    use super::*;

    fn call_str(op: &str, args: &str) -> Result<Vec<u8>, String> {
        let f = (PREDICT_VT.call)(RString::from(op), RString::from(args));
        let code = (f.poll)(f.state);
        assert_ne!(code, 0, "mock 引擎恒 ready（Err 帧码 -1 / Ok 帧码 1）");
        let r = match std::result::Result::from((f.take)(f.state)) {
            // Ok 帧码 1；Err（含 -1）经 RResult::From 转 std Result::Err。
            Ok(b) => Ok(b.iter().copied().collect::<Vec<u8>>()), // stabby Vec → std Vec
            Err(e) => Err(e[..].to_string()),
        };
        (f.free)(f.state);
        r
    }

    /// descriptor 身份必须是插件名 `predict`（非 crate 名）——宿主按 stem/清单键索引。
    #[test]
    fn descriptor_name_is_plugin_name_not_crate_name() {
        assert_eq!(&descriptor().name[..], "predict");
    }

    /// run 契约：result + ticket 回显；rows 条数缺省 3。
    #[test]
    fn run_echoes_ticket_and_defaults_horizon() {
        let out = call_str(
            "run",
            r#"{"pql":"FORECAST revenue","ticket":"gt-abc","dataScope":{"sources":["db:crm.orders"]}}"#,
        )
        .expect("run ok");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["ticket"], "gt-abc");
        assert_eq!(v["result"]["engine"], "mock");
        assert_eq!(v["result"]["rows"].as_array().map(Vec::len), Some(3));
        assert_eq!(v["result"]["deterministic"], true);
    }

    /// 确定性：同 pql 两次同值；不同 pql 值不同（播种生效）。
    #[test]
    fn run_is_deterministic_per_pql() {
        let a1 = call_str("run", r#"{"pql":"P-A","ticket":"t"}"#).unwrap();
        let a2 = call_str("run", r#"{"pql":"P-A","ticket":"t"}"#).unwrap();
        let b = call_str("run", r#"{"pql":"P-B","ticket":"t"}"#).unwrap();
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    /// maxRows 驱动条数（1..=100 clamp）。
    #[test]
    fn run_honors_max_rows_with_clamp() {
        let one = call_str("run", r#"{"pql":"X","ticket":"t","dataScope":{"maxRows":1}}"#).unwrap();
        let v: Value = serde_json::from_slice(&one).unwrap();
        assert_eq!(v["result"]["rows"].as_array().map(Vec::len), Some(1));
        let big = call_str("run", r#"{"pql":"X","ticket":"t","dataScope":{"maxRows":99999}}"#).unwrap();
        let v: Value = serde_json::from_slice(&big).unwrap();
        assert_eq!(v["result"]["rows"].as_array().map(Vec::len), Some(100));
    }

    /// 空 pql → 结构化 Err（引擎契约：非空字符串）。
    #[test]
    fn run_rejects_empty_pql() {
        let e = call_str("run", r#"{"pql":"  ","ticket":"t"}"#).expect_err("empty pql must fail");
        assert!(e.contains("non-empty"), "{e}");
    }

    /// 未知 op → ready Err 列合法 op（泛型通道协议在插件边界）。
    #[test]
    fn unknown_op_lists_known_ops() {
        let e = call_str("forecast", r#"{}"#).expect_err("unknown op must fail");
        assert!(e.contains("unknown op 'forecast'"), "{e}");
        assert!(e.contains("run"), "{e}");
    }
}
