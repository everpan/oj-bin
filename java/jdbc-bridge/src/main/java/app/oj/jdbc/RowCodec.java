package app.oj.jdbc;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.InputStream;
import java.io.StringReader;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.sql.Blob;
import java.sql.Clob;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.Time;
import java.sql.Timestamp;
import java.sql.Types;
import java.time.OffsetDateTime;
import java.time.temporal.ChronoUnit;
import java.util.Base64;
import java.util.List;

/**
 * 参数绑定 + 结果集解码的唯一入口（T4/5/6 复用）：
 * 绑定时 JSON 值按位绑定 PreparedStatement；解码时按 spec §5.4 表逐行分流。
 * null 绑定用 Types.VARCHAR——Oracle/达梦对 Types.NULL 报 "Invalid column type"。
 */
final class RowCodec {
    /** ORA-01461 防线：超阈值字符串切 setClob（Oracle/DM 超长隐式转 CLOB 报错） */
    static final int CLOB_THRESHOLD = 4000;
    /** spec §5.1/§5.4：单 cell LOB/文本上限 16MB，超出报结构化错误（v1 不做流式） */
    static final int CELL_MAX_CHARS = 16 * 1024 * 1024;
    static final int CELL_MAX_BYTES = 16 * 1024 * 1024;
    static final String BLOB_PREFIX = "$blob:";

    /** f64 安全整数边界：|v|≤2^53-1 直接出 JSON number，与宿主 JS number 语义对齐 */
    private static final BigInteger MAX_SAFE_INT = new BigInteger("9007199254740991");
    private static final BigInteger I64_MAX = BigInteger.valueOf(Long.MAX_VALUE);
    private static final BigInteger I64_MIN = BigInteger.valueOf(Long.MIN_VALUE);

    private RowCodec() {}

    static void bind(PreparedStatement ps, List<Object> params) throws Exception {
        if (params == null) {
            return;
        }
        for (int i = 0; i < params.size(); i++) {
            bindOne(ps, i + 1, params.get(i));
        }
    }

    private static void bindOne(PreparedStatement ps, int i, Object v) throws Exception {
        if (v == null || v == JSONObject.NULL) {
            ps.setNull(i, Types.VARCHAR);
        } else if (v instanceof Boolean) {
            ps.setBoolean(i, (Boolean) v);
        } else if (v instanceof Double || v instanceof Float || v instanceof java.math.BigDecimal) {
            ps.setDouble(i, ((Number) v).doubleValue());
        } else if (v instanceof Number) {
            ps.setLong(i, ((Number) v).longValue());
        } else if (v instanceof String) {
            bindString(ps, i, (String) v);
        } else if (v instanceof JSONObject || v instanceof JSONArray) {
            ps.setString(i, v.toString()); // 对象/数组先序列化（文档约定）
        } else {
            ps.setString(i, v.toString()); // 兜底可读降级
        }
    }

    private static void bindString(PreparedStatement ps, int i, String s) throws Exception {
        if (s.startsWith(BLOB_PREFIX)) {
            ps.setBytes(i, Base64.getDecoder().decode(s.substring(BLOB_PREFIX.length())));
        } else if (s.length() > CLOB_THRESHOLD) {
            ps.setClob(i, new StringReader(s));
        } else {
            ps.setString(i, s);
        }
    }

    /** spec §5.4 JDBC→JSON 表逐行实现；返回 JSONObject.NULL 表 SQL NULL。 */
    static Object cell(ResultSet rs, int i, ResultSetMetaData md) throws Exception {
        switch (md.getColumnType(i)) {
            case Types.BOOLEAN:
            case Types.BIT: {
                boolean b = rs.getBoolean(i);
                return rs.wasNull() ? JSONObject.NULL : b;
            }
            case Types.TINYINT:
            case Types.SMALLINT:
            case Types.INTEGER:
            case Types.BIGINT:
                return intFamily(rs.getBigDecimal(i));
            case Types.DECIMAL:
            case Types.NUMERIC:
                // DM8 把 INT/NUMBER(p,0) 上报 NUMERIC：scale==0 必须走整数族
                if (md.getScale(i) == 0) {
                    return intFamily(rs.getBigDecimal(i));
                }
                BigDecimal d = rs.getBigDecimal(i);
                return d == null ? JSONObject.NULL : d.toPlainString();
            case Types.FLOAT:
            case Types.DOUBLE:
            case Types.REAL: {
                double v = rs.getDouble(i);
                return rs.wasNull() ? JSONObject.NULL : v;
            }
            case Types.DATE:
            case Types.TIMESTAMP: {
                // toInstant() 是绝对时刻，toString 恒为 UTC "Z" 尾，不受 JVM 默认时区影响
                Timestamp ts = rs.getTimestamp(i);
                return ts == null ? JSONObject.NULL : ts.toInstant().toString();
            }
            case Types.TIME: {
                // JDBC 规范对 TIME 调 getTimestamp 未定义，必须单独 getTime
                Time t = rs.getTime(i);
                return t == null ? JSONObject.NULL
                        : t.toLocalTime().truncatedTo(ChronoUnit.SECONDS).toString();
            }
            case Types.TIMESTAMP_WITH_TIMEZONE: {
                // JDBC 4.2：getTimestamp 会丢 offset 造信息（spec §5.4）
                Object o = rs.getObject(i, OffsetDateTime.class);
                return o == null ? JSONObject.NULL : o.toString();
            }
            case Types.CLOB:
            case Types.NCLOB: {
                Clob c = rs.getClob(i);
                if (c == null) {
                    return JSONObject.NULL;
                }
                String s = c.getSubString(1, (int) Math.min(c.length(), CELL_MAX_CHARS + 1L));
                if (s.length() > CELL_MAX_CHARS) {
                    throw tooLarge(md, i);
                }
                return s;
            }
            case Types.CHAR:
            case Types.VARCHAR:
            case Types.LONGVARCHAR:
            case Types.NCHAR:
            case Types.NVARCHAR:
            case Types.LONGNVARCHAR: {
                String s = rs.getString(i);
                if (s == null) {
                    return JSONObject.NULL;
                }
                if (s.length() > CELL_MAX_CHARS) {
                    throw tooLarge(md, i);
                }
                return s;
            }
            case Types.BLOB: {
                Blob b = rs.getBlob(i);
                if (b == null) {
                    return JSONObject.NULL;
                }
                try (InputStream in = b.getBinaryStream()) {
                    return blobCell(in.readAllBytes(), md, i);
                }
            }
            case Types.BINARY:
            case Types.VARBINARY:
            case Types.LONGVARBINARY: {
                byte[] b = rs.getBytes(i);
                return b == null ? JSONObject.NULL : blobCell(b, md, i);
            }
            case Types.ARRAY: {
                java.sql.Array a = rs.getArray(i);
                if (a == null) {
                    return JSONObject.NULL;
                }
                JSONArray out = new JSONArray();
                for (Object e : (Object[]) a.getArray()) {
                    out.put(arrayElem(e));
                }
                return out;
            }
            case Types.STRUCT:
            case Types.REF:
            case Types.ROWID:
            case Types.OTHER:
            default: {
                // 可读降级（spec §5.4：文档标注）
                String s = rs.getString(i);
                return s == null ? JSONObject.NULL : s;
            }
        }
    }

    /** 整数族统一 getBigDecimal 判幅值：≤2^53-1 number；≤i64 marker；更大 u64 marker。 */
    private static Object intFamily(BigDecimal bd) {
        if (bd == null) {
            return JSONObject.NULL;
        }
        BigInteger bi = bd.toBigIntegerExact();
        if (bi.abs().compareTo(MAX_SAFE_INT) <= 0) {
            return bi.longValueExact();
        }
        if (bi.compareTo(I64_MAX) <= 0 && bi.compareTo(I64_MIN) >= 0) {
            return "$oj$i64:" + bi;
        }
        if (bi.signum() >= 0) {
            return "$oj$u64:" + bi;
        }
        return bi.toString(); // 负超 i64：spec 未覆盖，十进制降级
    }

    private static Object blobCell(byte[] b, ResultSetMetaData md, int i) throws Exception {
        if (b.length > CELL_MAX_BYTES) {
            throw tooLarge(md, i);
        }
        return BLOB_PREFIX + Base64.getEncoder().encodeToString(b);
    }

    /** ARRAY 元素递归（无列元数据，按 Java 运行时形态分流）。 */
    private static Object arrayElem(Object o) throws Exception {
        if (o == null) {
            return JSONObject.NULL;
        }
        if (o instanceof byte[]) {
            return BLOB_PREFIX + Base64.getEncoder().encodeToString((byte[]) o);
        }
        if (o instanceof BigDecimal) {
            BigDecimal d = (BigDecimal) o;
            return d.scale() <= 0 ? intFamily(d) : d.toPlainString();
        }
        if (o instanceof Number) {
            return ((Number) o).longValue();
        }
        if (o instanceof java.sql.Array) {
            JSONArray out = new JSONArray();
            for (Object e : (Object[]) ((java.sql.Array) o).getArray()) {
                out.put(arrayElem(e));
            }
            return out;
        }
        return o.toString();
    }

    private static CellTooLargeException tooLarge(ResultSetMetaData md, int i) throws Exception {
        return new CellTooLargeException(md.getColumnLabel(i));
    }
}
