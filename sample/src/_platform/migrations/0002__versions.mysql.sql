-- 内容版本化原语（v0.1.61）mysql 方言覆盖：sha256 用 CHAR(64)（TEXT 不可作 PK），
-- UNIQUE(entity_type, entity_id, version) 是乐观锁的并发兜底。
CREATE TABLE IF NOT EXISTS oj_versions (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  entity_type VARCHAR(191) NOT NULL,
  entity_id VARCHAR(191) NOT NULL,
  version BIGINT NOT NULL,
  sha256 CHAR(64) NOT NULL,
  author VARCHAR(191),
  message TEXT,
  created_at BIGINT NOT NULL,
  UNIQUE KEY uq_ver (entity_type, entity_id, version),
  KEY idx_ent (entity_type, entity_id, version)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE IF NOT EXISTS oj_ver_content (
  sha256 CHAR(64) PRIMARY KEY,
  content MEDIUMTEXT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
