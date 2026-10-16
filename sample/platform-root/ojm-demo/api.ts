// 平台特权演示模块（ojm 批次，非阻塞项 D4 补遗）。
//
// 本模块行使三类平台特权面，前提由 config 侧装配保证：
//   - config 的 `platform.modules` 含 "ojm-demo"；
//   - config 的 `platform.root` 指向本目录父级（sample/platform-root），
//     且本模块经 mounts 的 `/_platform` 前缀挂载，归属即 "ojm-demo"
//     （模块身份请求级、URL/JS 侧不可伪造）；
//   - config 的 `fs:` 段已配置（fs 轴与 exec 的前置），且 `trusted_paths`
//     覆盖 config.yaml；`platform.exec.ojm-demo.allow` 含 tar 模板。
//
// 三个 handler 各演示一个特权面；每个都用 try/catch 包成 json.fail，绝不裸抛，
// 便于直接观察 NotCapable / 模板失配等授权错误文案。
//
// 注意：exec 的 argv 模板占位符（如 ${path:archive}）写在 config 的
// `platform.exec.ojm-demo.allow` 里；JS 侧只填真实路径 token，由运行时做
// 模板匹配 + 路径白名单（fs.root ∪ trusted_paths）校验。

// GET /_platform/ojm-demo/
// fs 特权：读 trusted_paths 内的 config.yaml 前 200 字符，证明特权模块可越出
// fs.root 读受信文件（前提：config.yaml 在 fs.root ∪ trusted_paths 内）。
async function get(): Promise<void> {
  try {
    const text = await fs.readTextFile("./config.yaml");
    json.ok({ preview: text.slice(0, 200) });
  } catch (e) {
    json.fail(500, "fs read failed: " + String(e));
  }
}

// POST /_platform/ojm-demo/
// platform.modules 特权：进程内触发热更（server.reloadConfig）。
// 非特权模块调用会报 NotCapable: module 'x' is not platform-privileged。
async function reload(): Promise<void> {
  try {
    const r = await server.reloadConfig();
    json.ok(r);
  } catch (e) {
    json.fail(500, "reloadConfig failed: " + String(e));
  }
}

// PUT /_platform/ojm-demo/?archive=<path>
// platform.exec 特权：跑系统 tar 预检一个归档（archive 须落在 fs.root ∪ trusted_paths 内）。
// 模板失配 / 越界一律 NotCapable 点名。
async function extract(): Promise<void> {
  try {
    const archive = String(http.param("archive", ""));
    if (!archive) {
      json.fail(400, "archive query param required (path under fs.root ∪ trusted_paths)");
      return;
    }
    const out = await exec.run(["tar", "-tzf", archive], { timeoutMs: 30000 });
    json.ok({ code: out.code, lines: out.stdout.split("\n").length });
  } catch (e) {
    json.fail(500, "exec failed: " + String(e));
  }
}

export default { get, post: reload, put: extract };
