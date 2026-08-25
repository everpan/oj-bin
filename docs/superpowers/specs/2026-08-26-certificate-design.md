# 证书设计方案（运行时校验 & GET 请求限制）

**日期**：2026-08-26

---

## 1. 背景与目标
- 采用非对称加密（RSA‑PSS 或 ECDSA）签发的 JWS（Header·Payload·Signature）证书。
- **在证书有效期内**，系统正常提供所有功能。
- **证书到期后**：立即对所有 `GET` 请求返回 `403` 并输出 JSON 错误；**宽限期** 30 天（可配置）内仍返回提示信息；宽限期结束后服务在 **启动时** 检测到已失效并退出。
- 支持 **热加载**：证书文件若被覆盖，服务自动重新加载并更新状态。
- 配置文件或 CLI 参数可指定证书路径与公钥路径。
- 错误信息使用统一 JSON 结构；日志记录并在 `health` 接口输出证书状态供监控使用。

---

## 2. 证书格式（JWS）
| 部分 | 内容 | 示例 |
|------|------|------|
| Header | `{"alg":"RS256","typ":"JWT"}`（固定） | Base64URL 编码 |
| Payload | `{"iss":"oj-server","sub":"mdm-server","nbf":1704067200,"exp":1798780800}` | 包含 `nbf`（生效时间）与 `exp`（过期时间）Unix 时间戳 |
| Signature | 使用私钥对 `Header.Payload` 的 RSA‑PSS‑SHA256 签名 | Base64URL 编码 |

> 完整的 JWS 字符串形如 `Base64URL(Header).Base64URL(Payload).Base64URL(Signature)`，存放在 `config/certificate.jws` 中。

---

## 3. 配置项（`config.yaml` 示例）
```yaml
server:
  public_key_path: "./config/public_key.pem"   # PEM 格式公钥
  certificate_path: "./config/certificate.jws" # JWS 证书文件
  grace_days: 30                               # 证书到期后宽限天数，默认 30 天
```
也支持通过 CLI 参数 `--cert-path`、`--key-path` 覆盖上述配置。

---

## 4. 启动时证书加载与验证
1. 读取 **公钥**（PEM）并使用 `ring`（或 `rsa`）解析为验证密钥。
2. 读取 **证书文件**（JWS），用 `.` 分割三段并进行 Base64URL 解码。
3. 用公钥验证签名（仅接受 `RS256`/`ES256`）。
4. 解析 Payload JSON，提取 `nbf`、`exp`。
5. 与当前系统时间比较：
   - `now < nbf` → **未生效**（记录 WARN，但仍视为可用，待生效后自动生效）。
   - `now >= exp` → 计算 `grace_end = exp + grace_days*86400`。
     - `now < grace_end` → **GRACE** 状态，记录剩余天数。
     - `now >= grace_end` → **EXPIRED** 状态。
6. 将 **状态**（`valid` / `grace` / `expired`）及对应的截止时间写入 `AppState.certificate_valid_until`（`Option<SystemTime>`）以及 `certificate_status`（在 `AppState` 中额外保存枚举）。
7. 若启动时已进入 **EXPIRED** 且宽限期已结束，记录 `ERROR` 并 `process::exit(1)`，阻止服务启动。

---

## 5. 请求处理中的 GET 限制（`handle`）
在 `server/src/lib.rs` 的 `handle` 函数开头（取得 `verb` 后）加入：
```rust
if verb == "GET" {
    match st.certificate_status {
        CertificateStatus::Valid => {}
        CertificateStatus::Grace { remaining_secs } => {
            let days = remaining_secs / 86_400;
            return fail_response(
                403,
                &format!("Certificate expired, grace period: {} days remaining", days),
            );
        }
        CertificateStatus::Expired => {
            return fail_response(403, "Certificate expired, service unavailable");
        }
    }
}
```
`fail_response` 已在项目中实现，返回统一的 JSON 错误体。

---

## 6. 热加载实现
- 使用 `notify` crate 监控 `public_key_path` 与 `certificate_path`（已在 `Cargo.toml` 中）。
- 文件 **modify** 事件触发 `reload_certificate()`，步骤同第 4 节。
- 若新证书为 **EXPIRED** 且已超过宽限期，立即切换 `AppState.certificate_status` 为 `Expired`，后续 GET 请求即时返回 403。
- 若加载失败（格式错误、签名不匹配），保留旧状态并记录 `WARN`，避免因错误证书导致服务崩溃。

---

## 7. 服务启动提示
在 `main`（或 `server_cmd::run`）完成证书加载后：
```rust
match app_state.certificate_status {
    CertificateStatus::Expired => {
        log::error!("Certificate has expired and grace period elapsed. Service will not start.");
        std::process::exit(1);
    }
    CertificateStatus::Grace { remaining_secs } => {
        let days = remaining_secs / 86_400;
        log::warn!("Certificate expired, {} days grace period remaining. Service starting.", days);
    }
    _ => {}
}
```
确保在日志里能清晰看到启动状态。

---

## 8. Health 接口扩展
在 `server/src/lib.rs` 中的 health 路由（若不存在则新建）返回：
```json
{
  "status": "OK",
  "certificate_status": "valid" | "grace" | "expired",
  "certificate_expiry": "2027-01-01T00:00:00Z"
}
```
监控系统（Prometheus、Grafana 等）可轮询此接口判断服务是否仍在合法运行。

---

## 9. 错误返回示例（JSON）
- **正常**：`200 OK`（业务响应）
- **GRACE**：
  ```json
  {"error":"Certificate expired","detail":"Grace period: 12 days remaining"}
  ```
- **EXPIRED**：
  ```json
  {"error":"Certificate expired","detail":"Service unavailable"}
  ```

---

## 10. 安全与审计措施
| 风险 | 对策 |
|------|------|
| 私钥泄露 | 私钥仅在签发端保管，服务器仅持有公钥。 |
| 证书被篡改 | 使用非对称签名，服务器校验签名后才接受。 |
| 宽限期滥用 | 宽限期可通过 `grace_days` 参数下调，监控系统对 `certificate_status=grace` 触发告警。 |
| 热加载期间竞争 | `AppState` 使用 `RwLock`（读写锁）保证状态切换原子。 |
| 证书路径被恶意替换 | 读取前检查文件权限（只读），记录文件 SHA256 指纹至日志供审计。 |

---

## 11. 多角色评审要点
| 角色 | 关注点 | 评审问题 |
|------|--------|----------|
| 架构师 | 系统耦合、可扩展性、热加载实现方式 | - 证书热加载是否会导致请求并发状态不一致？\n- 若未来需要多租户或多证书方案，当前结构是否易于演进？ |
| 运维 | 部署、监控、日志、容错 | - 配置文件（相对/绝对路径）是否满足容器化或 K8s ConfigMap 部署？\n- `health` 接口返回的字段是否足够 Prometheus 采集？ |
| 开发 | 代码侵入度、错误处理、测试覆盖 | - 现有 `handle` 函数的可读性是否受到影响？\n- 是否需要为证书加载编写独立的单元测试？ |
| 安全专家 | 密钥管理、算法选型、攻击面 | - RSA‑PSS‑SHA256 是否满足当前安全要求？\n- 是否需要提供 CRL/OCSP 撤销机制？\n- 文件权限与审计是否足够防止恶意替换？ |

---

## 12. 实施路线（下一步）
1. **代码实现**：在 `server/src/lib.rs` 添加 `CertificateStatus`、证书加载、热加载、GET 限制逻辑。
2. **配置与文档**：在 `config.yaml` 增加新字段，更新 README 示例。
3. **单元/集成测试**：覆盖证书正常、GRACE、EXPIRED 三种状态及热加载情形。
4. **监控与日志**：在 health 接口中加入证书状态返回，确保日志记录完整。
5. **部署验证**：在本地与 CI 中演练证书替换、宽限期行为。

---

**文档已提交，后续可通过 `writing-plans` skill 生成实现计划并进入编码阶段。**