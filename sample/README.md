# oj sample — user/order/file

  cargo run -p oj -- server -c sample/config.yaml -d sample/src         # dev（TS，热重载；模式按目录自动判定）
  curl http://localhost:9778/v1/api/user/account/?id=1

  cargo run -p oj -- build -d sample/src -o sample/dist    # 构建（版本目录+manifests.yaml+tgz）
  cargo run -p oj -- server -c sample/config.yaml -d sample/dist       # release（按锁聚合）

- 路由 = 目录镜像：src/user/profile/detail/api.ts → /v1/api/user/profile/detail/
- config.yaml `server.root: dist`：API 未命中的 GET/HEAD 落静态（/manifests.yaml、
  /user-0.1.0.tgz 可直接访问；dist 无 index.html 故 / 为 404）
- dist/ 为 oj build 产物（保留原名原结构，默认 minify），可再生，勿手改
- node_modules/escape-goat 为直接 vendor 的纯 ESM 包（可 npm install 替换）
- db.sqlite 由 seed.sql 初始化（幂等），已 gitignore
