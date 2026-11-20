package app.oj.jdbc;

import org.json.JSONObject;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;

import java.io.BufferedReader;
import java.io.BufferedWriter;
import java.io.StringReader;
import java.io.StringWriter;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
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
