// sso 共享核心：provider 解析 / success_when 判定 / JIT 建号 / 302 目标白名单。
// JIT 键 sso:<provider>:<tenant>:<username>（tenant 段防 users 表跨租户混同——
// users 表无租户列、被所有租户共享，同 OIDC 键的隔离教训）；
// password_hash = "!sso" 非法占位（bcrypt.verify 恒 false → 该号永不可密码登录）。

export type Prov = {
  kind: "form" | "cas2";
  login_url: string;
  cas_validate_url?: string;
  callback_url?: string;
  user_field?: string;
  pass_field?: string;
  body_encoding?: "form" | "json";
  extra_fields?: Record<string, unknown>;
  success_when?: Record<string, unknown>;
  profile?: Record<string, string>;
  allow_http?: boolean;
};

export function providerOf(name: string): Prov | null {
  return (sso.providers as Record<string, Prov>)[name] ?? null;
}

export function dotGet(obj: unknown, path: string): unknown {
  let cur: unknown = obj;
  for (const seg of String(path).split(".")) {
    if (cur == null || typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[seg];
  }
  return cur;
}

/** success_when 判定：点路径缺失/null 仅当期望值为 null 命中；其余
 *  String(actual) === String(want)（0 与 "0" 互命中；不支持数组下标）。 */
export function hitWhen(body: unknown, when: Record<string, unknown>): boolean {
  for (const [path, want] of Object.entries(when)) {
    const got = dotGet(body, path);
    if (want === null) {
      if (got !== null && got !== undefined) return false;
      continue;
    }
    if (got === null || got === undefined || String(got) !== String(want)) return false;
  }
  return true;
}

export async function jit(provider: string, username: string, upstream: unknown,
                          p: Prov): Promise<{ id: string; roles: string[] }> {
  const localName = `sso:${provider}:${http.tenantId ?? ""}:${username}`;
  let rows = await db.table("users").select(["id", "roles"])
    .where({ field: "username", op: "eq", value: localName }).all();
  if (!rows.length) {
    const extra: Record<string, unknown> = {};
    for (const [col, path] of Object.entries(p.profile ?? {})) {
      extra[col] = String(dotGet(upstream, path) ?? "");
    }
    await db.table("users")
      .insert({ username: localName, password_hash: "!sso", roles: "[]", ...extra }).run();
    rows = await db.table("users").select(["id", "roles"])
      .where({ field: "username", op: "eq", value: localName }).all();
  }
  let roles: string[] = [];
  try { roles = JSON.parse(String(rows[0].roles || "[]")); } catch { roles = []; }
  return { id: String(rows[0].id), roles };
}

/** 双 cookie 交接（照 auth/login 的写法）：token 不进 URL，httpOnly 会话 + csrf 双提交。 */
export function setSessionCookies(accessToken: string): void {
  const maxAge = 86400; // 与 oj-auth cookie.ttl_secs 默认对齐
  json.header("Set-Cookie",
    `oj_sess=${accessToken}; HttpOnly; SameSite=Lax; Path=/; Max-Age=${maxAge}`);
  json.header("Set-Cookie",
    `oj_csrf=${crypto.randomHex(16)}; SameSite=Lax; Path=/; Max-Age=${maxAge}`);
}

/** 302 目标白名单：`/` 开头且非 `//` 的相对路径，其余回落 `/`（防 open-redirect）。 */
export function safeNext(raw: unknown): string {
  const s = String(raw ?? "/");
  return s.startsWith("/") && !s.startsWith("//") ? s : "/";
}
