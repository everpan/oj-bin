package app.oj.jdbc;

import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Paths;
import java.sql.DriverManager;
import java.sql.SQLFeatureNotSupportedException;
import java.util.Properties;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;

/**
 * NDJSON 桥：stdin 读请求帧，stdout 写响应帧。
 * 请求 {"id":u64,"op":"...",...}；响应 {"id":u64,"ok":{...}} / {"id":u64,"err":"..."}。
 * 帧契约（u64 通道）：params 中以 "$oj$u64:<十进制>" 字符串形态出现的值 = u64 整数
 * （Rust 侧 jsint 标记 / 超 i64 裸整数预字符串化），RowCodec.bindString 剥前缀按
 * 整数字面量 setBigDecimal 精确承载。
 */
public final class Bridge {
    static final String HELLO = "{\"hello\":{\"proto\":1,\"bridge\":\"0.1.0\"}}";

    private Bridge() {}

    /** connect/disconnect 专用单线程 executor（v0.1.147 评审收口）：此前二者在
     *  stdin 读线程**内联**执行——一个网络黑洞 connect 会无限阻塞 dispatcher，
     *  所有会话的请求帧全停（全租户停摆）。单线程串行保持 connect/disconnect
     *  的先后语义；写帧与常规路径同一把锁（跨 conn 帧可交错，既有语义）。 */
    private static final ExecutorService CONNECT_EXEC =
            Executors.newSingleThreadExecutor(r -> {
                Thread t = new Thread(r, "oj-jdbc-connect");
                t.setDaemon(true);
                return t;
            });

    /** 登录超时秒数：环境变量 OJ_JDBC_LOGIN_TIMEOUT_SECS > 0 覆盖，缺省 30。 */
    private static int loginTimeoutSecs() {
        String v = System.getenv("OJ_JDBC_LOGIN_TIMEOUT_SECS");
        if (v != null) {
            try {
                int n = Integer.parseInt(v.trim());
                if (n > 0) {
                    return n;
                }
            } catch (NumberFormatException ignored) {
                // 非法值按缺省 30s，不杀桥
            }
        }
        return 30;
    }

    public static void main(String[] args) throws Exception {
        // 协议写原始 fd 1：DriverLoader 会把 System.out 重定向到 stderr
        BufferedWriter out = new BufferedWriter(
                new OutputStreamWriter(new FileOutputStream(FileDescriptor.out), StandardCharsets.UTF_8));
        out.write(HELLO);
        out.newLine();
        out.flush();
        // v0.1.147（评审收口）：JDBC 登录超时——缺省时对网络黑洞（防火墙丢包无
        // RST）的 getConnection 会挂到 OS TCP 超时（分钟级）。默认 30s，
        // OJ_JDBC_LOGIN_TIMEOUT_SECS 可覆盖；socket 级超时属驱动 URL 参数，见
        // docs/plugins/jdbc.md 限制节。
        DriverManager.setLoginTimeout(loginTimeoutSecs());
        DriverLoader.load(Paths.get(args.length > 0 ? args[0] : "drivers"));
        BufferedReader in = new BufferedReader(
                new InputStreamReader(System.in, StandardCharsets.UTF_8));
        serve(in, out);
        Session.shutdownAll();
    }

    /** 逐行读请求直至 EOF；任何单请求异常（含 Error）只回 err 帧，不杀循环。
     *  connect/disconnect 在专用单线程 executor 执行（v0.1.147：不再内联阻塞读线程）；
     *  stream_cancel（独立线程执行 cancel，唯一绕过 per-conn 队列者）；其余请求按
     *  handle 投递到对应 Session 的单线程队列严格 FIFO。 */
    static void serve(BufferedReader in, BufferedWriter out) throws IOException {
        Object lock = new Object(); // 多 Session 执行线程并发写帧的互斥（不同 conn 的帧可交错）
        String line;
        while ((line = in.readLine()) != null) {
            if (line.isEmpty()) {
                continue;
            }
            long id = -1; // 帧 id 不可解析时的哨兵（契约 id:u64 之外的容错值）
            try {
                JSONObject req = new JSONObject(line);
                id = req.optLong("id", -1);
                String op = req.optString("op");
                if ("connect".equals(op) || "disconnect".equals(op)) {
                    // v0.1.147：移出读线程——读线程只负责收帧与分发，慢 connect
                    // 不再阻塞其他会话的请求处理（黑洞场景全桥停摆的修复）。
                    final long fid = id;
                    final BufferedWriter fout = out;
                    final Object flock = lock;
                    CONNECT_EXEC.submit(() -> {
                        JSONObject resp;
                        try {
                            resp = new JSONObject().put("id", fid).put("ok", dispatch(req));
                        } catch (Throwable t) {
                            resp = new JSONObject().put("id", fid).put("err", errMsg(t));
                        }
                        writeFrame(fout, flock, null, resp);
                    });
                    continue;
                }
                Session s = Session.byHandle(req.optInt("handle", -1));
                if (s == null) {
                    writeFrame(out, lock, null,
                            new JSONObject().put("id", id).put("err", "unknown handle: " + req.optInt("handle", -1)));
                    continue;
                }
                if ("stream_cancel".equals(op)) {
                    dispatchStreamCancel(req, s, out, lock, id);
                    continue;
                }
                final long fid = id;
                s.submit(() -> {
                    JSONObject resp;
                    try {
                        resp = new JSONObject().put("id", fid).put("ok", dispatch(req));
                    } catch (Throwable t) {
                        resp = new JSONObject().put("id", fid).put("err", errMsg(t));
                    }
                    writeFrame(out, lock, null, resp);
                });
            } catch (Throwable t) {
                writeFrame(out, lock, null, new JSONObject().put("id", id).put("err", errMsg(t)));
            }
        }
        // EOF：先等 connect/disconnect executor 收尾（有界 = 登录超时上界），
        // 再等各 Session 在途任务——响应全部落盘后再返回。
        CONNECT_EXEC.shutdown();
        try {
            if (!CONNECT_EXEC.awaitTermination(loginTimeoutSecs() + 5L, TimeUnit.SECONDS)) {
                CONNECT_EXEC.shutdownNow();
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            CONNECT_EXEC.shutdownNow();
        }
        Session.drainAll();
    }

    /** cancel 的 Statement.cancel() 由独立线程执行；cancel 线程与 dispatcher 路径
     *  抢同一 id 的响应帧，claimed 先赢者写，败者丢弃。 */
    private static void dispatchStreamCancel(JSONObject req, Session s, BufferedWriter out, Object lock, long id) {
        AtomicBoolean claimed = new AtomicBoolean();
        try {
            s.streamCancel(req.getLong("stream_id"),
                    e -> writeFrame(out, lock, claimed, new JSONObject().put("id", id).put("err", errMsg(e))));
            writeFrame(out, lock, claimed, new JSONObject().put("id", id).put("ok", new JSONObject()));
        } catch (Throwable t) {
            writeFrame(out, lock, claimed, new JSONObject().put("id", id).put("err", errMsg(t)));
        }
    }

    private static void writeFrame(BufferedWriter out, Object lock, AtomicBoolean claimed, JSONObject resp) {
        if (claimed != null && !claimed.compareAndSet(false, true)) {
            return; // 败者丢弃：cancel 响应唯一性
        }
        synchronized (lock) {
            try {
                out.write(resp.toString());
                out.newLine();
                out.flush();
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
        }
    }

    /** err 帧载荷：SQLException 走 §5.8 结构化形态 `{message,sql_state,error_code}`，
     *  其余 throwable 仍回裸串（codec 两种形态都认）。org.json 语义：put null 值 = 删键，
     *  故驱动不给 SQLState 时 key 天然缺席（Rust 侧 None → internal，不硬套）。 */
    static Object errMsg(Throwable t) {
        String msg = t.getMessage() != null ? t.getMessage() : t.getClass().getSimpleName();
        if (t instanceof SQLFeatureNotSupportedException) {
            msg = "unsupported: " + msg; // 驱动缺实现时结构化可辨（spec §5.3）
        }
        if (t instanceof java.sql.SQLException) {
            java.sql.SQLException e = (java.sql.SQLException) t;
            return new JSONObject()
                    .put("message", msg)
                    .put("sql_state", e.getSQLState()) // null → key 缺席（org.json put-null 删键）
                    .put("error_code", Integer.valueOf(e.getErrorCode()));
        }
        return msg;
    }

    private static Object dispatch(JSONObject req) throws Exception {
        String op = req.getString("op");
        switch (op) {
            case "connect": {
                Properties info = new Properties();
                if (req.has("user")) {
                    info.setProperty("user", req.optString("user", null));
                }
                if (req.has("password")) {
                    info.setProperty("password", req.optString("password", null));
                }
                JSONObject props = req.optJSONObject("props");
                if (props != null) {
                    for (String key : props.keySet()) {
                        info.setProperty(key, props.optString(key, null));
                    }
                }
                Session s = Session.connect(req.getString("url"), info);
                return new JSONObject().put("handle", s.handle);
            }
            case "disconnect": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                s.disconnect();
                return new JSONObject();
            }
            case "query": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                return new JSONObject().put("rows", s.query(req.getString("sql"), req.optJSONArray("params")));
            }
            case "exec": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                return s.exec(req.getString("sql"), req.optJSONArray("params"));
            }
            case "begin": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                return new JSONObject().put("tx_id", s.begin());
            }
            case "tx_query":
            case "tx_exec": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                long txId = req.getLong("tx_id");
                if (op.equals("tx_query")) {
                    return new JSONObject().put("rows", s.txQuery(txId, req.getString("sql"), req.optJSONArray("params")));
                }
                return s.txExec(txId, req.getString("sql"), req.optJSONArray("params"));
            }
            case "tx_commit":
            case "tx_rollback": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                long txId = req.getLong("tx_id");
                if (op.equals("tx_commit")) {
                    s.commit(txId);
                } else {
                    s.rollback(txId);
                }
                return new JSONObject();
            }
            case "stream_open": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                return s.streamOpen(req.getString("sql"), req.optJSONArray("params"));
            }
            case "stream_next": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                return s.streamNext(req.getLong("stream_id"));
            }
            case "stream_close": {
                Session s = Session.byHandle(req.getInt("handle"));
                if (s == null) {
                    throw new IllegalStateException("unknown handle: " + req.getInt("handle"));
                }
                s.streamClose(req.getLong("stream_id"));
                return new JSONObject();
            }
            default:
                throw new IllegalArgumentException("unknown op: " + op);
        }
    }
}
