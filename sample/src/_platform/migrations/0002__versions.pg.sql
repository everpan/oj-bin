-- 内容版本化原语（v0.1.61）pg 方言覆盖。
CREATE TABLE IF NOT EXISTS oj_versions (
  id BIGSERIAL PRIMARY KEY,
  entity_type TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  version BIGINT NOT NULL,
  sha256 CHAR(64) NOT NULL,
  author TEXT,
  message TEXT,
  created_at BIGINT NOT NULL,
  UNIQUE (entity_type, entity_id, version)
);
CREATE INDEX IF NOT EXISTS idx_oj_versions_entity ON oj_versions (entity_type, entity_id, version);
CREATE TABLE IF NOT EXISTS oj_ver_content (
  sha256 CHAR(64) PRIMARY KEY,
  content TEXT NOT NULL
);
