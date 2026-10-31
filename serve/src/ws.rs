//! WebSocket 层（P5a echo + P5b JS 帧循环，v0.1.10 帧池化）。
//!
//! Go 模式：WS 路由注册在 catch-all 之前；Reader/Processor/Writer 三任务流水线，
//! msgChan/respChan 各 cap 64（背压保护）。JS 帧经 RoutePool 调度（per-conn 串行
//! 保序、W 个无状态 worker、毒化半径 = 1）——`JsRuntime` !Send 的约束沉到池的
//! worker 线程（current_thread runtime），连接侧 frame_loop 全 Send 跑在 axum
//! runtime 上，不再每连接起专用线程。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::ws::{Message, WebSocket};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use only_js::bridge::frame_pool::{FrameError, RoutePool};
use only_js::bridge::{Bridge, ModuleIndex, WsOutcome, WsSend};

/// 全局 WS 并发连接闸门（config ws.max_connections；0 = 不限）。
static WS_LIVE: AtomicU64 = AtomicU64::new(0);

fn gate_enter(max: u64) -> bool {
    loop {
        let live = WS_LIVE.load(Ordering::Relaxed);
        if max != 0 && live >= max {
            return false;
        }
        if WS_LIVE
            .compare_exchange(live, live + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return true;
        }
    }
}

/// 闸门计数守卫：连接无论正常收尾、panic 还是被取消（任务被 drop，如测试
/// runtime 关停），结束时都递减 WS_LIVE——与 gate_enter 1:1 配对，不漏减。
/// 以 Arc 双持：upgrade 失败（on_failed_upgrade）与连接结束（on_upgrade 闭包
/// drop）两条路径共享一份计数，任一路径终结时 GateGuard 恰好 drop 一次。
struct GateGuard;
impl Drop for GateGuard {
    fn drop(&mut self) {
        WS_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// WS 装配选项（来自 config ws 段；server 不依赖 config crate）。
#[derive(Clone, Copy)]
pub struct WsOptions {
    /// 全局并发连接上限：超限 upgrade 直接 503；0 = 不限。
    pub max_connections: u64,
    /// 每路由 Worker 数（无状态，可小于并发连接数）。
    pub workers_per_route: usize,
    /// 路由连接归零后 Worker 池保活毫秒数（0 = 立即退役）。
    pub idle_linger_ms: u64,
}

impl Default for WsOptions {
    fn default() -> Self {
        Self {
            max_connections: 1000,
            workers_per_route: 2,
            idle_linger_ms: 0,
        }
    }
}

/// 挂载最小 echo 路由（GET /ws）——P5a 链路验证用。
pub fn echo_route() -> axum::Router {
    axum::Router::new().route("/ws", axum::routing::get(upgrade))
}

/// 挂载 JS handler 驻留会话路由：connection/message/close 逐帧经 pool 调度
/// （per-conn 串行保序），json.ok 信封与 ws.send 逐事件写回；单事件熔断超时
/// 由池内看门狗承担（RoutePool 构造时注入，超时必断连）；
/// opts.max_connections 为全局并发连接上限（0 = 不限），超限 upgrade 直接 503。
///
/// 无鉴权形态（测试/无守卫部署）；生产走 js_route_guarded。
pub fn js_route(path: &str, pool: Arc<RoutePool>, opts: WsOptions) -> axum::Router {
    js_route_guarded(path, "", pool, opts, None)
}

/// 守卫形态：upgrade 前过 AuthGuard（path_no_base = base 之后路径，与 HTTP 管线
/// 同语义；WS 握手 = GET 方法、cookie 为唯一凭证形态——浏览器 WS 发不了
/// Authorization 头，Bearer 经 query token 的变体本期不做，见 04-oj-upstream-prs）。
/// Err → 401 不升级；Ok(None)（匿名路径）与 Ok(Some(user)) 都放行。
pub fn js_route_guarded(
    path: &str,
    path_no_base: &str,
    pool: Arc<RoutePool>,
    opts: WsOptions,
    guard: Option<Arc<dyn only_js::bridge::AuthGuard>>,
) -> axum::Router {
    let path_no_base = path_no_base.to_string();
    axum::Router::new().route(
        path,
        axum::routing::get(
            move |ws: axum::extract::WebSocketUpgrade, headers: axum::http::HeaderMap| {
                let pool = pool.clone();
                let path_no_base = path_no_base.clone();
                async move {
                    if let Some(g) = &guard {
                        let authz = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok());
                        // 与 serve::run_route 同构：全部请求头 JSON（小写名 → 值）。
                        let headers_json = serde_json::to_string(
                            &headers
                                .iter()
                                .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str(), s)))
                                .collect::<std::collections::HashMap<_, _>>(),
                        )
                        .unwrap_or_default();
                        if let Err(msg) = g.verify(&path_no_base, "GET", authz, Some(&headers_json))
                        {
                            return fail_response(401, &msg);
                        }
                    }
                    if !gate_enter(opts.max_connections) {
                        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                    // 槽位归还两条路径共享一份 Arc<GateGuard>：upgrade 半路失败
                    // （客户端握手后即 RST，axum 走 on_failed_upgrade、永不 spawn
                    // on_upgrade 回调）与连接真实结束各持一个克隆，GateGuard 恰 drop
                    // 一次——缺了 failed 路径，反复 connect+RST 可永久打满闸门（DoS）。
                    let gate = Arc::new(GateGuard);
                    let failed_gate = Arc::clone(&gate);
                    ws.on_failed_upgrade(move |_err| drop(failed_gate))
                        .on_upgrade(move |socket| async move {
                            // 连接真实结束（正常收尾/panic/任务取消）才递减闸门计数。
                            let _gate = gate;
                            frame_loop(socket, pool).await;
                        })
                }
            },
        ),
    )
}

/// 401 信封（与 tasks.rs fail_response 同形；ws.rs 自带一份避免跨模块伸手）。
fn fail_response(status: u16, msg: &str) -> Response {
    let (body, _) = only_js::bridge::fail(status as i32, msg, &serde_json::Value::Null);
    let mut r = Response::new(axum::body::Body::from(body));
    *r.status_mut() = axum::http::StatusCode::from_u16(status)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    r
}

/// 一条 WS 目录镜像挂载。装配（注册路由）、启动清单、下游核对**同一份数据**——
/// 下游工具不得再从目录结构自行推算 WS 路径（那正是 v0.1.87 前口径分叉的来源）。
#[derive(Clone, Debug)]
pub struct WsMount {
    /// 完整 HTTP 路径（含挂载 base）：`/v1/api/wiki/ws`。
    pub path: String,
    /// 守卫面路径（剥 base，与 `auth.anonymous_paths` 同根）：`/wiki/ws`。
    pub path_no_base: String,
    /// ws.ts（dev）/ ws.js（release）源文件。
    pub file: PathBuf,
}

/// 目录镜像挂载计算（装配与启动清单同源）。
///
/// `<模块根>/<dir>/ws.ts` → `GET {base}/{prefix}/{dir}/ws`；模块根级 ws.ts →
/// `GET {base}/{prefix}/ws`；模块外（挂载根散文件）→ `GET {base}/ws`。
///
/// **与 api.ts 同源**：rel 一律走 `routes::mirror_rel`（`--module-root` 下由模块索引
/// 给出 `{prefix}/{模块根内路径}`）。此前按「文件父目录相对挂载根」平铺计算，
/// `src/wiki/api/ws.ts` 会挂成 `/api/wiki/api/ws`——多出一层**布局**段（`api` 是模块根
/// 本身，不是路由段），与 api.ts 的 `/api/wiki/tree` 自相矛盾（v0.1.87 修）。
///
/// `ts`：dev=true（`ws.ts` 优先）/ release=false（`ws.js` 优先，dist 残留的 `ws.ts`
/// 不得抢先）。
///
/// 同 path 由**不同目录**的两个文件映射而来 → **Err 拒启**：胜者决定执行哪份 JS
/// （会话状态、`bus.subscribe` 出口都在其中），不能交给文件遍历顺序。同目录的
/// `ws.ts`/`ws.js` 兄弟是先到先得，不在此列。
pub fn ws_mounts(
    base: &str,
    root: &Path,
    ts: bool,
    index: Option<&ModuleIndex>,
) -> Result<Vec<WsMount>, String> {
    let mut seen: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for file in ws_files(root, ts) {
        let rel = crate::routes::mirror_rel(root, &file, index);
        let tail = if rel.is_empty() {
            "ws".to_string()
        } else {
            format!("{rel}/ws")
        };
        let path = crate::routes::join_url(base, &tail);
        // path_no_base（守卫面）：与 HTTP 管线的 path_no_base 同根——{rel}/ws，
        // 根级 ws 为 "/ws"；anonymous_paths 里 WS 路由按此形态声明。
        let path_no_base = format!("/{tail}");
        match seen.get(&path) {
            Some(prev) if prev.parent() == file.parent() => continue, // 同目录 ts/js 兄弟：先到者胜
            Some(prev) => {
                return Err(format!(
                    "ws mount conflict: {} and {} both map to GET {path}\n  \
                     下一步：删除或改名其中一个（胜者决定执行哪份 JS）",
                    rel_show(prev, root),
                    rel_show(&file, root)
                ));
            }
            None => {}
        }
        seen.insert(path.clone(), file.clone());
        out.push(WsMount {
            path,
            path_no_base,
            file,
        });
    }
    Ok(out)
}

/// 清单/报错用：相对挂载根显示（启动日志不落本机绝对路径）。
fn rel_show(p: &Path, root: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// 按 `ws_mounts` 的挂载清单注册路由（无 WS 文件 → 空 Router，merge 无副作用）。
/// release 下 root=dist，URL 含模块版本段（news-0.1.0/ws）——v0.2 已知限制，见 user-manual。
pub fn mirror_routes(
    mounts: &[WsMount],
    timeout: std::time::Duration,
    make_bridge: impl Fn() -> Bridge + Send + Sync + 'static,
    opts: WsOptions,
    guard: Option<Arc<dyn only_js::bridge::AuthGuard>>,
) -> axum::Router {
    let make = Arc::new(make_bridge);
    let mut router = axum::Router::new();
    for m in mounts {
        // 每路由一池：worker 数与空池保活来自 ws 配置段。
        let pool = RoutePool::new(
            m.file.clone(),
            make.clone(),
            timeout,
            opts.workers_per_route,
            opts.idle_linger_ms,
        );
        router = router.merge(js_route_guarded(
            &m.path,
            &m.path_no_base,
            pool,
            opts,
            guard.clone(),
        ));
    }
    router
}

/// root 下全部 WS 处理器：主扩展名全部在前（优先），次扩展名在后；各自排序保证注册序确定。
/// dev（ts=true）`ws.ts` 优先；release `ws.js` 优先。
fn ws_files(root: &Path, ts: bool) -> Vec<PathBuf> {
    let (first, second) = if ts {
        ("ws.ts", "ws.js")
    } else {
        ("ws.js", "ws.ts")
    };
    let mut a = Vec::new();
    crate::routes::walk_files(root, first, &mut a);
    let mut b = Vec::new();
    crate::routes::walk_files(root, second, &mut b);
    a.sort();
    b.sort();
    a.extend(b);
    a
}

/// upgrade 后的连接处理：整个连接搬到专用 OS 线程（current_thread runtime）。
async fn echo_on_pinned(socket: WebSocket) {
    std::thread::Builder::new()
        .name("ws-conn".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("ws conn runtime init");
            rt.block_on(async move {
                let mut socket = socket;
                while let Some(Ok(msg)) = socket.recv().await {
                    if matches!(msg, Message::Close(_)) {
                        break;
                    }
                    // 仅回显文本/二进制帧（ping/pong 由 axum 自动处理）。
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
            });
        })
        .expect("spawn ws-conn thread");
}

/// axum upgrade handler（echo 路由用，不占用 axum 线程做任何帧处理）。
async fn upgrade(ws: axum::extract::WebSocketUpgrade) -> Response {
    ws.on_upgrade(echo_on_pinned)
}

/// 事件结果写出：ws.send 集合先于信封帧（顺序契约，原 run_ws 消费端逐行等价）。
fn emit(resp_tx: &mpsc::Sender<WsSend>, o: WsOutcome) {
    for s in o.sends {
        let _ = resp_tx.try_send(s); // 满则丢弃
    }
    if !o.capture.body.is_empty() {
        let _ = resp_tx.try_send(WsSend::Text(
            String::from_utf8_lossy(&o.capture.body).into_owned(),
        ));
    }
}

/// 帧池连接循环（全 Send，跑在 axum runtime 上）：
/// Reader(stream→msgChan) / Writer(respChan→sink) / Bus forwarder 三任务与 v0.1.9
/// 逐字相同；事件循环把 connection/message/close 逐帧投给 pool（per-conn 串行、
/// 会话状态由池的 Rust 会话表持有，worker 无状态可换人；单帧超时熔断在池内
/// 看门狗，本函数不再持有 timeout）。语义：
/// Timeout = 必断连（V8 已 terminate，worker 弃会话）；Core = 丢帧继续；
/// PoolClosed = 干净断连；客户端 Close/socket 断 = 正常收尾。
/// 所有退出路径都先 detach（删会话条目 + 丢排队帧 + 空池退役计时）再收尾写出。
async fn frame_loop(socket: WebSocket, pool: Arc<RoutePool>) {
    let (msg_tx, mut msg_rx) = mpsc::channel::<(Vec<u8>, bool)>(64);
    let (resp_tx, mut resp_rx) = mpsc::channel::<WsSend>(64);
    // bus 会话端：发送端经 attach 存入池会话表（worker publish 用）；收到的广播帧转写回 socket。
    let (bus_tx, mut bus_rx) = mpsc::unbounded_channel::<WsSend>();

    let (mut sink, mut stream) = socket.split();

    // Reader：读帧 → msgChan（帧型随行：Text=false / Binary=true；满则背压至 TCP 层）。
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let frame = match msg {
                Message::Text(t) => (t.as_bytes().to_vec(), false),
                Message::Binary(b) => (b.to_vec(), true),
                Message::Close(_) => break,
                _ => continue, // ping/pong 自动处理
            };
            if msg_tx.send(frame).await.is_err() {
                break;
            }
        }
    });

    // Bus forwarder：订阅的广播帧 → 同一写出通道（与 ws.send 天然保序）。
    // 连接结束由 frame_loop abort 收尾——bus_tx 会滞留 Bus 表，不 abort 则 Writer 永不排空。
    let forwarder = tokio::spawn({
        let resp_tx = resp_tx.clone();
        async move {
            while let Some(frame) = bus_rx.recv().await {
                let _ = resp_tx.try_send(frame);
            }
        }
    });

    // 预载失败（文件缺失/无钩子）已锁存 → attach 后 fire 恒 PoolClosed。
    let handle = pool.attach(bus_tx);
    // 超时毒化后该连接的会话已随 worker 丢弃：跳过后续一切 fire（含 close）。
    let mut alive = true;
    match handle.fire("connection", Vec::new(), false).await {
        Ok(o) => emit(&resp_tx, o),
        Err(FrameError::Timeout) => alive = false,
        // 预载失败 → 干净断连（先发 Close 帧再丢弃，避免未读数据触发 TCP RST）。
        Err(FrameError::PoolClosed) => {
            let _ = sink.send(Message::Close(None)).await;
            handle.detach();
            return;
        }
        Err(e) => eprintln!("ws connection: {e}"),
    }

    // Writer：respChan → 串行写回（Text/Binary 按帧型）；通道排空（连接结束）后发 Close 帧干净关闭。
    // （置后启动：PoolClosed 断连路径要在 sink 被 writer 接走前直发 Close。）
    let writer = tokio::spawn(async move {
        while let Some(frame) = resp_rx.recv().await {
            let msg = match frame {
                WsSend::Text(t) => Message::Text(t.into()),
                WsSend::Binary(b) => Message::Binary(b.into()),
            };
            if sink.send(msg).await.is_err() {
                return;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    while alive {
        let Some((bytes, binary)) = msg_rx.recv().await else {
            break; // 客户端 Close / socket 断
        };
        match handle.fire("message", bytes, binary).await {
            Ok(o) => {
                let closing = o.close;
                emit(&resp_tx, o);
                if closing {
                    break;
                }
            }
            // 帧超时 = 必断连（V8 已 terminate，worker 已弃会话）。
            Err(FrameError::Timeout) => {
                eprintln!("ws frame timeout");
                alive = false;
                break;
            }
            // 池退役/预载失败锁存 → 干净断连。
            Err(FrameError::PoolClosed) => {
                alive = false;
                break;
            }
            // error() 缺失或自身抛：丢帧继续（与「error 后连接继续」决策一致）。
            Err(e) => eprintln!("ws frame error: {e}"),
        }
    }
    if alive {
        // close 恰好一次，尽力而为：钩子可 ws.send 离帧，失败只记日志。
        match handle.fire("close", Vec::new(), false).await {
            Ok(o) => emit(&resp_tx, o),
            Err(e) => eprintln!("ws close: {e}"),
        }
    }
    handle.detach(); // 所有退出路径汇合：删会话条目 + 丢排队帧 + 空池退役计时
    drop(resp_tx); // Writer 排空后自然退出
    forwarder.abort(); // 释放 bus_rx 与 resp_tx 克隆，Writer 才能排空退出
    let _ = writer.await;
}

#[cfg(test)]
#[cfg(test)]
#[path = "ws_tests.rs"]
#[allow(clippy::await_holding_lock)]
mod tests;
