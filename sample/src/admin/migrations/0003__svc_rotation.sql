-- R6 svc 凭据自动轮换（v0.1.132）：
--   status       NULL/active = 有效；superseded = 已轮换（旧 jti 宽限期仍放行）
--   grace_until  superseded 的宽限截止（epoch 秒）；过期后 SvcGate 自动移出有效集
--   token        明文 bearer（R6 跨重启恢复面：仅 GET current（svc 自认证）回显；
--                admin list 永不选择该列——明文面不扩）
--   expires_at   签发时写死的 exp（epoch 秒），rotate/current 契约直接返回
ALTER TABLE oj_svc_token ADD COLUMN status TEXT;
ALTER TABLE oj_svc_token ADD COLUMN grace_until INTEGER;
ALTER TABLE oj_svc_token ADD COLUMN token TEXT;
ALTER TABLE oj_svc_token ADD COLUMN expires_at INTEGER;
