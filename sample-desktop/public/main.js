// 零 Rust 依赖：用 Tauri 注入的全局 window.__TAURI__ 调 oj_dispatch。
// Tauri v2 的 invoke 在 window.__TAURI__.core.invoke（v1 才是顶层）。
const tauri = window.__TAURI__;
const invoke =
  (tauri && tauri.core && tauri.core.invoke) || (tauri && tauri.invoke);

const out = document.getElementById("out");

document.getElementById("call").addEventListener("click", async () => {
  out.textContent = "请求中…";
  try {
    const resp = await invoke("oj_dispatch", {
      method: "GET",
      uri: "/v1/api/sample/",
      headers: {},
      body: null,
    });
    out.textContent = "HTTP " + resp.status + "\n\n" + resp.body;
  } catch (e) {
    const msg = (e && (e.message || e.toString())) || JSON.stringify(e);
    out.textContent = "ERROR: " + msg;
  }
});
