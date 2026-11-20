package app.oj.jdbc;

import org.json.JSONObject;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.StringReader;
import java.io.StringWriter;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Properties;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

class BridgeTest {

    @BeforeEach
    void resetSessions() {
        Session.resetForTests();
    }

    @Test
    void connectReturnsHandleAndEofEndsServe() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:t1\"}\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw)); // EOF → serve 返回
        String[] lines = sw.toString().split("\n");
        assertEquals(1, lines.length);
        JSONObject resp = new JSONObject(lines[0]);
        assertEquals(1, resp.getLong("id"));
        assertTrue(resp.has("ok"), "expect ok frame: " + resp);
        assertEquals(1, resp.getJSONObject("ok").getInt("handle"));
        assertFalse(resp.has("err"));
    }

    @Test
    void connectViaDriversDirOnly() throws Exception {
        // 模拟生产形态（java -jar，classpath 无驱动）：清空 classpath 驱动的注册，
        // 此时唯一候选是 driversDir 里 child-first UCL 独家装载的驱动——
        // 其对 app CL 不可见，未经 DriverShim 注册会被 isDriverAllowed 拒绝。
        for (var e = java.sql.DriverManager.getDrivers(); e.hasMoreElements(); ) {
            java.sql.DriverManager.deregisterDriver(e.nextElement());
        }
        Class<?> h2 = Class.forName("org.h2.Driver");
        Path jar = Path.of(h2.getProtectionDomain().getCodeSource().getLocation().toURI());
        Path drivers = Files.createTempDirectory("oj-drivers");
        Files.copy(jar, drivers.resolve(jar.getFileName().toString()));
        DriverLoader.load(drivers);
        Session s = Session.connect("jdbc:h2:mem:t3", new Properties());
        assertNotNull(s.conn());
        s.disconnect();
    }

    @Test
    void unknownOpReturnsErrAndServeContinues() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(
                "{\"id\":1,\"op\":\"bogus\"}\n"
                        + "{\"id\":2,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:t2\"}\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));
        String[] lines = sw.toString().split("\n");
        assertEquals(2, lines.length, "err 帧后循环须继续");
        JSONObject err = new JSONObject(lines[0]);
        assertEquals(1, err.getLong("id"));
        assertTrue(err.has("err"), "expect err frame: " + err);
        JSONObject ok = new JSONObject(lines[1]);
        assertEquals(2, ok.getLong("id"));
        assertTrue(ok.has("ok"), "expect ok frame: " + ok);
    }
}
