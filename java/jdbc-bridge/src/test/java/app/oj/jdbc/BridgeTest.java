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

public class BridgeTest {

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
    void streamBatchesAt100() throws Exception {
        java.util.List<JSONObject> f = serveFrames(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:s100\"}",
                "{\"id\":2,\"op\":\"exec\",\"handle\":1,\"sql\":\"create table t (id int)\"}",
                "{\"id\":3,\"op\":\"exec\",\"handle\":1,\"sql\":\"insert into t select x from system_range(1,250)\"}",
                "{\"id\":4,\"op\":\"stream_open\",\"handle\":1,\"sql\":\"select id from t\"}",
                "{\"id\":5,\"op\":\"stream_next\",\"handle\":1,\"stream_id\":1}",
                "{\"id\":6,\"op\":\"stream_next\",\"handle\":1,\"stream_id\":1}",
                "{\"id\":7,\"op\":\"stream_next\",\"handle\":1,\"stream_id\":1}",
                "{\"id\":8,\"op\":\"stream_next\",\"handle\":1,\"stream_id\":1}",
                "{\"id\":9,\"op\":\"stream_close\",\"handle\":1,\"stream_id\":1}");
        assertEquals(9, f.size());
        assertEquals(1, f.get(3).getJSONObject("ok").getLong("stream_id"));
        assertEquals(100, f.get(4).getJSONObject("ok").getJSONArray("rows").length());
        assertEquals(100, f.get(5).getJSONObject("ok").getJSONArray("rows").length());
        assertEquals(50, f.get(6).getJSONObject("ok").getJSONArray("rows").length());
        assertTrue(f.get(7).getJSONObject("ok").getBoolean("done"));
        assertTrue(f.get(8).has("ok"), "expect ok frame: " + f.get(8));
    }

    @Test
    void cancelProducesExactlyOneResponse() throws Exception {
        java.util.List<JSONObject> f = serveFrames(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:cancel\"}",
                "{\"id\":2,\"op\":\"exec\",\"handle\":1,\"sql\":\"create alias sleep_if"
                        + " for \\\"app.oj.jdbc.BridgeTest.sleepIf(boolean, int)\\\"\"}",
                // H2 embedded 在 executeQuery 即全量求值（生产驱动的 setFetchSize 流式下慢在
                // next），故慢查询在途窗口落在 stream_open；cancel 须能取消执行中的 open。
                // H2 在扫描循环里高频检查 cancel 标志：2M 行扫描 + 第 1M 行 sleep 2s，
                // cancel 必然落在在途窗口内并在睡眠返回后的检查点中止（实测 3 行小查询
                // 无检查点，跑完才被 cancel，测不到中止）。
                "{\"id\":3,\"op\":\"stream_open\",\"handle\":1,\"sql\":\"select x as id"
                        + " from system_range(1,2000000) where sleep_if(mod(x,1000000)=0, 2000)\"}",
                "{\"id\":4,\"op\":\"stream_cancel\",\"handle\":1,\"stream_id\":1}",
                "{\"id\":5,\"op\":\"stream_close\",\"handle\":1,\"stream_id\":1}");
        assertEquals(5, f.size(), "每请求恰好一帧: " + f);
        java.util.Map<Long, JSONObject> byId = new java.util.HashMap<>();
        for (JSONObject fr : f) {
            long fid = fr.getLong("id");
            assertFalse(byId.containsKey(fid), "同一 id 恰好一帧（claimed 唯一性）: " + f);
            byId.put(fid, fr);
        }
        // cancel 帧绕过队列，可能与 open 帧乱序——按 id 匹配，不按位置
        JSONObject open = byId.get(3L);
        assertEquals("cancelled", open.getJSONObject("ok").getString("error"));
        assertTrue(byId.get(4L).has("ok") || byId.get(4L).has("err"), "cancel 帧须唯一: " + f);
        assertTrue(byId.get(5L).has("ok") || byId.get(5L).has("err"), "close 在取消后须单帧收尾: " + f);
    }

    @Test
    void sameConnOpsAreOrdered() throws Exception {
        java.util.List<JSONObject> f = serveFrames(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:order\"}",
                "{\"id\":2,\"op\":\"exec\",\"handle\":1,\"sql\":\"create alias sleep_if"
                        + " for \\\"app.oj.jdbc.BridgeTest.sleepIf(boolean, int)\\\"\"}",
                // 慢查询 1.5s：若并发执行，后面的快查询帧会先落地；FIFO 队列下响应序==请求序
                "{\"id\":3,\"op\":\"query\",\"handle\":1,\"sql\":\"select x from"
                        + " system_range(1,1) where sleep_if(true, 1500)\"}",
                "{\"id\":4,\"op\":\"query\",\"handle\":1,\"sql\":\"select 1 as v\"}",
                "{\"id\":5,\"op\":\"query\",\"handle\":1,\"sql\":\"select 2 as v\"}");
        assertEquals(5, f.size());
        for (int i = 0; i < 5; i++) {
            assertEquals(i + 1, f.get(i).getLong("id"), "响应序须与请求序一致: " + f);
        }
    }

    @Test
    void featureNotSupportedErrIsStructuredWithUnsupportedMessage() {
        JSONObject fns = (JSONObject) Bridge.errMsg(new java.sql.SQLFeatureNotSupportedException("no cubes"));
        assertEquals("unsupported: no cubes", fns.getString("message"));
        assertEquals(0, fns.getInt("error_code"));
        // SQLFeatureNotSupportedException 不带 SQLState → key 缺席（org.json put-null 删键）。
        assertFalse(fns.has("sql_state"));
        // 非 SQLException 仍回裸串（codec 两种形态都认）。
        assertEquals("IllegalStateException", Bridge.errMsg(new IllegalStateException()));
    }

    @Test
    void sqlExceptionErrCarriesSqlStateAndErrorCode() {
        JSONObject err = (JSONObject) Bridge.errMsg(new java.sql.SQLException("dup", "23505", 1234));
        assertEquals("dup", err.getString("message"));
        assertEquals("23505", err.getString("sql_state"));
        assertEquals(1234, err.getInt("error_code"));
    }

    @Test
    void realH2UniqueViolationErrFrameIsStructured() throws Exception {
        java.util.List<JSONObject> f = serveFrames(
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:uverr\"}",
                "{\"id\":2,\"op\":\"exec\",\"handle\":1,\"sql\":\"create table t (id int primary key)\"}",
                "{\"id\":3,\"op\":\"exec\",\"handle\":1,\"sql\":\"insert into t values (1)\"}",
                "{\"id\":4,\"op\":\"exec\",\"handle\":1,\"sql\":\"insert into t values (1)\"}");
        JSONObject err = f.get(3);
        assertTrue(err.has("err"), "唯一键冲突须为 err 帧: " + err);
        JSONObject e = err.getJSONObject("err");
        assertEquals("23505", e.getString("sql_state"), "H2 唯一键冲突 SQLState: " + e);
        assertTrue(e.getInt("error_code") != 0, "H2 唯一键冲突 vendor code: " + e);
        // 帧间解析往返：err 值是对象而非裸串。
        assertTrue(err.toString().contains("\"sql_state\":\"23505\""), "线上形态为结构化对象: " + err);
    }

    /** H2 CREATE ALIAS 反射入口：cond 为真则睡眠 ms（cancel 测试用慢查询），恒返回 true。 */
    @SuppressWarnings("unused")
    public static boolean sleepIf(boolean cond, int ms) {
        if (cond) {
            try {
                Thread.sleep(ms);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
        return true;
    }

    private static java.util.List<JSONObject> serveFrames(String... reqs) throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(String.join("\n", reqs) + "\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));
        java.util.List<JSONObject> frames = new java.util.ArrayList<>();
        for (String l : sw.toString().split("\n")) {
            if (!l.isEmpty()) {
                frames.add(new JSONObject(l));
            }
        }
        return frames;
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
