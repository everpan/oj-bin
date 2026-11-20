package app.oj.jdbc;

import org.json.JSONArray;
import org.json.JSONObject;

import java.io.StringReader;
import java.sql.PreparedStatement;
import java.sql.Types;
import java.util.Base64;
import java.util.List;

/**
 * 参数绑定的唯一入口（T4/5/6 复用）：JSON 值按位绑定 PreparedStatement。
 * null 用 Types.VARCHAR——Oracle/达梦对 Types.NULL 报 "Invalid column type"。
 */
final class RowCodec {
    /** ORA-01461 防线：超阈值字符串切 setClob（Oracle/DM 超长隐式转 CLOB 报错） */
    static final int CLOB_THRESHOLD = 4000;
    static final String BLOB_PREFIX = "$blob:";

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
}
