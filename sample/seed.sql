CREATE TABLE IF NOT EXISTS account (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, role TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY AUTOINCREMENT, no TEXT NOT NULL, account_id INTEGER NOT NULL, amount REAL NOT NULL);
CREATE TABLE IF NOT EXISTS tenant(id INTEGER PRIMARY KEY AUTOINCREMENT,guid TEXT NOT NULL, en_name TEXT NOT NULL, cn_name TEXT NOT NULL, dsn_key TEXT NOT NULL);
INSERT OR IGNORE INTO account (id, name, role) VALUES (1, 'neo', 'admin');
INSERT OR IGNORE INTO account (id, name, role) VALUES (2, 'trinity', 'user');
INSERT OR IGNORE INTO orders (id, no, account_id, amount) VALUES (1, 'A-0001', 1, 99.5);
INSERT OR IGNORE INTO orders (id, no, account_id, amount) VALUES (2, 'A-0002', 2, 0.5);
INSERT OR IGNORE INTO tenant (id, guid, en_name, cn_name, dsn_key) VALUES (1, "A1E9BFE9-391B-4F03-5DF5-D0AB6B54F5F8", "XXX", "XXX", "default");
INSERT OR IGNORE INTO tenant (id, guid, en_name, cn_name, dsn_key) VALUES (2, "A1E9BFE9-0000-0000-0000-000000000000", "TEST", "TEST ENV", "test");

CREATE TABLE IF NOT EXISTS users (id INTEGER PRIMARY KEY AUTOINCREMENT, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL, roles TEXT NOT NULL DEFAULT '[]');
INSERT OR IGNORE INTO users (id, username, password_hash, roles) VALUES (1, 'demo', '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '["admin"]');

CREATE TABLE IF NOT EXISTS certs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  note TEXT NOT NULL DEFAULT '',
  public_pem TEXT NOT NULL,
  private_pem TEXT NOT NULL,
  cert_jws TEXT NOT NULL,
  nbf INTEGER NOT NULL,
  exp INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
-- trinity：user 角色测试账号（password_hash 与 demo 行相同，即密码 demo1234；
-- 仅供 403 用例，样例数据勿用于生产）。
INSERT OR IGNORE INTO users (id, username, password_hash, roles)
  VALUES (2, 'trinity',
    '$2b$10$aKN7gpFP.dhK7Il8sc19neUPaziSONYdsfks1xm0H2COzkp2vlqV2', '["user"]');
