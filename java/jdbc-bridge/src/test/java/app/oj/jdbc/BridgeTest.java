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
    void txCommitMakesRowVisible() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:txc\"}\n"
                        + "{\"id\":2,\"op\":\"exec\",\"handle\":1,\"sql\":\"create table t (id int, name varchar(20))\"}\n"
                        + "{\"id\":3,\"op\":\"begin\",\"handle\":1}\n"
                        + "{\"id\":4,\"op\":\"tx_exec\",\"handle\":1,\"tx_id\":1,\"sql\":\"insert into t values (1, 'a')\"}\n"
                        + "{\"id\":5,\"op\":\"tx_commit\",\"handle\":1,\"tx_id\":1}\n"
                        + "{\"id\":6,\"op\":\"query\",\"handle\":1,\"sql\":\"select id, name from t\"}\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));
        String[] lines = sw.toString().split("\n");
        JSONObject begin = new JSONObject(lines[2]);
        assertTrue(begin.has("ok"), "expect ok frame: " + begin);
        assertEquals(1, begin.getJSONObject("ok").getLong("tx_id"));
        JSONObject exec = new JSONObject(lines[3]);
        assertTrue(exec.has("ok"), "expect ok frame: " + exec);
        assertEquals(1, exec.getInt("ok"));
        JSONObject commit = new JSONObject(lines[4]);
        assertTrue(commit.has("ok"), "expect ok frame: " + commit);
        JSONObject rows = new JSONObject(lines[5]);
        assertEquals(1, rows.getJSONObject("ok").getJSONArray("rows").length());
    }

    @Test
    void txRollbackUndoesInsert() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:txr\"}\n"
                        + "{\"id\":2,\"op\":\"exec\",\"handle\":1,\"sql\":\"create table t (id int)\"}\n"
                        + "{\"id\":3,\"op\":\"begin\",\"handle\":1}\n"
                        + "{\"id\":4,\"op\":\"tx_exec\",\"handle\":1,\"tx_id\":1,\"sql\":\"insert into t values (1)\"}\n"
                        + "{\"id\":5,\"op\":\"tx_rollback\",\"handle\":1,\"tx_id\":1}\n"
                        + "{\"id\":6,\"op\":\"query\",\"handle\":1,\"sql\":\"select id from t\"}\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));
        String[] lines = sw.toString().split("\n");
        JSONObject rollback = new JSONObject(lines[4]);
        assertTrue(rollback.has("ok"), "expect ok frame: " + rollback);
        JSONObject rows = new JSONObject(lines[5]);
        assertEquals(0, rows.getJSONObject("ok").getJSONArray("rows").length());
    }

    @Test
    void nestedBeginReturnsErr() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:txn\"}\n"
                        + "{\"id\":2,\"op\":\"begin\",\"handle\":1}\n"
                        + "{\"id\":3,\"op\":\"begin\",\"handle\":1}\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));
        String[] lines = sw.toString().split("\n");
        JSONObject err = new JSONObject(lines[2]);
        assertEquals(3, err.getLong("id"));
        assertTrue(err.has("err"), "expect err frame: " + err);
    }

    @Test
    void unknownTxIdReturnsErr() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:txu\"}\n"
                        + "{\"id\":2,\"op\":\"tx_commit\",\"handle\":1,\"tx_id\":99}\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));
        String[] lines = sw.toString().split("\n");
        JSONObject err = new JSONObject(lines[1]);
        assertEquals(2, err.getLong("id"));
        assertTrue(err.has("err"), "expect err frame: " + err);
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
