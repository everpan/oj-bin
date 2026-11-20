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
import java.util.Properties;

/**
 * NDJSON 桥：stdin 读请求帧，stdout 写响应帧。
 * 请求 {"id":u64,"op":"...",...}；响应 {"id":u64,"ok":{...}} / {"id":u64,"err":"..."}。
 */
public final class Bridge {
    static final String HELLO = "{\"hello\":{\"proto\":1,\"bridge\":\"0.1.0\"}}";

    private Bridge() {}

    public static void main(String[] args) throws Exception {
        // 协议写原始 fd 1：DriverLoader 会把 System.out 重定向到 stderr
        BufferedWriter out = new BufferedWriter(
                new OutputStreamWriter(new FileOutputStream(FileDescriptor.out), StandardCharsets.UTF_8));
        out.write(HELLO);
        out.newLine();
        out.flush();
        DriverLoader.load(Paths.get(args.length > 0 ? args[0] : "drivers"));
        BufferedReader in = new BufferedReader(
                new InputStreamReader(System.in, StandardCharsets.UTF_8));
        serve(in, out);
        Session.shutdownAll();
    }

    /** 逐行读请求直至 EOF；任何单请求异常（含 Error）只回 err 帧，不杀循环。 */
    static void serve(BufferedReader in, BufferedWriter out) throws IOException {
        String line;
        while ((line = in.readLine()) != null) {
            if (line.isEmpty()) {
                continue;
            }
            long id = -1; // 帧 id 不可解析时的哨兵（契约 id:u64 之外的容错值）
            JSONObject resp;
            try {
                JSONObject req = new JSONObject(line);
                id = req.optLong("id", -1);
                resp = new JSONObject().put("id", id).put("ok", dispatch(req));
            } catch (Throwable t) {
                String msg = t.getMessage() != null ? t.getMessage() : t.getClass().getSimpleName();
                resp = new JSONObject().put("id", id).put("err", msg);
            }
            out.write(resp.toString());
            out.newLine();
            out.flush();
        }
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
            default:
                throw new IllegalArgumentException("unknown op: " + op);
        }
    }
}
