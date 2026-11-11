-- mysql 方言覆盖（沿 0002__versions.mysql.sql 先例）：TEXT 不可作 PK/带默认值，
-- 键列用 VARCHAR(191)，reason 不带 DEFAULT（TEXT 列禁 DEFAULT）。
CREATE TABLE IF NOT EXISTS oj_svc_token (
  jti VARCHAR(191) PRIMARY KEY,
  name VARCHAR(191) NOT NULL,
  scopes TEXT NOT NULL,
  created_by VARCHAR(191) NOT NULL,
  created_at BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE IF NOT EXISTS oj_bot_binding (
  provider VARCHAR(191) NOT NULL,
  external_sub VARCHAR(191) NOT NULL,
  user_id VARCHAR(191) NOT NULL,
  last_used_at BIGINT NOT NULL DEFAULT 0,
  created_by VARCHAR(191) NOT NULL,
  created_at BIGINT NOT NULL,
  PRIMARY KEY (provider, external_sub)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE TABLE IF NOT EXISTS oj_obo_audit (
  id BIGINT AUTO_INCREMENT PRIMARY KEY,
  svc VARCHAR(191) NOT NULL,
  provider VARCHAR(191) NOT NULL,
  external_sub VARCHAR(191) NOT NULL,
  user_id VARCHAR(191) NOT NULL,
  ok INTEGER NOT NULL,
  reason TEXT NOT NULL,
  ts BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
