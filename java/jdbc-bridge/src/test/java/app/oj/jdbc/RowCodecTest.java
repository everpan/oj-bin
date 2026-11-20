package app.oj.jdbc;

import org.json.JSONArray;
import org.json.JSONObject;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.StringReader;
import java.io.StringWriter;
import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.Statement;
import java.util.Arrays;
import java.util.Base64;
import java.util.List;
import java.util.Map;
import java.util.Properties;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

class RowCodecTest {

    private Connection conn;

    @BeforeEach
    void setup() throws Exception {
        Session.resetForTests();
        conn = DriverManager.getConnection("jdbc:h2:mem:codec");
        try (Statement st = conn.createStatement()) {
            st.execute("drop table if exists t");
            st.execute("create table t(a int, b varchar, c double, d boolean, e blob)");
        }
    }

    /** golden fixture 双侧共用契约：Rust 侧（Task 7/10）消费同一份文件。 */
    private static final Path FIXTURE =
            Path.of("../../plugins/oj-db-jdbc/fixtures/types.json");

    @Test
    void mapsResultSetCells() throws Exception {
        JSONArray fixture = new JSONArray(Files.readString(FIXTURE));
        for (int k = 0; k < fixture.length(); k++) {
            JSONObject entry = fixture.getJSONObject(k);
            String ddl = entry.optString("ddl", entry.getString("jdbc_type"));
            try (Statement st = conn.createStatement()) {
                st.execute("drop table if exists cell_t");
                st.execute("create table cell_t(v " + ddl + ")");
            }
            Object input = entry.isNull("input") ? null : entry.get("input");
            try (PreparedStatement ps = conn.prepareStatement("insert into cell_t values(?)")) {
                RowCodec.bind(ps, Arrays.asList(input));
                assertEquals(1, ps.executeUpdate(), entry.getString("jdbc_type") + " 插入");
            }
            Session s = Session.connect("jdbc:h2:mem:codec", new Properties());
            try {
                JSONArray rows = s.query("select v from cell_t", null);
                assertEquals(1, rows.length(), entry.getString("jdbc_type"));
                // JSON 序列化形态对齐比较（Integer/Long、null/JSONObject.NULL 归一）
                Object actual = rows.getJSONObject(0).get("V");
                assertEquals(new JSONArray().put(entry.get("expect")).toString(),
                        new JSONArray().put(actual).toString(),
                        entry.getString("jdbc_type") + " cell 映射");
            } finally {
                s.disconnect();
            }
        }
    }

    @Test
    void oversizedClobThrowsCellTooLarge() throws Exception {
        String big = "x".repeat(RowCodec.CELL_MAX_CHARS + 1);
        try (Statement st = conn.createStatement()) {
            st.execute("drop table if exists big_t");
            st.execute("create table big_t(v clob)");
        }
        try (PreparedStatement ps = conn.prepareStatement("insert into big_t values(?)")) {
            RowCodec.bind(ps, List.of(big));
            assertEquals(1, ps.executeUpdate());
        }
        Session s = Session.connect("jdbc:h2:mem:codec", new Properties());
        try {
            CellTooLargeException ex = assertThrows(CellTooLargeException.class,
                    () -> s.query("select v from big_t", null));
            assertTrue(ex.getMessage().contains("cell_too_large"), "err 帧须含结构化标记");
        } finally {
            s.disconnect();
        }
    }

    /** T3 遗留负向用例：绑参数目不匹配只回 err 帧，serve 循环不中断。 */
    @Test
    void paramCountMismatchReturnsErrAndLoopSurvives() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(String.join("\n",
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:mismatch\"}",
                "{\"id\":2,\"op\":\"query\",\"handle\":1,\"sql\":\"select ?\",\"params\":[1,2]}",
                "{\"id\":3,\"op\":\"query\",\"handle\":1,\"sql\":\"select 1 as x\"}")
                + "\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));

        String[] lines = sw.toString().split("\n");
        assertEquals(3, lines.length);
        JSONObject bad = new JSONObject(lines[1]);
        assertEquals(2, bad.getLong("id"));
        assertTrue(bad.has("err"), "参数不匹配须回 err 帧: " + bad);
        JSONObject good = new JSONObject(lines[2]);
        assertEquals(3, good.getLong("id"));
        assertTrue(good.has("ok"), "err 帧后循环须存活: " + good);
    }

    @Test
    void bindsEveryJsonShapeAndSelectsBack() throws Exception {
        List<Object> params = Arrays.asList(1, null, 1.5, true, "$blob:aGk=");
        PreparedStatement ps = conn.prepareStatement("insert into t values(?,?,?,?,?)");
        RowCodec.bind(ps, params);
        assertEquals(1, ps.executeUpdate());
        ps.close();

        Map<String, Object> row = selectSingleRow();
        assertEquals(5, row.size(), "5 列形状");
        assertEquals(1, ((Number) row.get("A")).intValue());
        assertNull(row.get("B"), "null 参数 → SQL NULL 往返");
        assertEquals(1.5, ((Number) row.get("C")).doubleValue(), 0.0);
        assertEquals(Boolean.TRUE, row.get("D"));
        // blob 列本任务占位断言（类型分流 T4 定型）：byte[] 或 $blob: 信封，内容须为 "hi"
        assertBlobContent(row.get("E"));
    }

    @Test
    void longStringSwitchesToClob() throws Exception {
        String big = "x".repeat(RowCodec.CLOB_THRESHOLD + 1);
        PreparedStatement ps = conn.prepareStatement("insert into t(a, b) values(?, ?)");
        RowCodec.bind(ps, List.of(1, big));
        assertEquals(1, ps.executeUpdate());
        ps.close();

        Map<String, Object> row = selectSingleRow();
        assertEquals(big, row.get("B"));
    }

    @Test
    void objectParamSerializedAsJson() throws Exception {
        PreparedStatement ps = conn.prepareStatement("insert into t(a, b) values(?, ?)");
        RowCodec.bind(ps, List.of(1, new JSONObject().put("k", 1)));
        assertEquals(1, ps.executeUpdate());
        ps.close();

        Map<String, Object> row = selectSingleRow();
        assertEquals("{\"k\":1}", row.get("B"));
    }

    @Test
    void bridgeQueryExecFrames() throws Exception {
        BufferedReader in = new BufferedReader(new StringReader(String.join("\n",
                "{\"id\":1,\"op\":\"connect\",\"url\":\"jdbc:h2:mem:frames\"}",
                "{\"id\":2,\"op\":\"exec\",\"handle\":1,"
                        + "\"sql\":\"create table t(a int, b varchar, c double, d boolean, e blob)\"}",
                "{\"id\":3,\"op\":\"exec\",\"handle\":1,"
                        + "\"sql\":\"insert into t values(?,?,?,?,?)\","
                        + "\"params\":[1,null,1.5,true,\"$blob:aGk=\"]}",
                "{\"id\":4,\"op\":\"query\",\"handle\":1,\"sql\":\"select a,b,c,d,e from t\"}")
                + "\n"));
        StringWriter sw = new StringWriter();
        Bridge.serve(in, new BufferedWriter(sw));

        String[] lines = sw.toString().split("\n");
        assertEquals(4, lines.length);
        JSONObject connect = new JSONObject(lines[0]);
        assertTrue(connect.has("ok"), "expect ok frame: " + connect);

        JSONObject exec = new JSONObject(lines[2]);
        assertEquals(3, exec.getLong("id"));
        assertEquals(1, exec.getInt("ok"), "exec 回受影响行数");

        JSONObject query = new JSONObject(lines[3]);
        assertEquals(4, query.getLong("id"));
        JSONObject row = query.getJSONObject("ok").getJSONArray("rows").getJSONObject(0);
        assertEquals(1, row.getInt("A"));
        assertTrue(row.isNull("B"));
        assertEquals(1.5, row.getDouble("C"), 0.0);
        assertTrue(row.getBoolean("D"));
        assertBlobContent(row.isNull("E") ? null : row.get("E"));
    }

    private Map<String, Object> selectSingleRow() throws Exception {
        List<Map<String, Object>> rows = queryDirect("select a, b, c, d, e from t");
        assertEquals(1, rows.size());
        return rows.get(0);
    }

    private List<Map<String, Object>> queryDirect(String sql) throws Exception {
        try (Statement st = conn.createStatement(); ResultSet rs = st.executeQuery(sql)) {
            var md = rs.getMetaData();
            int n = md.getColumnCount();
            List<Map<String, Object>> rows = new java.util.ArrayList<>();
            while (rs.next()) {
                Map<String, Object> row = new java.util.LinkedHashMap<>();
                for (int i = 1; i <= n; i++) {
                    row.put(md.getColumnLabel(i), rs.getObject(i));
                }
                rows.add(row);
            }
            return rows;
        }
    }

    private void assertBlobContent(Object cell) throws Exception {
        if (cell instanceof java.sql.Blob) { // 直接 getObject 的原始形态（占位）
            cell = ((java.sql.Blob) cell).getBinaryStream().readAllBytes();
        }
        if (cell instanceof byte[]) {
            assertArrayEquals("hi".getBytes(), (byte[]) cell);
        } else if (cell instanceof String && ((String) cell).startsWith("$blob:")) {
            assertArrayEquals("hi".getBytes(),
                    Base64.getDecoder().decode(((String) cell).substring("$blob:".length())));
        } else {
            throw new AssertionError("blob 占位形态未识别: " + (cell == null ? "null" : cell.getClass()));
        }
    }
}
