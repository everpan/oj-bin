import { sessionKey } from "../_shared/session";

export default {
  async post() {
    const token = String((http.body || {}).refresh_token ?? "");
    await kv.del(sessionKey(token));
    json.ok(null);
  },
};
