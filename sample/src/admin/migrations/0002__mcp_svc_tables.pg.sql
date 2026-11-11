-- pg 方言覆盖（沿 0002__versions.pg.sql 先例）：自增用 BIGSERIAL。
CREATE TABLE IF NOT EXISTS oj_svc_token (
  jti TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  scopes TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS oj_bot_binding (
  provider TEXT NOT NULL,
  external_sub TEXT NOT NULL,
  user_id TEXT NOT NULL,
  last_used_at BIGINT NOT NULL DEFAULT 0,
  created_by TEXT NOT NULL,
  created_at BIGINT NOT NULL,
  PRIMARY KEY (provider, external_sub)
);
CREATE TABLE IF NOT EXISTS oj_obo_audit (
  id BIGSERIAL PRIMARY KEY,
  svc TEXT NOT NULL,
  provider TEXT NOT NULL,
  external_sub TEXT NOT NULL,
  user_id TEXT NOT NULL,
  ok INTEGER NOT NULL,
  reason TEXT NOT NULL DEFAULT '',
  ts BIGINT NOT NULL
);
