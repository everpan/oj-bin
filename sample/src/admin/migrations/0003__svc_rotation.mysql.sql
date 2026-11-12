-- R6 svc 凭据自动轮换（v0.1.132，mysql 方言）：列语义见 0003__svc_rotation.sql。
ALTER TABLE oj_svc_token ADD COLUMN status TEXT;
ALTER TABLE oj_svc_token ADD COLUMN grace_until BIGINT;
ALTER TABLE oj_svc_token ADD COLUMN token TEXT;
ALTER TABLE oj_svc_token ADD COLUMN expires_at BIGINT;
