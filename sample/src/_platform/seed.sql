INSERT OR IGNORE INTO tenant (id, guid, en_name, cn_name, dsn_key) VALUES (1, "A1E9BFE9-391B-4F03-5DF5-D0AB6B54F5F8", "XXX", "XXX", "default");
INSERT OR IGNORE INTO tenant (id, guid, en_name, cn_name, dsn_key) VALUES (2, "A1E9BFE9-0000-0000-0000-000000000000", "TEST", "TEST ENV", "test");
INSERT OR IGNORE INTO users (id, username, password_hash, roles) VALUES (1, 'demo', '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '["admin"]');
-- trinity：user 角色测试账号（password_hash 与 demo 行相同，即密码 demo1234；
-- 仅供 403 用例，样例数据勿用于生产）。
INSERT OR IGNORE INTO users (id, username, password_hash, roles)
  VALUES (2, 'trinity',
    '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '["user"]');
