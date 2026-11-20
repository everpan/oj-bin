package app.oj.jdbc;

import org.json.JSONArray;
import org.json.JSONObject;

import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.util.Map;
import java.util.Properties;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.atomic.AtomicInteger;

/** 每 handle 一条 JDBC Connection；静态注册表供 serve 循环按 id 寻址。 */
final class Session {
    private static final Map<Integer, Session> REGISTRY = new ConcurrentHashMap<>();
    private static final AtomicInteger NEXT = new AtomicInteger(1);

    final int handle;
    private final Connection conn;
    private long nextTxId = 1; // tx_id 从 1 递增；0 表示无活动事务
    private long activeTx;

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
        conn.setAutoCommit(true);
        activeTx = 0;
    }

    void disconnect() {
        REGISTRY.remove(handle);
        try {
            conn.close();
        } catch (Exception ignored) {
            // 关闭失败不阻断其余会话清理
        }
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
