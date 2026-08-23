CREATE TABLE IF NOT EXISTS account (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, role TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY AUTOINCREMENT, no TEXT NOT NULL, account_id INTEGER NOT NULL, amount REAL NOT NULL);
CREATE TABLE IF NOT EXISTS tenant(id INTEGER PRIMARY KEY AUTOINCREMENT,guid TEXT NOT NULL, en_name TEXT NOT NULL, cn_name TEXT NOT NULL, dsn_key TEXT NOT NULL);
INSERT OR IGNORE INTO account (id, name, role) VALUES (1, 'neo', 'admin');
INSERT OR IGNORE INTO account (id, name, role) VALUES (2, 'trinity', 'user');
INSERT OR IGNORE INTO orders (id, no, account_id, amount) VALUES (1, 'A-0001', 1, 99.5);
INSERT OR IGNORE INTO orders (id, no, account_id, amount) VALUES (2, 'A-0002', 2, 0.5);
INSERT OR IGNORE INTO tenant (id, guid, en_name, cn_name, dsn_key) VALUES (1, "A1E9BFE9-391B-4F03-5DF5-D0AB6B54F5F8", "XXX", "XXX", "default");
INSERT OR IGNORE INTO tenant (id, guid, en_name, cn_name, dsn_key) VALUES (2, "A1E9BFE9-0000-0000-0000-000000000000", "TEST", "TEST ENV", "test");

