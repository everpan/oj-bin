//! oj-3 L1 实测：WebAssembly.instantiate 与 wasm-bindgen 胶水 Web API
//! （atob/btoa、crypto.getRandomValues）在 oj runtime 可跑。
//! 上游缺口 PR oj-3 的验收用例；v0.1.30 落地。

use only_js::bridge::{Bridge, DataAccessor, Extras, InMemoryAccessor, InMemoryKV, SchemaRegistry};
use std::collections::HashMap;
use std::sync::Arc;

fn bridge() -> Bridge {
    Bridge::with_dbs_and_loader(
        HashMap::from([(
            "default".to_string(),
            Arc::new(InMemoryAccessor::new()) as Arc<dyn DataAccessor>,
        )]),
        Arc::new(InMemoryKV::new()),
        SchemaRegistry::new(),
        false,
        None,
        Extras::default(),
    )
}

/// (module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))
const ADD_WASM: &[u8] = &[
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
    0x01, 0x07, 0x01, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, // type: (i32, i32) -> i32
    0x03, 0x02, 0x01, 0x00, // function: type 0
    0x07, 0x07, 0x01, 0x03, 0x61, 0x64, 0x64, 0x00, 0x00, // export "add" -> func 0
    0x0a, 0x09, 0x01, 0x07, 0x00, 0x20, 0x00, 0x20, 0x01, 0x6a, 0x0b, // code body
];

#[tokio::test(flavor = "current_thread")]
async fn wasm_instantiate_and_glue_apis() {
    let b = bridge();
    let src = format!(
        r#"(async () => {{
            const m = await WebAssembly.instantiate(new Uint8Array({:?}), {{}});
            const add = m.instance.exports.add;
            const atobOk = atob("aGk=") === "hi";
            const b64 = btoa("hello");
            const roundtrip = atob(b64) === "hello";
            const v = new Uint8Array(16);
            const ret = crypto.getRandomValues(v);
            const sameView = ret === v && v.byteLength === 16;
            const w = new Uint8Array(16);
            crypto.getRandomValues(w);
            let differ = false;
            for (let i = 0; i < 16; i++) if (w[i] !== v[i]) {{ differ = true; break; }}
            let err = null;
            try {{ atob("a="); }} catch (e) {{ err = String(e); }}
            let typeErr = null;
            try {{ crypto.getRandomValues([1, 2]); }} catch (e) {{ typeErr = String(e); }}
            json.ok({{ add: add(1, 2), atobOk, b64, roundtrip, sameView, differ, err, typeErr }});
        }})().catch((e) => json.fail(500, String(e)));"#,
        ADD_WASM
    );
    let cap = b.run_with(&src, Default::default()).await.unwrap();
    assert_eq!(
        cap.status,
        200,
        "body={}",
        String::from_utf8_lossy(&cap.body)
    );
    let v: serde_json::Value = serde_json::from_slice(&cap.body).unwrap();
    let d = &v["data"];
    assert_eq!(d["add"], 3, "{v}");
    assert_eq!(d["atobOk"], true, "{v}");
    assert_eq!(d["b64"], "aGVsbG8=", "{v}");
    assert_eq!(d["roundtrip"], true, "{v}");
    assert_eq!(d["sameView"], true, "{v}");
    assert_eq!(d["differ"], true, "{v}");
    assert!(
        d["err"].as_str().unwrap().contains("InvalidCharacterError"),
        "{v}"
    );
    assert!(d["typeErr"].as_str().unwrap().contains("TypeError"), "{v}");
}
