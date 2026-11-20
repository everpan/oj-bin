package app.oj.jdbc;

import java.sql.Connection;
import java.sql.DriverManager;
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
