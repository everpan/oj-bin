// 会话与签发共享逻辑（对齐旧 server/auth.rs token_pair/session_* 语义）。

export function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

export function sessionKey(refreshToken: string): string {
  return "AUTH-SESSION:" + crypto.sha256Hex(refreshToken);
}

// 签 access + 生成 refresh 并落 session（exp = now + refresh 时长，读取侧惰性判定）。
export async function issueTokens(uid: string, roles: string[]) {
  const accessToken = await jwt.sign({ sub: uid, roles });
  const refreshToken = crypto.randomHex(32);
  await kv.set(
    sessionKey(refreshToken),
    JSON.stringify({ uid, exp: nowSecs() + jwt.refreshDuration }),
  );
  return {
    access_token: accessToken,
    refresh_token: refreshToken,
    expires_in: jwt.accessDuration,
    user: { id: uid, roles },
  };
}
