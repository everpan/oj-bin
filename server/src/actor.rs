//! JS actor：把 !Send 的 Bridge 钉在专用 OS 线程上，axum 侧只经 channel 通信（future 天然 Send）。
//! actor 线程内跑 current_thread tokio runtime，串行执行 job；请求并发度 = actor 数（P4 按 PoolSize 开多个）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use only_js::bridge::{Bridge, Capture, RequestInfo};
use tokio::sync::{mpsc, oneshot};

/// 一次 handler 执行请求。
pub enum Job {
    /// 源码执行（per-request 源码字符串）。
    Source {
        source: String,
        req: RequestInfo,
        timeout: Option<std::time::Duration>,
        resp: oneshot::Sender<Result<Capture, RunFail>>,
    },
    /// ESM 模块执行（oj server 路径）。
    Module {
        path: std::path::PathBuf,
        method: String,
        req: RequestInfo,
        timeout: Option<std::time::Duration>,
        resp: oneshot::Sender<Result<Capture, RunFail>>,
    },
}

/// run_module 无不限时变体：None 用远大于配置的哨兵（对齐 run 的 None 语义）。
const NO_TIMEOUT_SENTINEL: std::time::Duration = std::time::Duration::from_secs(3600);

/// 执行失败（区分超时熔断 408 与普通失败 500）。
#[derive(Debug, Clone)]
pub struct RunFail {
    pub msg: String,
    pub timeout: bool,
}

impl From<only_js::bridge::RunError> for RunFail {
    fn from(e: only_js::bridge::RunError) -> Self {
        match e {
            only_js::bridge::RunError::Timeout => Self {
                msg: "handler execution timed out".into(),
                timeout: true,
            },
            only_js::bridge::RunError::Core(e) => Self {
                msg: e.to_string(),
                timeout: false,
            },
        }
    }
}

/// JS actor 句柄（Clone = 同一组队列的引用；Send + Sync，可入 axum state）。
#[derive(Clone)]
pub struct JsActor {
    senders: Vec<mpsc::Sender<Job>>,
    next: Arc<AtomicUsize>,
}

impl JsActor {
    /// 单 actor 线程。
    pub fn new(make_bridge: impl Fn() -> Bridge + Send + Sync + 'static) -> Self {
        Self::pool(1, make_bridge)
    }

    /// N 个 actor 线程 + 轮询分发（请求并发度 = n；Go PoolSize 的等价物）。
    /// 工厂包 Arc，各线程内各构造一次 Bridge（Bridge !Send，不可跨线程搬预构实例）。
    pub fn pool(n: usize, make_bridge: impl Fn() -> Bridge + Send + Sync + 'static) -> Self {
        let make = Arc::new(make_bridge);
        let mut senders = Vec::new();
        for _ in 0..n.max(1) {
            let (tx, mut rx) = mpsc::channel::<Job>(64);
            let make = make.clone();
            std::thread::Builder::new()
                .name("js-actor".into())
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("actor runtime init");
                    let bridge = make();
                    rt.block_on(async move {
                        while let Some(job) = rx.recv().await {
                            let (out, resp) = match job {
                                Job::Source {
                                    source,
                                    req,
                                    timeout,
                                    resp,
                                } => {
                                    let out = match timeout {
                                        Some(t) => bridge
                                            .run_with_timeout(&source, req, t)
                                            .await
                                            .map_err(RunFail::from),
                                        None => bridge.run_with(&source, req).await.map_err(|e| {
                                            RunFail {
                                                msg: e.to_string(),
                                                timeout: false,
                                            }
                                        }),
                                    };
                                    (out, resp)
                                }
                                Job::Module {
                                    path,
                                    method,
                                    req,
                                    timeout,
                                    resp,
                                } => {
                                    let t = timeout.unwrap_or(NO_TIMEOUT_SENTINEL);
                                    let out = bridge
                                        .run_module(&path, &method, req, t)
                                        .await
                                        .map_err(RunFail::from);
                                    (out, resp)
                                }
                            };
                            let _ = resp.send(out);
                        }
                    });
                })
                .expect("spawn js-actor thread");
            senders.push(tx);
        }
        Self {
            senders,
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 提交执行并等待 Capture（调用方可在任意 tokio runtime 上 await）。
    pub async fn run(
        &self,
        source: impl Into<String>,
        req: RequestInfo,
        timeout: Option<std::time::Duration>,
    ) -> Result<Capture, RunFail> {
        let (tx, rx) = oneshot::channel();
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        self.senders[i]
            .send(Job::Source {
                source: source.into(),
                req,
                timeout,
                resp: tx,
            })
            .await
            .map_err(|_| RunFail {
                msg: "js actor stopped".into(),
                timeout: false,
            })?;
        rx.await.map_err(|_| RunFail {
            msg: "js actor dropped job".into(),
            timeout: false,
        })?
    }

    /// ESM 模块执行（oj server 路径）。
    pub async fn run_module(
        &self,
        path: std::path::PathBuf,
        method: String,
        req: RequestInfo,
        timeout: Option<std::time::Duration>,
    ) -> Result<Capture, RunFail> {
        let (tx, rx) = oneshot::channel();
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        self.senders[i]
            .send(Job::Module {
                path,
                method,
                req,
                timeout,
                resp: tx,
            })
            .await
            .map_err(|_| RunFail {
                msg: "js actor stopped".into(),
                timeout: false,
            })?;
        rx.await.map_err(|_| RunFail {
            msg: "js actor dropped job".into(),
            timeout: false,
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use only_js::bridge::{InMemoryAccessor, InMemoryKV};
    use serde_json::{Value, json};

    fn actor() -> JsActor {
        JsActor::new(|| {
            Bridge::new(
                std::sync::Arc::new(InMemoryAccessor::new()),
                std::sync::Arc::new(InMemoryKV::new()),
            )
        })
    }

    // 跨线程往返：测试跑在多线程 runtime（同 axum），Bridge 钉在 actor 线程。
    #[tokio::test]
    async fn runs_handler_and_returns_capture() {
        let a = actor();
        let cap = a
            .run(
                r#"json.ok({ m: http.method });"#,
                RequestInfo {
                    method: "GET".into(),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(cap.status, 200);
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v, json!({"code": 0, "msg": "ok", "data": {"m": "GET"}}));
    }

    #[tokio::test]
    async fn reports_js_compile_error() {
        let a = actor();
        let err = a
            .run("this is !!! not valid js", RequestInfo::default(), None)
            .await
            .err()
            .map(|e| e.msg)
            .unwrap_or_default();
        assert!(!err.is_empty());
    }

    // 池：多线程轮询分发，所有 job 均被执行。
    #[tokio::test]
    async fn pool_runs_all_jobs() {
        let pool = JsActor::pool(3, || {
            Bridge::new(
                std::sync::Arc::new(InMemoryAccessor::new()),
                std::sync::Arc::new(InMemoryKV::new()),
            )
        });
        let jobs: Vec<_> = (0..9)
            .map(|i| {
                let p = pool.clone();
                tokio::spawn(async move {
                    let cap = p
                        .run(
                            format!("json.ok({{ i: {i} }});"),
                            RequestInfo::default(),
                            None,
                        )
                        .await
                        .unwrap();
                    let v: Value = serde_json::from_slice(&cap.body).unwrap();
                    v["data"]["i"].as_i64().unwrap()
                })
            })
            .collect();
        let mut got: Vec<i64> = Vec::new();
        for j in jobs {
            got.push(j.await.unwrap());
        }
        got.sort();
        assert_eq!(got, (0..9).collect::<Vec<i64>>());
    }
}
