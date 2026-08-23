# only js

构建命令行应用程序`oj`,包含以下子命令

## server
  执行 `oj server -c config.yaml` 将读取配置文件，启动一个web服务

  -c config.yaml 读取配置文件中的信息，启动服务，其中包含 host port db redis 等配置信息
  
  以下为一个例子
  ```yaml
    server:
      host: "localhost"
      port: 778
    db:
      default: "sqlite:///tmp/database.db"
      mysql: "mysql:host=localhost;dbname=test"
      pg: "postgresql://dbuser:pwd@localhost/testdb"
    redis:
      default: "redis://user:password@127.0.0.1:6379/1"
      other: "redis://user:password@127.0.0.1:6379/2"
  ```

  -b base 作为默认的服务基础路由，默认 /v1/api
   
  -d dir 以 `dir` 作为项目的服务目录，一个开发中(--dev)的服务目录如下
  ```
    dist   # 编译目录与src结构相同
    src    # 源码目录，首层子目录为模块名
    ├── moduleA
    │   └── featA # 子特性文件夹
    │   └── featB
    │   └── manifest.yaml # moduleA 的清单文件
    └── user
        ├── account
        └── profile
        └── manifest.yaml # user 模块的清单
  ```
   每个模块目录下包含一个配置文件 manifest.yaml 用于记录该模型的相关清单，其案例结构如下
  ```yaml
    name: "user" # 与模块名文件夹同名，即父目录名，约束条件
    desc: "用户信息相关，记录账号、地址等个人信息" # 模块的简短描述
    version: "0.1.0" # 当前的版本，打包的时候加入打包路径
    config: # 本模块的其他设置信息
  ```
### 访问规则
   --dev 开发模式下
   如果某个子文件夹中存在 `api.ts`, 如 `src/moduleA/featB/api.ts`, 则可以通过 `GET` `/v1/api/moduleA/featB/` 来执行 `api.ts` 中导出的`get`方法
   --release 发布模式下，一般是编译文件目录，如 `-d dist/` 下存在各模块版本目录 `moduleA-{VERSION}/`，server 按 `dist/manifests.yaml` 锁定的版本加载各 `routes.js` 聚合路由，则可以通过 `GET` `/v1/api/moduleA/featB/` 来执行对应 `featB/api.js`（minified）中导出的`get`方法
   其中 `DELETE` 映射为 `del` 方法

   `api.ts` 伪代码如下
  ```ts
    import 'xxx/xxxxx/xx' // 导入需要模块
    function get(){
        let r = db.query("select * from user where id = ?", http.param("id",0))
        json.ok(r)
    }
    function post(){}
    // 
    export default {get,post}
  ```
   以上代码通过 `oj build moduleA` 命令来编译，制品存放在 `dist`，`build` 子命令为内置转译管线（swc）的包装。

## build
  开发者通过执行 `oj build user` 命令来编译 user 模块，`build` 子命令为内置转译管线（swc）的包装，基本过程如下
      - 校验 user/manifest.yaml 中的模块名是否与根目录相同，即模块名必须严格相同, 其中 version 记录为 {VERSION}
      - 遍历所有子文件夹的 `api.ts`, 并将其路径提取，例如 `user/profile/detail/api.ts` `API_PATH='user/profile/detail'`
      - 从api.ts中export的方法中，提取 route，如 `get.route={id}`, 则可以依此作为生产 `routes.js` 的路由项数据 `{ method: "get", pattern: "user/profile/detail/{id}", file: "detail/api.js" }`；`file` 相对版本目录根，子目录下的 api.ts 形如 `detail/api.js`（产物保留原名与目录结构，api.ts → 同目录 `api.js`）
      - pattern 无首斜杠、不含 base，以模块名段开头（`/` 开头的根级声明则剥首斜杠、不加模块段）
      - 全部 .ts 按原路径换 .js 扩展落盘（api.ts 同名 `api.js`，仅多一步剥 `.route` 声明），manifest.yaml 原样复制；跨模块相对导入（如 order 导入 `../user/_shared/validate`）构建期改写为指向目标模块版本目录的相对路径
      - 转译产物默认 minify（单行、剥注释——含内联 sourcemap），`--no-minify` 可关闭以得到可读产物排障
      - 最后将形成若干 `dist/user-{VERSION}/`（如 `detail/api.js`、`_shared/validate.js`）以及 `dist/user-{VERSION}/routes.js`, routes.js 中包含若干路由项数据
      - 同时形成压缩包文件 `dist/user-{VERSION}.tgz`, 用于整体发布（内容确定，同输入重复打包结果一致）

  参数：
```
    oj build [module] -d src -o dist [--no-minify]
```
      module  可选，指定要编译的模块名；缺省为全部模块
      -d      源码目录，默认 `src`
      -o      产物目录，默认 `dist`
      --no-minify  关闭产物 minify（多行可读，排障用；默认开启）

  产物树示例：
```
    dist
    ├── manifests.yaml              # 模块 → 锁定版本
    ├── user-0.1.0/
    │   ├── manifest.yaml           # 原样复制
    │   ├── routes.js               # 本模块路由表
    │   ├── _shared/validate.js     # 非 api.ts，原路径换 .js（minified）
    │   └── account/api.js          # api.ts 产物，原名原目录（minified）
    ├── user-0.1.0.tgz
    └── order-0.1.0/ ...
```

  `dist/manifests.yaml` 记录模块 → 版本的锁定关系（如 `user: 0.1.0`），每次构建 upsert 对应模块；版本升级后旧版本目录保留，可多版本共存，锁定文件始终指向当前版本。server release 模式按 `dist/manifests.yaml` 逐模块校验（白名单、版本目录存在、manifest name 与模块一致）后加载各 `routes.js` 聚合路由，任何校验失败即启动报错（fail-fast）。

