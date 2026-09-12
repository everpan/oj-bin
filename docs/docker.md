# oj Docker 方案（编译 / 测试 / 发布 / 验证 / 运行）

本文覆盖用 Docker 镜像分发 oj 的完整链路。镜像自带 glibc，发布后彻底摆脱宿主机
glibc 版本约束（原「`GLIBC_2.35 not found`」老系统报错问题随之消失）。

相关文件（仓库根目录）：

| 文件 | 作用 |
|---|---|
| `Dockerfile` | 多阶段构建：builder(`ubuntu:20.04`, glibc 2.31) → runtime(`distroless cc-debian11`) |
| `.dockerignore` | 构建上下文瘦身（排除 target/bin/dist/node_modules/运行期落盘数据 等） |
| `docker-compose.yml` | 挂载 `sample/` 以 dev 模式起服务的演示编排 |
| `sample/config.docker.yaml` | sample 配置变体：`server.host=0.0.0.0` + `console_log=true`（容器内必需） |

---

## 1. 背景与适用场景

- **为什么用镜像**：此前 Linux 发布物在 `ubuntu-latest`（glibc 2.39）构建，低版本系统运行报
  `GLIBC_2.35 not found`。容器构建把产物 glibc 需求锚定在 2.31，且镜像自带运行时
  （distroless，glibc 2.31 + libstdc++），与宿主机 glibc **完全解耦**。
- **镜像体积**：约 **56.7 MiB**（压缩 OCI）。体积主体是 V8 静态库链接进 `bin/oj` + 8 个插件
  cdylib，这部分是 deno_core/runtime 固有，镜像手段无法再压；切换 distroless 已去掉 ubuntu
  基础镜像与 apt 元数据开销。
- **本地镜像名 vs 远端名**：仓库内统一用本地名 `oj-bin/oj`；推送到 Docker Hub 时命名空间必须是
  你的账号，最终远端名为 `everpan/oj`（见 §5）。

> ⚠️ **架构注意（重要）**：本机为 Apple Silicon，**默认构建产出 `linux/arm64` 镜像**。
> 若部署目标是 **x86_64 Linux 服务器**（即最初报 GLIBC 错误的那类机器），需要
> `linux/amd64` 镜像，否则镜像无法在该机器运行。详见 §8。

---

## 2. 前置条件

- 容器运行时二选一：
  - **Apple `container` CLI**（本机推荐，v1.4+）：`container --version`
  - 或 Docker / Docker Desktop
- 构建资源：建议 **4 CPU / 8 GiB**（V8 静态库链接吃内存，低于此易 OOM）
- 发布所需：Docker Hub 账号 `everpan` 的 **Access Token**（Settings → Security 生成，
  勾选 `read:write` 或 `write:packages` 等价权限）

---

## 3. 编译（Build）

### 3.1 Apple `container`

首次需启动构建器实例（分配资源）：

```bash
container builder start -c 4 -m 8G
```

构建镜像（本地名 `oj-bin/oj`）：

```bash
container build -c 4 -m 8G -t oj-bin/oj .
```

### 3.2 Docker

```bash
docker build -t oj-bin/oj .
```

### 3.3 构建期内自动执行的门禁

`Dockerfile` 在 builder 阶段依次执行，任一失败即中断构建：

1. `cargo xtask build` —— 构建 oj + 全部第一方插件（归置到 `bin/`） + devkit
2. `cargo xtask smoke --bin bin/oj` —— 发布门禁：隐藏构建机 JS 源后产物仍须能完成 `oj build`
   （防止 deno_core 把扩展 JS 绝对路径烧进二进制，v0.1.12 教训）
3. `strip bin/oj bin/plugins/*/*.so` —— 压低产物体积
4. **glibc 2.31 基线自检** —— 扫描 `bin/oj` 与全部插件 cdylib 引用的 `GLIBC_2.x` 符号，
   最大值超过 2.31 则 fail-fast（提示基线被破坏，而非让用户到老系统上才撞错）

构建成功后得到本地镜像 `oj-bin/oj:latest`。确认体积：

```bash
container image inspect oj-bin/oj:latest | grep -E '"size"'
# 约 59475690 字节 ≈ 56.7 MiB
```

---

## 4. 测试（Test）

### 4.1 构建期门禁（自动）

上面 §3.3 的 smoke 门禁 + glibc 自检即为「构建即测试」，无需额外动作。

### 4.2 运行期冒烟（推荐）

起服务后探活（见 §7 运行），预期：

- `GET /v1/api/health` → **200** `{"status":"OK",...}`
- `GET /v1/api/user/account/?id=1` → **401**（该路径不在匿名列表，JWT 守卫正确拦截，符合预期）

### 4.3 JS 业务测试（可选，镜像内）

镜像内置 `oj` 命令，可覆盖 entrypoint 直接跑 `oj test`：

```bash
container run --rm --entrypoint oj \
  --mount type=bind,source="$PWD/sample",target=/app \
  oj-bin/oj:latest test -c config.docker.yaml --format human
```

### 4.4 Rust 单测 / e2e

不在镜像内执行。开发机直接跑（见 `docs/dev-guide.md`）：

```bash
cargo test --release -p oj --test e2e
```

---

## 5. 发布（Publish）

> 命名空间约束：Docker Hub 镜像名格式为 `<命名空间>/<仓库>`，命名空间必须是你拥有的账号或组织。
> 本地名 `oj-bin/oj` 中的 `oj-bin` 是 GitHub 仓库目录名，**不是** Docker Hub 命名空间，
> 因此远端只能发布为 **`everpan/oj`**（`everpan` 为登录账号）。若想保留 `oj-bin` 字样，
> 需先在 Docker Hub 创建名为 `oj-bin` 的组织，再发布为 `oj-bin/oj`。

### 5.1 取版本号

```bash
awk -F'"' '/^version =[[:space:]]*"/{print $2; exit}' oj/Cargo.toml   # 例：0.1.14
VER=$(awk -F'"' '/^version =[[:space:]]*"/{print $2; exit}' oj/Cargo.toml)
```

### 5.2 打版本 tag（本地）

```bash
container image tag oj-bin/oj:latest everpan/oj:$VER
container image tag oj-bin/oj:latest everpan/oj:latest
```

### 5.3 登录 Docker Hub

⚠️ **Apple `container` 的已知坑**：交互式 `container registry login docker.io` 会报
`refusing insecure credential exchange`（Docker Hub token 端点自身发 challenge，`container`
  安全守卫拒绝交出凭证）。**必须用 `--password-stdin` 形态**：

```bash
echo "$DOCKERHUB_TOKEN" | container registry login --username everpan --password-stdin docker.io
# 校验：container registry list  应出现 registry-1.docker.io / everpan
```

（`$DOCKERHUB_TOKEN` 为 Docker Hub Access Token，不是账号密码。）

### 5.4 推送

```bash
container image push everpan/oj:$VER
container image push everpan/oj:latest
```

> 偶发 `XPC connection error: Connection interrupted`（构建器 VM 瞬时抖动）时，blob 已上传，
> 直接重跑 `container image push` 即可秒过。

### 5.5 验证发布

```bash
docker pull everpan/oj:0.1.14   # 任意装了 docker 的机器
```

---

## 6. 验证（Verify）

完整验证 = 构建门禁 + 体积 + 运行探活：

```bash
# 1) 体积
container image inspect oj-bin/oj:latest | grep -E '"size"'

# 2) 起服务（见 §7），另开终端探活
curl -s -w '\n[%{http_code}]\n' localhost:9778/v1/api/health -H "X-TENANT-ID: t1"   # 200
curl -s -o /dev/null -w '%{http_code}\n' "localhost:9778/v1/api/user/account/?id=1" -H "X-TENANT-ID: t1"  # 401

# 3)（可选）容器内 glibc 需求上限
container run --rm --entrypoint objdump oj-bin/oj:latest -T /usr/local/bin/oj \
  | grep -o 'GLIBC_2\.[0-9]*' | sort -uV | tail -1
# 应输出 GLIBC_2.30 或更低（≤ 2.31 基线）
```

---

## 7. 运行（Run）

### 7.1 一行起服务（前台）

```bash
container run --rm -p 9778:9778 \
  --mount type=bind,source="$PWD/sample",target=/app \
  oj-bin/oj -c config.docker.yaml --api-path src
```

### 7.2 后台起 + 命名（便于日志/停止）

```bash
container run -d --name oj-verify -p 9778:9778 \
  --mount type=bind,source="$PWD/sample",target=/app \
  oj-bin/oj:latest -c config.docker.yaml --api-path src

container logs oj-verify          # 看日志（config.docker.yaml 已开 console_log）
container stop  oj-verify         # 停止
container delete oj-verify        # 删除
```

### 7.3 docker-compose

```bash
docker compose up --build
# 等价于：构建 oj-bin/oj + 挂载 ./sample + 暴露 9778
```

### 7.4 配置与挂载要点

- **`server.host` 必须 `0.0.0.0`**：容器内绑 `localhost` 则宿主机端口映射访问不到。
  `sample/config.docker.yaml` 已设好；自写配置时务必改。
- **业务目录挂到 `/app`**：`config.yaml` + `src/`（dev）或 `dist/`（release）都在 `/app` 下；
  配置里相对路径（sqlite 库、uploads、证书）相对配置目录 `/app` 解析。
- **容器以 root 运行**：sqlite / logs / uploads 直接写在挂载的 `/app` 内，产生的文件属主为
  root；如需指定属主可加 `--user 1000:1000`。
- **插件目录**：镜像已内置 `/usr/local/oj/plugins`（含 `<host-triple>/` 子目录），
  经 `OJ_PLUGINS_DIR` 发现；业务如需自带插件，挂载到该目录下对应 triple 子目录即可。
- **端口**：由 `config.server.port` 决定（EXPOSE 9778 仅为文档标注，sample 默认 9778）。

### 7.5 探活示例

```bash
curl localhost:9778/v1/api/health -H "X-TENANT-ID: t1"
# {"status":"OK","certificate_status":"valid",...}

curl "localhost:9778/v1/api/user/account/?id=1" -H "X-TENANT-ID: t1"
# 401（未带 JWT，符合预期）
```

---

## 8. 架构 / 跨平台（部署必读）

- 本机 Apple Silicon 构建默认产出 **`linux/arm64`**。
- 若部署目标是 **x86_64 Linux 服务器**，需 `linux/amd64` 镜像，否则无法运行。
  两种产出方式：
  - **CI 产出（推荐）**：在 `release.yml` 的 `ubuntu-latest`（x86_64）上
    `container build --platform linux/amd64 -t everpan/oj .` 后 push。CI 天然是 amd64，
    V8 原生编译快、无模拟开销。
  - **本机产出（不推荐）**：`container build --platform linux/amd64 ...` 需 QEMU 模拟，
    V8 编译极慢且可能不稳定。
- 若要 **单 tag 多架构**（arm64 + amd64 自动按宿主选择），CI 需用 buildx 打 multi-arch
  manifest（`container build` 当前为单架构）。

---

## 9. CI 集成（可选）

把镜像发布接进 `release.yml`：打 `v*` tag 时自动

1. `container builder start -c 4 -m 8G`（或复用 job 容器）
2. `container build --platform linux/amd64 -t everpan/oj:$VER .`
3. `echo $TOKEN | container registry login --username everpan --password-stdin docker.io`
4. `container image push everpan/oj:$VER` + `:latest`

这样发版即出镜像，与现有 GitHub Release / npm 发布同源。需要我补这段 workflow 时再说。
