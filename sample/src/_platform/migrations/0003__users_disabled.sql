-- users.disabled（spec 2026-11-11 P2）：0 在职 1 停用——obo 换签与 login 都以此拒绝。
ALTER TABLE users ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;
