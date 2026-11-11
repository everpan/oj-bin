// admin 模块共享：运营端点门禁（service-token / bot-binding / obo-audit）。
// 与 cert/_shared/guard.ts 同形——admin role 判定不允许两处漂移，故各自模块内
// 就地声明（跨模块 import 会造成假的 deps 依赖边）。

export function isAdmin(): boolean {
  return !!http.user && ((http.user.roles as string[]) ?? []).includes("admin");
}

export function denyNonAdmin(): void {
  json.fail(403, "admin role required");
}
