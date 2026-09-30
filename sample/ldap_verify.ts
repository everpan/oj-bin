// ldap_verify.ts —— 用 oj-ldap 插件复刻 PHP 的 AD 鉴证流程，验证插件正确性。
//
// 对应 PHP 逻辑：
//   1. 以服务账号（admin）绑定（LDAPv3，不跟随 referral —— 与 oj-ldap 默认一致）
//   2. 在 base 下按 samaccountname 搜索用户，取 cn / userPrincipalName / dn / samaccountname
//   3. 以域账号 `wiz\${account}` + 密码再做一次 simple_bind 鉴证（bind 返回 bool）
//   4. 汇总并返回用户信息
//
// 运行（需先 `cargo xtask plugin ldap` 构建 oj-ldap，并 `cargo xtask bin` 构建 oj）：
//   bin/oj exec sample/ldap_verify.ts -c sample/config.yaml -- <account> <password> [adminPassword]
//
// 说明：
//   - 服务账号 DN 写在 sample/config.yaml 的 `ldap.default.bind_dn`（DN 非密码，可留配置）；
//     服务账号**密码**优先取 config 的 `ldap.default.bind_pw`；config 未配时再退到运行时参数
//     <adminPassword>，不强制落配置/源码。
//   - search 的 opts.bindPw 仅在不传 <adminPassword> 时省略，此时 oj-ldap 回落 config 的
//     bind_pw（与 config bind_dn 合并），与 PHP 先 bind(admin) 再 search 一致；
//     whoami/compare 等仍用 config 服务账号。
//   - 用户密码鉴证走 `ldap.bind("wiz\\<account>", password)`，独立成连（ponytail 模型），
//     返回 true/false；false 表示 LDAP 拒绝凭据（rc≠0，含 49 invalidCredentials）。
//   - filter 中用户名经 esc() 转义（LDAP 版 SQL 注入防护；PHP 原样拼接，这里按文档建议补上）。

declare const args: string[];

// LDAP RFC 4515 过滤器转义（与 docs/ldap-integration.md §5 一致）。
const LDAP_FILTER_ESCAPES: Record<string, string> = {
  "\\": "\\5c",
  "*": "\\2a",
  "(": "\\28",
  ")": "\\29",
  "\0": "\\00",
};
const esc = (s: string): string =>
  s.replace(/[\\*()\0]/g, (c) => LDAP_FILTER_ESCAPES[c]);

// PHP 的 ldap_get_entries 把返回的属性名统一小写；oj-ldap 保留服务端原始大小写
// （如 sAMAccountName）。这里做大小写不敏感查找，复刻 PHP 行为。
function attrOf(
  attrs: Record<string, string[]>,
  name: string,
): string | undefined {
  const key = Object.keys(attrs).find(
    (k) => k.toLowerCase() === name.toLowerCase(),
  );
  return key ? attrs[key]?.[0] : undefined;
}

async function main(): Promise<void> {
  const account = args[0];
  const password = args[1]; // 被鉴证用户的密码（必填）
  const adminPw = args[2]; // 可选：服务账号 bindPw 密码；不传则采用 config 的 ldap.default.bind_pw
  if (!account || password === undefined) {
    throw new Error(
      "usage: oj exec sample/ldap_verify.ts -c sample/config.yaml -- <account> <password> [adminPassword]",
    );
  }

  // —— 步骤 1：服务账号绑定的连通性已由 oj-ldap 在 search 前置绑定完成 ——
  // PHP: ldap_connect + set_option(v3) + set_option(referrals=0) + ldap_bind(admin)
  // oj-ldap 恒用 LDAPv3 且不跟随 referral，config ldap.default 的 bind_dn/bind_pw 即服务账号。

  const base = "ou=鹏锐,dc=wiz,dc=top";
  const filter = `(&(objectCategory=Person)(samaccountname=${esc(account)}))`;
  const attrs = ["cn", "userPrincipalName", "dn", "samaccountname"];

  console.log(`[ldap] search base=${base}`);
  console.log(`[ldap] filter=${filter}`);

  // —— 步骤 2：目录查询（scope 缺省 "sub" == PHP 默认的整棵子树）——
  // 不传 <adminPassword> 时省略 bindPw，oj-ldap 回落 config 的 bind_pw（与 bind_dn 合并），不落配置。
  const searchOpts: Record<string, unknown> = { scope: "sub", filter, attrs };
  if (adminPw !== undefined) searchOpts.bindPw = adminPw;
  const entries = await ldap.search(base, searchOpts);
  if (entries.length === 0) {
    console.log(JSON.stringify({ login: false, reason: "user not found" }, null, 2));
    return;
  }
  if (entries.length > 1) {
    console.log(`[ldap] warn: ${entries.length} entries matched, use first`);
  }
  console.log(`[ldap] found ${entries.length} entries, use first`);
  console.log(`[ldap] entry `+JSON.stringify(entries[0], null, 2));
  const entry = entries[0];
  const info: Record<string, unknown> = {
    cn: attrOf(entry.attrs, "cn"), // 潘益伟
    account: attrOf(entry.attrs, "samaccountname"), // panyiwei
    dn: entry.dn, // CN=潘益伟,OU=研发部,OU=鹏锐,DC=wiz,DC=top
    email: (attrOf(entry.attrs, "userPrincipalName") ?? "").trim(),
  };

  // —— 步骤 3：以域账号鉴证密码（PHP: ldap_bind("wiz\\$account", $password)）——
  const domainUser = `wiz\\${account}`;
  const ok = await ldap.bind(domainUser, password); // true=成功；false=凭据被拒
  info.login = ok;
  info.from = "ldap";

  // —— 步骤 4：输出（exec 经 stdout 直出）——
  console.log(JSON.stringify(info, null, 2));
}

main().catch((e) => {
  console.error("[ldap] verify failed:", String((e && e.stack) || e));
  throw e; // 让 oj exec 以退出码 1 结束，便于 CI/脚本判定
});
