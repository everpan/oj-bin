-- MCP 三痛点（spec 2026-11-11 P2）：服务凭据 / 机器人绑定 / 换签审计。
-- 列名即契约：serve/src/dispatch.rs 的 SvcGate（jti 门禁）与 admin JS 端点
-- 都按这些列名直查/构造——改列必须三处同步（本文件、schema.yaml、消费端）。
CREATE TABLE IF NOT EXISTS oj_svc_token (
  jti TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  scopes TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS oj_bot_binding (
  provider TEXT NOT NULL,
  external_sub TEXT NOT NULL,
  user_id TEXT NOT NULL,
  last_used_at INTEGER NOT NULL DEFAULT 0,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (provider, external_sub)
);
CREATE TABLE IF NOT EXISTS oj_obo_audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  svc TEXT NOT NULL,
  provider TEXT NOT NULL,
  external_sub TEXT NOT NULL,
  user_id TEXT NOT NULL,
  ok INTEGER NOT NULL,
  reason TEXT NOT NULL DEFAULT '',
  ts INTEGER NOT NULL
);
