// CAS 2.0 跳转腿：302 到 {login_url}?service={callback_url}。
// service 用装配期配置的固定绝对 URL（禁从请求 Host 派生），与校验腿同串
// （CAS 按串精确匹配）；本腿带不了自定义头/租户头，须进两条匿名列表。
import { redirect } from "#/auth/_shared/util";
import { providerOf } from "../../_shared/core";

export default {
  get() {
    const name = String(http.query.provider ?? "");
    const p = providerOf(name);
    if (!p || p.kind !== "cas2") { json.fail(400, `unknown cas provider: ${name}`); return; }
    redirect(`${p.login_url}?service=${encodeURIComponent(p.callback_url ?? "")}`);
  },
};
