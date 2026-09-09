//! WS 帧池（spec 2026-09-09）：每路由 队列 + W 个无状态 Worker + Rust 会话表。
//! SOLID：本文件只管「帧的排队/调度/会话表/Worker 池生命周期」；V8 执行细节复用
//! mod.rs 的 WsSession（预载）与 ReqState 捕获链，不在此重复。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use deno_core::error::CoreError;
use tokio::sync::Notify;
use tokio::sync::oneshot;

use super::WsOutcome;

/// 一帧 = 一个无状态作业。
pub(crate) struct Frame {
    pub conn: u64,
    pub ev: &'static str,
    pub body: Vec<u8>,
    pub done: oneshot::Sender<Result<WsOutcome, FrameError>>,
}

/// 帧失败语义：Timeout/Core 对应 RunError（连接侧断连/丢帧），Dropped/PoolClosed 池侧。
#[derive(Debug)]
pub enum FrameError {
    Timeout,
    Core(CoreError),
    /// 连接已 detach：排队帧作废。
    Dropped,
    /// 池已退役/Worker 预载失败。
    PoolClosed,
}

#[derive(Default)]
struct SchedInner {
    ready: VecDeque<Frame>,
    /// per-conn 在飞=1：在飞期间后续帧在此排队（保序）。
    waiting: HashMap<u64, VecDeque<Frame>>,
    in_flight: HashSet<u64>,
    closed: bool,
}

/// 帧调度器：多生产（连接 Reader）/多消费（Worker）。
pub(crate) struct Scheduler {
    inner: Mutex<SchedInner>,
    notify: Notify,
}

impl Scheduler {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SchedInner::default()),
            notify: Notify::new(),
        })
    }

    pub(crate) fn submit(&self, f: Frame) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            let _ = f.done.send(Err(FrameError::PoolClosed));
            return;
        }
        if g.in_flight.contains(&f.conn) {
            g.waiting.entry(f.conn).or_default().push_back(f);
        } else {
            g.in_flight.insert(f.conn);
            g.ready.push_back(f);
        }
        drop(g);
        self.notify.notify_one();
    }

    /// 取下一帧；池关闭（退役）返回 None → Worker 退出。
    pub(crate) async fn pull(&self) -> Option<Frame> {
        loop {
            let notified = self.notify.notified();
            {
                let mut g = self.inner.lock().unwrap();
                if let Some(f) = g.ready.pop_front() {
                    return Some(f);
                }
                if g.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// Worker 执行完一帧：放行该连接的下一帧。
    pub(crate) fn complete(&self, conn: u64) {
        let mut g = self.inner.lock().unwrap();
        g.in_flight.remove(&conn);
        if let Some(q) = g.waiting.get_mut(&conn)
            && let Some(f) = q.pop_front()
        {
            if g.closed {
                let _ = f.done.send(Err(FrameError::PoolClosed));
            } else {
                g.in_flight.insert(conn);
                g.ready.push_back(f);
            }
        }
        drop(g);
        self.notify.notify_one();
    }

    /// 连接 detach：排队帧作废；在飞帧让它自然跑完（complete 无后续可放）。
    pub(crate) fn drop_conn(&self, conn: u64) {
        let mut g = self.inner.lock().unwrap();
        if let Some(q) = g.waiting.remove(&conn) {
            for f in q {
                let _ = f.done.send(Err(FrameError::Dropped));
            }
        }
    }

    /// 退役：拒绝新帧、排空存量（Worker pull→None 退出）。
    pub(crate) fn close(&self) {
        let mut g = self.inner.lock().unwrap();
        g.closed = true;
        for f in g.ready.drain(..) {
            let _ = f.done.send(Err(FrameError::PoolClosed));
        }
        for (_, q) in g.waiting.drain() {
            for f in q {
                let _ = f.done.send(Err(FrameError::PoolClosed));
            }
        }
        drop(g);
        self.notify.notify_waiters();
    }

    /// 复活（退役后新连接 attach）。
    pub(crate) fn reopen(&self) {
        self.inner.lock().unwrap().closed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        conn: u64,
    ) -> (
        Frame,
        tokio::sync::oneshot::Receiver<Result<WsOutcome, FrameError>>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            Frame {
                conn,
                ev: "message",
                body: vec![],
                done: tx,
            },
            rx,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_serializes_per_conn_and_releases_after_complete() {
        let s = Scheduler::new();
        let (f1, _r1) = frame(7);
        let (f2, _r2) = frame(7);
        s.submit(f1);
        s.submit(f2); // conn7 在飞 → f2 进 waiting
        let got = s.pull().await.unwrap();
        assert_eq!(got.conn, 7);
        s.complete(7); // 释放 waiting 里的 f2
        let got2 = s.pull().await.unwrap();
        assert_eq!(got2.conn, 7);
        s.complete(7);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_drop_conn_discards_queued_and_close_resolves_rest() {
        let s = Scheduler::new();
        let (fa, _ra) = frame(1);
        s.submit(fa); // 在飞
        let (fb, rb) = frame(1);
        s.submit(fb); // waiting
        s.drop_conn(1);
        assert!(matches!(rb.await, Ok(Err(FrameError::Dropped))));
        let (fc, rc) = frame(2);
        s.submit(fc);
        s.close();
        assert!(matches!(rc.await, Ok(Err(FrameError::PoolClosed))));
        assert!(s.pull().await.is_none()); // closed → pull None（worker 退出）
        s.reopen(); // 退役后新连接可复活
        let (fd, _rd) = frame(3);
        s.submit(fd);
        assert!(s.pull().await.is_some());
    }
}
