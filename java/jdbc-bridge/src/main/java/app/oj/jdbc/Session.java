package app.oj.jdbc;

import org.json.JSONArray;
import org.json.JSONObject;

import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Statement;
import java.util.Map;
import java.util.Properties;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Consumer;

/** 每 handle 一条 JDBC Connection + 一条单线程 FIFO 执行队列（Connection 非线程安全，
 *  同 conn 的 op 严格有序；仅 Statement.cancel() 允许跨线程）。 */
final class Session {
    private static final Map<Integer, Session> REGISTRY = new ConcurrentHashMap<>();
    private static final AtomicInteger NEXT = new AtomicInteger(1);

    final int handle;
    private final Connection conn;
    private final ExecutorService exec = Executors.newSingleThreadExecutor(r -> {
        Thread t = new Thread(r, "oj-jdbc-session");
        t.setDaemon(true); // 进程退出不被执行线程拖住
        return t;
    });
    private final Map<Long, Stream> streams = new ConcurrentHashMap<>();
    private long nextTxId = 1; // tx_id 从 1 递增；0 表示无活动事务
    private long activeTx;
    private long nextStreamId = 1; // stream_id 从 1 递增

    /** 在途/已取消流：rs 仅 Session 线程访问，cancel 线程只碰 stmt.cancel()。 */
    private static final class Stream {
        final Statement stmt;
        ResultSet rs; // streamOpen 中执行成功后挂上
        volatile boolean cancelled;

        Stream(Statement stmt) {
            this.stmt = stmt;
        }
    }

    private Session(int handle, Connection conn) {
        this.handle = handle;
        this.conn = conn;
    }

    static Session connect(String url, Properties info) throws Exception {
        Object driverClass = info.remove("driver_class");
        if (driverClass instanceof String) {
            DriverLoader.ensureDriver((String) driverClass);
        }
        Connection conn = DriverManager.getConnection(url, info);
        Session s = new Session(NEXT.getAndIncrement(), conn);
        REGISTRY.put(s.handle, s);
        return s;
    }

    static Session byHandle(int handle) {
        return REGISTRY.get(handle);
    }

    Connection conn() {
        return conn;
    }

    /** 请求入 per-conn 队列，严格 FIFO 执行。 */
    void submit(Runnable task) {
        exec.submit(task);
    }

    /** 等队列中在途任务全部收尾（barrier 落队尾，天然排在已入队任务之后）。 */
    private void drain() {
        try {
            exec.submit(() -> {}).get();
        } catch (Exception ignored) {
            // 队列已关闭等场景：尽力而为
        }
    }

    static void drainAll() {
        REGISTRY.values().forEach(Session::drain);
    }

    /** 查询：RowCodec 绑参 + 逐 cell 按 spec §5.4 类型分流。 */
    JSONArray query(String sql, JSONArray params) throws Exception {
        try (PreparedStatement ps = conn.prepareStatement(sql)) {
            RowCodec.bind(ps, params == null ? null : params.toList());
            try (ResultSet rs = ps.executeQuery()) {
                ResultSetMetaData md = rs.getMetaData();
                int n = md.getColumnCount();
                JSONArray rows = new JSONArray();
                while (rs.next()) {
                    JSONObject row = new JSONObject();
                    for (int i = 1; i <= n; i++) {
                        row.put(md.getColumnLabel(i), RowCodec.cell(rs, i, md));
                    }
                    rows.put(row);
                }
                return rows;
            }
        }
    }

    /** 执行：RowCodec 绑参，回受影响行数。 */
    int exec(String sql, JSONArray params) throws Exception {
        try (PreparedStatement ps = conn.prepareStatement(sql)) {
            RowCodec.bind(ps, params == null ? null : params.toList());
            return ps.executeUpdate();
        }
    }

    /** 开流：PreparedStatement + setFetchSize(500)（spec §5.6 shortcut：硬编码，
     *  出现具体驱动问题时再外提）。先登记 stream_id 再执行——cancel 才可能命中
     *  执行中的 open（H2 embedded 在 executeQuery 全量求值，慢查询在途窗口在 open；
     *  流式驱动慢在 next）。被取消的 SQLException 归一为 {"error":"cancelled"}。 */
    JSONObject streamOpen(String sql, JSONArray params) throws Exception {
        PreparedStatement ps = conn.prepareStatement(sql);
        try {
            ps.setFetchSize(500);
            RowCodec.bind(ps, params == null ? null : params.toList());
            long id = nextStreamId++;
            Stream st = new Stream(ps);
            streams.put(id, st);
            try {
                st.rs = ps.executeQuery();
            } catch (SQLException e) {
                streams.remove(id);
                if (st.cancelled) {
                    try {
                        ps.close();
                    } catch (Exception ignored) {
                        // 尽力而为
                    }
                    return new JSONObject().put("error", "cancelled");
                }
                throw e;
            }
            return new JSONObject().put("stream_id", id);
        } catch (Exception e) {
            ps.close();
            throw e;
        }
    }

    /** 拉一批：每帧 ≤100 行；无更多行 → {"done":true}；被取消驱动的 SQLException
     *  归一为 ok 值里的 {"error":"cancelled"}（spec §5.3）。 */
    JSONObject streamNext(long streamId) throws Exception {
        Stream st = streamOrThrow(streamId);
        try {
            ResultSetMetaData md = st.rs.getMetaData();
            int n = md.getColumnCount();
            JSONArray rows = new JSONArray();
            while (rows.length() < 100 && st.rs.next()) {
                JSONObject row = new JSONObject();
                for (int i = 1; i <= n; i++) {
                    row.put(md.getColumnLabel(i), RowCodec.cell(st.rs, i, md));
                }
                rows.put(row);
            }
            if (rows.isEmpty()) {
                return new JSONObject().put("done", true);
            }
            return new JSONObject().put("rows", rows);
        } catch (SQLException e) {
            if (st.cancelled) {
                return new JSONObject().put("error", "cancelled");
            }
            throw e;
        }
    }

    /** cancel：置标志 + 独立线程调 Statement.cancel()（JDBC 规范允许跨线程的唯一调用，
     *  绕过 per-conn 队列）。cancel() 自身抛错经 onCancelError 回帧——与 dispatcher
     *  路径抢同一 id 的响应帧，由 claimed 保证恰好一帧。stream 可能尚未登记（open 在途），
     *  有界轮询等待。 */
    void streamCancel(long streamId, Consumer<Exception> onCancelError) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        Stream st;
        while ((st = streams.get(streamId)) == null) {
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("unknown stream_id: " + streamId + " [OJ-JDBC-STREAM-GONE]");
            }
            Thread.sleep(1);
        }
        final Stream target = st;
        target.cancelled = true;
        Thread t = new Thread(() -> {
            try {
                target.stmt.cancel();
            } catch (Exception e) {
                onCancelError.accept(e);
            }
        }, "oj-jdbc-cancel");
        t.setDaemon(true);
        t.start();
    }

    void streamClose(long streamId) {
        Stream st = streams.remove(streamId);
        if (st == null) {
            throw new IllegalStateException("unknown stream_id: " + streamId + " [OJ-JDBC-STREAM-GONE]");
        }
        try {
            st.rs.close();
        } catch (Exception ignored) {
            // 已取消/已坏的流，关闭尽力而为
        }
        try {
            st.stmt.close();
        } catch (Exception ignored) {
            // 同上
        }
    }

    private Stream streamOrThrow(long streamId) {
        Stream st = streams.get(streamId);
        if (st == null) {
            throw new IllegalStateException("unknown stream_id: " + streamId + " [OJ-JDBC-STREAM-GONE]");
        }
        return st;
    }

    /** 开事务：autoCommit 关闭；不支持嵌套（活动事务未收尾时再来 begin 即报错）。 */
    long begin() throws Exception {
        if (activeTx != 0) {
            throw new IllegalStateException("nested begin not supported, active tx: " + activeTx);
        }
        conn.setAutoCommit(false);
        activeTx = nextTxId++;
        return activeTx;
    }

    JSONArray txQuery(long txId, String sql, JSONArray params) throws Exception {
        checkTx(txId);
        return query(sql, params);
    }

    int txExec(long txId, String sql, JSONArray params) throws Exception {
        checkTx(txId);
        return exec(sql, params);
    }

    void commit(long txId) throws Exception {
        checkTx(txId);
        conn.commit();
        endTx();
    }

    void rollback(long txId) throws Exception {
        checkTx(txId);
        conn.rollback();
        endTx();
    }

    private void checkTx(long txId) {
        if (txId != activeTx) {
            throw new IllegalStateException("unknown tx_id: " + txId);
        }
    }

    private void endTx() throws Exception {
        try {
            conn.setAutoCommit(true);
        } finally {
            activeTx = 0; // setAutoCommit 异常时也不留撕裂状态
        }
    }

    void disconnect() {
        REGISTRY.remove(handle);
        drain(); // 排在途任务之后关闭，保持 FIFO（close 不抢 pending）
        try {
            conn.close();
        } catch (Exception ignored) {
            // 关闭失败不阻断其余会话清理
        }
        exec.shutdown();
    }

    static void shutdownAll() {
        REGISTRY.values().forEach(Session::disconnect);
    }

    /** 测试专用：复位注册表与句柄计数。 */
    static void resetForTests() {
        shutdownAll();
        NEXT.set(1);
    }
}
