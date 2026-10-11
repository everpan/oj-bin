// `--` 之后的参数经 globalThis.args 注入（spec §3.3）。
console.log("ARGS=" + (globalThis.args || []).join("|"));
