// 企微机器人回调骨架（spec 2026-11-11 P1-P3 的落地锚点，配套 docs/devkit/scenarios.md
// 场景 75）。演示 PATTERN：MsgId 去重 → obo 换签 → /mcp tools/call → 中文模板回复。
// 真实企微回调是 AES 加密 XML：接入时把「解密+验签」补在 parse() 处，其余照抄。
// 回调须 5s 内 ack——重活（多轮 tools/call、慢查询）搬 tasks 池异步 + sendMessage
// 主动推送，本骨架为可读性全程同步（示例任务量级 <1s）。

const OBO_TTL_GUARD_SECS = 3600; // MsgId 去重键保留 1h（企微重推窗口）
const OJ_ORIGIN = "http://127.0.0.1:9778"; // 部署期常量建议入 vars（vars.get("OJ_ORIGIN")）

// 解析回调体（占位：真实接入在此解密 XML + 核对 MsgSignature）。
function parse(body: unknown): { msgId: string; text: string; from: string } | null {
  const b = (body || {}) as Record<string, unknown>;
  const msgId = String(b.MsgId ?? "");
  if (!msgId) return null; // 非消息类回调（事件等）：ack 不处理
  return { msgId, text: String(b.text ?? ""), from: String(b.FromUserName ?? "") };
}

// obo 换签：svc token（本 bot 的机器凭据，admin/service-token 签发，roles:["bot"]）
// 换员工受托 token（10min，claims.act 指回本 bot——act_mask_destructive 据此遮蔽 delete）。
async function obo(svcToken: string, userid: string): Promise<string> {
  const r = await fetch(`${OJ_ORIGIN}/v1/api/auth/obo`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${svcToken}` },
    body: JSON.stringify({ provider: "wecom", external_sub: userid }),
  });
  const v = await r.json();
  if (!r.ok) throw new Error(`obo failed: ${v.code}`); // 40310-40313 → 告警 + 文明兜底话术
  return v.data.access_token as string;
}

// 经 /mcp 调工具：受托 token（act 携带）+ 三档 scopes 服务端收口——客户端不判权限。
async function callTool(token: string, name: string, args: unknown): Promise<string> {
  const r = await fetch(`${OJ_ORIGIN}/v1/api/mcp`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json",
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: Date.now(),
      method: "tools/call",
      params: { name, arguments: args },
    }),
  });
  const v = await r.json();
  if (v.error) throw new Error(`rpc ${v.error.code}`); // 含 [code=403] 权限拒绝（换签过期→重换签一次再试）
  return String(v.result?.content?.[0]?.text ?? JSON.stringify(v.result));
}

export default {
  // URL 合法性校验（企微接配置时的 echostr 原样回显——裸串，不走信封）。
  async get() {
    json.raw(String(http.query.echostr ?? "ok"));
  },

  async post() {
    const msg = parse(http.body);
    if (!msg) return json.ok(); // 事件类回调：立即 ack

    // MsgId 去重（incr 原子判首见；真 Redis 下多实例共享）。竞态窗口：incr 与
    // expire 之间宕机 → 键无 TTL 常驻，量级可忽略（骨架取舍，接入时可用 Lua/SET NX）。
    const dedupKey = `bot:wecom:msgid:${msg.msgId}`;
    if ((await kv.incr(dedupKey)) > 1) return json.ok(); // 重推：直接 ack
    await kv.expire(dedupKey, OBO_TTL_GUARD_SECS);

    // svc token 从哪来：启动时由运营侧签发一次（admin/service-token），经 vars
    // 注入（vars 声明键才可读）。真实接入建议存 kv 并定期轮换。
    const svcToken = String(vars.get("WECOM_BOT_SVC_TOKEN") ?? "");
    if (!svcToken) {
      return json.ok({ msg: "机器人凭据未配置：请以 admin 签发 svc token 并经 vars 注入。" });
    }

    let reply: string;
    try {
      const token = await obo(svcToken, msg.from);
      const who = await callTool(token, "user_account_get", { query: { id: "1" } });
      reply = `已收到：「${msg.text}」\n查询结果：${who}`;
    } catch (e) {
      reply = `抱歉，处理失败了，请稍后再试。（${String(e)}）`;
    }

    // 中文模板回复（占位：真实接入调企微 sendMessage API 主动推送；此处随 ack 带回）。
    return json.ok({ msg: reply });
  },
};
