//! oj-6 e2e（L1）：WS 房间/presence 原语（ws.join/leave/broadcast/roomSize）。
//!
//! 独立测试二进制的原因同 e2e_cookie.rs：进程级 WsHub 单例 + oj-auth 插件
//! GUARD OnceLock 污染隔离。单 boot 串行断言全程。
//!
//! 前置：`cargo xtask plugin auth` 已归置 bin/plugins/<triple>/（严格清单只装
//! oj-auth 避扫描模式；守卫不接线——cfg.auth = None，WS 路由无鉴权形态）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use oj::server_cmd;
use only_js::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn tmp_project(files: &[(&str, &str)]) -> PathBuf {
    let t = std::env::temp_dir().join(format!("oj-wsrooms-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&t);
    std::fs::create_dir_all(&t).unwrap();
    for (rel, c) in files {
        let p = t.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }
    t
}

fn base_cfg(dir: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.server.port = 0;
    let n = server::test_support::now_secs();
    server::test_support::write_cert_into(
        &mut cfg.server,
        dir,
        n.saturating_sub(3600),
        n + 365 * 86_400,
    );
    cfg.db.insert("default".into(), "sqlite::memory:".into());
    cfg
}

/// 裸 TCP WebSocket 客户端：101 握手 + 掩码文本帧写出 + 服务端文本帧读入。
struct WsClient(TcpStream);

impl WsClient {
    async fn connect(addr: std::net::SocketAddr, path: &str) -> Self {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        // 读到响应头结束（101 Switching Protocols）；残留字节一并丢弃（本用例无）。
        let mut buf = vec![0u8; 1024];
        let mut total = 0;
        loop {
            let n = s.read(&mut buf[total..]).await.unwrap();
            assert!(n > 0, "handshake EOF");
            total += n;
            let head = String::from_utf8_lossy(&buf[..total]);
            if head.contains("\r\n\r\n") {
                assert!(head.starts_with("HTTP/1.1 101"), "upgrade rejected: {head}");
                break;
            }
        }
        Self(s)
    }

    /// 客户端 → 服务端文本帧（MASK 位 + 固定掩码键，协议允许任意键）。
    async fn send_text(&mut self, payload: &str) {
        let bytes = payload.as_bytes();
        let mut frame = Vec::with_capacity(bytes.len() + 8);
        frame.push(0x81); // FIN + text opcode
        let mask_bit = 0x80u8;
        match bytes.len() {
            n if n < 126 => frame.push(mask_bit | n as u8),
            n if n < 65536 => {
                frame.push(mask_bit | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(mask_bit | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let key = [0x11u8, 0x22, 0x33, 0x44];
        frame.extend_from_slice(&key);
        for (i, b) in bytes.iter().enumerate() {
            frame.push(b ^ key[i % 4]);
        }
        self.0.write_all(&frame).await.unwrap();
    }

    /// 读一帧服务端文本帧（服务端帧不掩码；支持 7/16/64 位长度）。
    async fn read_text(&mut self) -> String {
        let mut hdr = [0u8; 2];
        self.0.read_exact(&mut hdr).await.unwrap();
        assert_eq!(
            hdr[0] & 0x0f,
            0x1,
            "expect text frame, opcode={:#x}",
            hdr[0]
        );
        let len = match hdr[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                self.0.read_exact(&mut b).await.unwrap();
                u16::from_be_bytes(b) as u64
            }
            127 => {
                let mut b = [0u8; 8];
                self.0.read_exact(&mut b).await.unwrap();
                u64::from_be_bytes(b)
            }
            n => n as u64,
        };
        let mut payload = vec![0u8; len as usize];
        self.0.read_exact(&mut payload).await.unwrap();
        String::from_utf8(payload).unwrap()
    }

    /// 超时读：None = 指定时长内无帧（广播除己语义的判定材料）。
    async fn read_text_timeout(&mut self, d: Duration) -> Option<String> {
        tokio::time::timeout(d, self.read_text()).await.ok()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ws_rooms_join_broadcast_leave_and_disconnect_cleanup_end_to_end() {
    let t = tmp_project(&[
        (
            "src/rooms/manifest.yaml",
            "name: rooms\ndesc: d\nversion: 0.1.0\n",
        ),
        (
            "src/rooms/api.ts",
            "export default {\n\
             \x20 get() { json.ok({ size: ws.roomSize(\"r1\") }); },\n\
             };\n",
        ),
        (
            "src/rooms/ws.ts",
            "export default {\n\
             \x20 connection() { json.ok({ hello: sess.id }); },\n\
             \x20 message() {\n\
             \x20   const m = http.body || {};\n\
             \x20   if (m.op === \"join\") { ws.join(m.room); json.ok({ op: \"join\", size: ws.roomSize(m.room) }); }\n\
             \x20   else if (m.op === \"leave\") { ws.leave(m.room); json.ok({ op: \"leave\", size: ws.roomSize(m.room) }); }\n\
             \x20   else if (m.op === \"broadcast\") { json.ok({ op: \"broadcast\", sent: ws.broadcast(m.room, m.data) }); }\n\
             \x20   else if (m.op === \"size\") { json.ok({ op: \"size\", size: ws.roomSize(m.room) }); }\n\
             \x20   else { json.ok({ op: \"noop\" }); }\n\
             \x20 },\n\
             };\n",
        ),
    ]);
    let mut cfg = base_cfg(&t);
    // 严格清单只装 oj-auth（隔离扫描模式与插件单例）；守卫不接线（cfg.auth = None）。
    cfg.plugins
        .insert("auth".into(), serde_json::Value::Object(Default::default()));
    let (addr, _h) = server_cmd::start(cfg, &t, t.join("src"), "/v1/api".into(), true)
        .await
        .unwrap();

    // 两连接握手；connection 钩子回 hello（sess.id 注入）。
    let mut a = WsClient::connect(addr, "/v1/api/rooms/ws").await;
    let hello_a: serde_json::Value = serde_json::from_str(&a.read_text().await).unwrap();
    assert_eq!(hello_a["code"], 0);
    assert!(hello_a["data"]["hello"].is_number());
    let mut b = WsClient::connect(addr, "/v1/api/rooms/ws").await;
    let hello_b: serde_json::Value = serde_json::from_str(&b.read_text().await).unwrap();
    assert!(hello_b["data"]["hello"].is_number());

    // HTTP 上下文 roomSize（跨上下文共享 hub）。
    let c = reqwest::Client::new();
    let v: serde_json::Value = c
        .get(format!("http://{addr}/v1/api/rooms/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["data"]["size"], 0, "{v}");

    // join：A → 1；B → 2。
    a.send_text(r#"{"op":"join","room":"r1"}"#).await;
    let v: serde_json::Value = serde_json::from_str(&a.read_text().await).unwrap();
    assert_eq!(
        (v["data"]["op"].as_str(), v["data"]["size"].as_u64()),
        (Some("join"), Some(1)),
        "{v}"
    );
    b.send_text(r#"{"op":"join","room":"r1"}"#).await;
    let v: serde_json::Value = serde_json::from_str(&b.read_text().await).unwrap();
    assert_eq!(
        (v["data"]["op"].as_str(), v["data"]["size"].as_u64()),
        (Some("join"), Some(2)),
        "{v}"
    );

    // A 广播：送达数 1（除己）；B 收到帧；A 自己收不到。
    a.send_text(r#"{"op":"broadcast","room":"r1","data":"hi-A"}"#)
        .await;
    let v: serde_json::Value = serde_json::from_str(&a.read_text().await).unwrap();
    assert_eq!(
        (v["data"]["op"].as_str(), v["data"]["sent"].as_u64()),
        (Some("broadcast"), Some(1)),
        "{v}"
    );
    assert_eq!(b.read_text().await, "hi-A");
    assert!(
        b.read_text_timeout(Duration::from_millis(300))
            .await
            .is_none(),
        "broadcast 只送达一次"
    );
    assert!(
        a.read_text_timeout(Duration::from_millis(300))
            .await
            .is_none(),
        "广播除己（A 不应收到自己发的帧）"
    );

    // B 广播 → A 收到。
    b.send_text(r#"{"op":"broadcast","room":"r1","data":"hi-B"}"#)
        .await;
    let v: serde_json::Value = serde_json::from_str(&b.read_text().await).unwrap();
    assert_eq!(v["data"]["sent"], 1, "{v}");
    assert_eq!(a.read_text().await, "hi-B");

    // B 主动 leave → 1；再 A leave → 0（房间回收，HTTP 侧同步可见）。
    b.send_text(r#"{"op":"leave","room":"r1"}"#).await;
    let v: serde_json::Value = serde_json::from_str(&b.read_text().await).unwrap();
    assert_eq!(
        (v["data"]["op"].as_str(), v["data"]["size"].as_u64()),
        (Some("leave"), Some(1)),
        "{v}"
    );

    // B 重进、再直接断连：detach 清理 → A 轮询 size 回落到 1。
    b.send_text(r#"{"op":"join","room":"r1"}"#).await;
    let _ = b.read_text().await;
    drop(b); // socket 关闭 = 客户端断连
    let mut size = 0;
    for _ in 0..50 {
        a.send_text(r#"{"op":"size","room":"r1"}"#).await;
        let v: serde_json::Value = serde_json::from_str(&a.read_text().await).unwrap();
        size = v["data"]["size"].as_u64().unwrap();
        if size == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(size, 1, "断连清理：B 的房间成员资格被摘除");

    // A leave → 0。
    a.send_text(r#"{"op":"leave","room":"r1"}"#).await;
    let v: serde_json::Value = serde_json::from_str(&a.read_text().await).unwrap();
    assert_eq!(v["data"]["size"], 0, "{v}");
}
