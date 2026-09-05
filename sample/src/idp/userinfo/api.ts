export default {
  get() {
    const auth = String(http.headers.authorization ?? "");
    const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
    if (!token) {
      json.fail(401, "missing bearer token");
      return;
    }
    let claims: { sub?: unknown; tenant?: unknown };
    try {
      claims = oidc.verify(token);
    } catch {
      json.fail(401, "invalid token");
      return;
    }
    json.raw({ sub: claims.sub, tenant: claims.tenant ?? null });
  },
};
