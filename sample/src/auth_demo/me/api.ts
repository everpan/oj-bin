// 受保护路由：auth 启用时需带 Authorization: Bearer <access_token>；
// http.user = { id, roles, claims }（验签后的用户身份）。
export default {
  get() {
    json.ok({ user: http.user });
  },
};
