//! ws.* 绑定：WebSocket 帧循环内 JS 主动控制。
//!
//! 帧控制三个 op：send(data) 收集到 ReqState.ws_sends（帧处理器结束后按序写出）、
//! close() 置位 ReqState.ws_close（本帧结束后关连接）、sess_set(v) 由 dispatcher
//! `finally` 把 `__sess` 快照交还 ReqState.ws_sess（帧池 sess.state 外置回传）。
//! HTTP 请求路径不读这三项（等价 nil 连接 no-op）。
//!
//! 房间原语（oj-6）：join/leave/broadcast/roomSize 走进程级 WsHub——join 直接把
//! per-conn 发送端（RequestInfo.bus_tx 注入）注册进房间，广播帧经既有 bus 转发器
//! 写回 socket；conn 断开由 ConnHandle::detach 从全房间摘除。

use deno_core::{JsBuffer, OpState, op2};

use super::{ReqState, WsSend};

// ---- WsHub：进程级房间中心（同 DELIVER_TARGETS 单例先例） ----

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use tokio::sync::mpsc;

/// 房间表：room → conn → per-conn 发送端（attach 时建立的 bus_tx 克隆）。
/// ponytail: remove_conn/广播清理为 O(rooms) 遍历——连接数 ≤ 千级
/// （ws.max_connections 默认 1000），可接受；rooms 膨胀时再上 conn→rooms 反查索引。
pub struct WsHub {
    rooms: Mutex<HashMap<String, HashMap<u64, mpsc::UnboundedSender<WsSend>>>>,
}

impl WsHub {
    pub fn new() -> Self {
        Self {
            rooms: Mutex::new(HashMap::new()),
        }
    }

    /// 加入房间（重复 join 同房间 = 幂等替换发送端）。
    pub fn join(&self, room: &str, conn: u64, tx: mpsc::UnboundedSender<WsSend>) {
        self.rooms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(room.to_string())
            .or_default()
            .insert(conn, tx);
    }

    /// 离开房间；房间空则回收。返回是否原本在房内。
    pub fn leave(&self, room: &str, conn: u64) -> bool {
        let mut rooms = self.rooms.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conns) = rooms.get_mut(room) else {
            return false;
        };
        let was = conns.remove(&conn).is_some();
        if conns.is_empty() {
            rooms.remove(room);
        }
        was
    }

    /// 扇出给房间内除 from 外的全部成员（socket.io 语义：调用方自己回声）。
    /// 顺带摘除已关闭的发送端（对端断开未走 detach 兜底的懒惰清理）。
    pub fn broadcast(&self, room: &str, from: u64, data: WsSend) -> u32 {
        let mut rooms = self.rooms.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conns) = rooms.get_mut(room) else {
            return 0;
        };
        let mut dead = Vec::new();
        let mut delivered = 0u32;
        for (c, tx) in &*conns {
            if *c == from {
                continue;
            }
            match data.try_clone_send(tx) {
                Ok(()) => delivered += 1,
                Err(()) => dead.push(*c),
            }
        }
        for c in dead {
            conns.remove(&c);
        }
        if conns.is_empty() {
            rooms.remove(room);
        }
        delivered
    }

    /// 房间成员数（presence 最小原语；含可能刚断连未摘除者——下一张出或广播即清）。
    pub fn room_size(&self, room: &str) -> usize {
        self.rooms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(room)
            .map_or(0, |c| c.len())
    }

    /// 连接断开：从全部房间摘除（所有退出路径经 ConnHandle::detach 调用）。
    pub fn remove_conn(&self, conn: u64) {
        let mut rooms = self.rooms.lock().unwrap_or_else(|e| e.into_inner());
        rooms.retain(|_, conns| {
            conns.remove(&conn);
            !conns.is_empty()
        });
    }
}

impl Default for WsHub {
    fn default() -> Self {
        Self::new()
    }
}

/// WsSend 的可克隆发送辅助：Text/Binary 各自克隆一份载荷。
impl WsSend {
    fn try_clone_send(&self, tx: &mpsc::UnboundedSender<WsSend>) -> Result<(), ()> {
        let frame = match self {
            WsSend::Text(t) => WsSend::Text(t.clone()),
            WsSend::Binary(b) => WsSend::Binary(b.clone()),
        };
        tx.send(frame).map_err(|_| ())
    }
}

static HUB: LazyLock<Arc<WsHub>> = LazyLock::new(|| Arc::new(WsHub::new()));

/// 进程级房间中心句柄。
pub fn hub() -> Arc<WsHub> {
    HUB.clone()
}

/// join/leave 的公共校验：有效 conn id + WS 帧上下文（per-conn 发送端）。
fn conn_checked(state: &OpState, conn: u32) -> Result<(), String> {
    if conn == 0 {
        return Err("ws rooms: invalid connection id".to_string());
    }
    if state.borrow::<ReqState>().req.bus_tx.is_none() {
        return Err("ws.join/leave: available only inside a ws handler".to_string());
    }
    Ok(())
}

/// ws.join(room)：本连接加入房间（帧经既有 bus 转发器写回 socket）。
#[op2(fast)]
pub(crate) fn op_ws_join(
    state: &mut OpState,
    #[string] room: String,
    conn: u32,
) -> Result<(), deno_error::JsErrorBox> {
    conn_checked(state, conn).map_err(deno_error::JsErrorBox::generic)?;
    let tx = state
        .borrow::<ReqState>()
        .req
        .bus_tx
        .clone()
        .expect("checked by conn_checked");
    hub().join(&room, conn as u64, tx);
    Ok(())
}

/// ws.leave(room)：离开房间（不在房内为 no-op）。
#[op2(fast)]
pub(crate) fn op_ws_leave(
    state: &mut OpState,
    #[string] room: String,
    conn: u32,
) -> Result<(), deno_error::JsErrorBox> {
    conn_checked(state, conn).map_err(deno_error::JsErrorBox::generic)?;
    hub().leave(&room, conn as u64);
    Ok(())
}

/// ws.broadcast(room, data)：扇出给房间内除本连接外全部成员；返回送达数。
/// conn=0（非 WS 上下文，如 HTTP handler 通知房间）不排除任何成员。
#[op2(fast)]
pub(crate) fn op_ws_broadcast(
    state: &mut OpState,
    #[string] room: String,
    #[string] data: String,
    conn: u32,
) -> u32 {
    let _ = state;
    hub().broadcast(&room, conn as u64, WsSend::Text(data))
}

/// ws.broadcast(room, Uint8Array)：二进制帧扇出（与 ws.send 的 binary 臂对称，v0.1.16 帧型）。
#[op2]
pub(crate) fn op_ws_broadcast_bin(
    state: &mut OpState,
    #[string] room: String,
    #[buffer] data: JsBuffer,
    conn: u32,
) -> u32 {
    let _ = state;
    hub().broadcast(&room, conn as u64, WsSend::Binary(data.to_vec()))
}

/// ws.roomSize(room)：房间成员数（任意上下文可调用）。
#[op2(fast)]
pub(crate) fn op_ws_room_size(state: &mut OpState, #[string] room: String) -> u32 {
    let _ = state;
    hub().room_size(&room) as u32
}

/// ws.send(data)：记录一次主动发送（Processor 按序推给 Writer）。
#[op2(fast)]
pub(crate) fn op_ws_send(state: &mut OpState, #[string] data: String) {
    state
        .borrow_mut::<ReqState>()
        .ws_sends
        .push(WsSend::Text(data));
}

/// ws.send(Uint8Array)：二进制帧收集（v0.1.16；Writer 按 opcode 0x2 写出）。
#[op2]
pub(crate) fn op_ws_send_bin(state: &mut OpState, #[buffer] data: JsBuffer) {
    state
        .borrow_mut::<ReqState>()
        .ws_sends
        .push(WsSend::Binary(data.to_vec()));
}

/// ws.close()：请求关闭当前连接。
/// 名字避开 deno_websocket 内置 `op_ws_close`（出站客户端，v0.1.7 并存注册）。
#[op2(fast)]
pub(crate) fn op_ws_frame_close(state: &mut OpState) {
    state.borrow_mut::<ReqState>().ws_close = true;
}

/// WS 帧收尾：dispatcher finally 把 __sess 快照交还（帧池状态外置回传）。
#[op2]
pub(crate) fn op_ws_sess_set(state: &mut OpState, #[serde] v: serde_json::Value) {
    state.borrow_mut::<ReqState>().ws_sess = Some(v);
}

#[cfg(test)]
mod hub_tests {
    use super::*;

    fn tx() -> (
        mpsc::UnboundedSender<WsSend>,
        mpsc::UnboundedReceiver<WsSend>,
    ) {
        mpsc::unbounded_channel()
    }

    #[test]
    fn join_broadcast_excludes_sender_and_leave_cleans_empty_room() {
        let h = WsHub::new();
        let (tx1, mut rx1) = tx();
        let (tx2, mut rx2) = tx();
        h.join("r", 1, tx1);
        h.join("r", 2, tx2);
        assert_eq!(h.room_size("r"), 2);
        // 重复 join 幂等（同 conn 替换发送端，不重复计数）。
        let (tx1b, _old_rx1) = tx();
        h.join("r", 1, tx1b);
        assert_eq!(h.room_size("r"), 2);

        let n = h.broadcast("r", 1, WsSend::Text("hi".into()));
        assert_eq!(n, 1, "除发送者外仅 conn2 收到");
        assert!(rx1.try_recv().is_err(), "发送者不收到自己的广播");
        assert_eq!(rx2.try_recv().unwrap(), WsSend::Text("hi".into()));

        // leave：conn2 离房 → 房间只剩 conn1；conn1 再离 → 房间回收。
        assert!(h.leave("r", 2));
        assert!(!h.leave("r", 2), "不在房内 leave = false");
        assert_eq!(h.room_size("r"), 1);
        assert!(h.leave("r", 1));
        assert_eq!(h.room_size("r"), 0, "空房间回收");
    }

    #[test]
    fn broadcast_prunes_dead_senders_and_remove_conn_strips_all_rooms() {
        let h = WsHub::new();
        let (tx1, _rx1) = tx();
        let (tx2, mut rx2) = tx();
        let (tx3, rx3) = tx();
        h.join("a", 1, tx1);
        h.join("a", 2, tx2.clone());
        h.join("b", 2, tx2);
        h.join("b", 3, tx3);
        drop(rx3); // conn3 对端已死（发送端还在房内）

        let n = h.broadcast("a", 1, WsSend::Text("x".into()));
        assert_eq!(n, 1);
        assert_eq!(rx2.try_recv().unwrap(), WsSend::Text("x".into()));
        // 广播未触达的房间保留死发送端；remove_conn 一次性清两房。
        h.remove_conn(3);
        assert_eq!(h.room_size("b"), 1);
        h.remove_conn(2);
        assert_eq!(h.room_size("a"), 1);
        assert_eq!(h.room_size("b"), 0, "conn2 离房后 b 空回收");
        h.remove_conn(1);
        assert_eq!(h.room_size("a"), 0);
        // 空房间广播 = 0，不炸。
        assert_eq!(h.broadcast("nope", 0, WsSend::Text("y".into())), 0);
    }
}

#[cfg(test)]
mod ws_client_tests {
    use crate::bridge::{Bridge, Extras, InMemoryKV, RequestInfo, SchemaRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn bridge() -> Bridge {
        Bridge::with_dbs_and_loader(
            HashMap::new(),
            Arc::new(InMemoryKV::new()),
            SchemaRegistry::new(),
            false,
            None,
            Extras::default(),
        )
    }

    async fn run(b: &Bridge, src: &str) -> Result<String, String> {
        b.run_with(src, RequestInfo::default())
            .await
            .map(|c| String::from_utf8_lossy(&c.body).into_owned())
            .map_err(|e| e.to_string())
    }

    /// 全局已由 deno_websocket 扩展声明（bootstrap.js 挂载）。
    #[tokio::test(flavor = "current_thread")]
    async fn given_runtime_when_probed_then_websocket_global_declared() {
        let out = run(&bridge(), "json.ok(typeof WebSocket === \"function\");")
            .await
            .unwrap();
        assert!(out.contains("true"), "{out}");
    }

    /// 回环：in-process tokio-tungstenite 服务端推一帧，WHATWG 客户端收到。
    /// 钉住 op 链路 + allow_all 权限 + 事件循环驱动。
    #[tokio::test(flavor = "current_thread")]
    async fn given_local_ws_server_when_connected_then_frame_received() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use futures::{SinkExt, StreamExt};
            use tokio_tungstenite::tungstenite::Message;
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::text("hello-from-oj")).await.unwrap();
            let _ = ws.next().await; // 悬住连接，客户端读完帧后 close
        });
        let b = bridge();
        let src = format!(
            "(async () => {{
                const ws = new WebSocket(\"ws://{addr}/\");
                const msg = await new Promise((ok, err) => {{
                    ws.onmessage = (e) => ok(e.data);
                    ws.onerror = () => err(new Error(\"ws error\"));
                    ws.onclose = () => err(new Error(\"ws closed\"));
                }});
                ws.close();
                json.ok(String(msg));
            }})()"
        );
        let out = run(&b, &src).await.unwrap();
        assert!(out.contains("hello-from-oj"), "{out}");
    }
}
