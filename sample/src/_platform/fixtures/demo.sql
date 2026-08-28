-- fixtures/demo.sql：演示数据（§4.5）——仅 `oj test` / `oj fixture` 灌入，
-- 不随启动重放、不进 release 产物。
INSERT OR IGNORE INTO users (id, username, password_hash, roles)
  VALUES (9, 'neo_dev',
    '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '["user"]');
