// 通用小工具（302/时间/base64url/form/cookie）。idp 与 oidc 模块跨模块导入本文件，
// 构建顺序：auth 最先（oj build auth → idp/oidc）。

export function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

// 302 跳转腿：Location 头 + fail(302)（HTTP 状态 = code 的既有映射，响应体为信封 JSON，
// 浏览器只认 302 + Location）。
export function redirect(url: string): void {
  json.header("Location", url);
  json.fail(302, "redirect");
}

// hex → base64url（无 padding）。纯字符表实现：运行时无 btoa/Buffer。
const B64U = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
export function b64uFromHex(hex: string): string {
  let out = "";
  for (let i = 0; i + 2 <= hex.length; i += 6) {
    const chunk = hex.slice(i, i + 6);
    const n = parseInt(chunk, 16);
    const bits = chunk.length * 4; // 24（满 3 字节）或 16（尾 2 字节）
    for (let j = 0; j < bits; j += 6) {
      // 尾组不足 6 位时左移补零（负 shift 在 JS 会按 mod 32 回绕，须单列）。
      const s = bits - j - 6;
      out += B64U[s >= 0 ? (n >>> s) & 0x3f : (n << -s) & 0x3f];
    }
  }
  return out;
}

// x-www-form-urlencoded → record（+ 为空格、%xx 解码）。
export function parseForm(body: unknown): Record<string, string> {
  const out: Record<string, string> = {};
  if (typeof body !== "string") return out;
  for (const pair of body.split("&")) {
    if (!pair) continue;
    const i = pair.indexOf("=");
    const k = i < 0 ? pair : pair.slice(0, i);
    const v = i < 0 ? "" : pair.slice(i + 1);
    out[decodeURIComponent(k.replace(/\+/g, " "))] =
      decodeURIComponent(v.replace(/\+/g, " "));
  }
  return out;
}

// Cookie 头 → record。
export function parseCookies(header: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of String(header || "").split(";")) {
    const i = part.indexOf("=");
    if (i > 0) out[part.slice(0, i).trim()] = part.slice(i + 1).trim();
  }
  return out;
}
