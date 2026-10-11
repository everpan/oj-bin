-- 内容版本化原语（v0.1.61）：修订表 + CAS 内容表。mysql/pg 由方言覆盖文件接管。
CREATE TABLE IF NOT EXISTS oj_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_type TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  author TEXT,
  message TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (entity_type, entity_id, version)
);
CREATE INDEX IF NOT EXISTS idx_oj_versions_entity ON oj_versions (entity_type, entity_id, version);
CREATE TABLE IF NOT EXISTS oj_ver_content (
  sha256 TEXT PRIMARY KEY,
  content TEXT NOT NULL
);
