# oj Docker 方案：多阶段构建（builder -> distroless runtime）。
#
# 镜像自带 glibc（distroless cc-debian11，glibc 2.31）+ libstdc++，与宿主机的
# glibc 版本彻底解耦——发布镜像后，老系统上的 `GLIBC_x.y not found` 问题消失。
#
# builder 仍锚定 glibc 2.31（ubuntu:20.04），使「万一从镜像里抽出的 bin/oj 直接跑」
# 在 Ubuntu 20.04+ / Debian 11+ 上也兼容；同时保证与运行期 glibc 自洽。
#
# 构建依赖（全部来自 apt，无 openssl/zlib 等外部原生库）：
#   - gcc/make：rusty_v8 链接 + sqlx bundled sqlite（libsqlite3-sys cc 编译）
#   - cmake  ：rdkafka-sys 从源码编 librdkafka（已关 ssl/sasl/zstd/lz4/libz，
#              见 plugins/oj-bus-kafka/Cargo.toml，Windows CI 同款约定）
#
# 用法：
#   docker build -t oj-bin/oj .
#   # 业务目录挂到 /app（含 config.yaml + src/ 或 dist/；server.host 须为 0.0.0.0）
#   docker run --rm -p 9778:9778 -v "$PWD:/app" oj-bin/oj
#   docker run --rm -p 9778:9778 -v "$PWD:/app" oj-bin/oj -c config.yaml --api-path dist
#   # sample 演示见 docker-compose.yml
#
# 运行期说明：distroless 无 shell，进程以 oj 主程序作为 PID 1 直接运行；
# sqlite/uploads/logs 写在挂载的 /app 内；端口由 config 的 server.port 决定
# （EXPOSE 仅为文档标注，sample 默认 9778）。

# ---- 构建阶段：glibc 2.31 基线 + rustup + xtask build + 发布门禁 ----
FROM ubuntu:20.04 AS builder

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        curl \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable

WORKDIR /build
# 构建上下文由 .dockerignore 瘦身：Rust 源码 + tests/（workspace 成员）
# + docs/ 与 sample/global.d.ts（devkit 素材）+ .cargo/（xtask alias）。
COPY . .

# 全量构建并归置：oj + 第一方插件（bin/plugins/<triple>/）+ devkit -> bin/
# （与 scripts/deploy.sh、CI release.yml 同一入口，产物同形）。
# 缓存挂载：registry/target 跨构建复用，源码变更不重编全部依赖。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo xtask build

# 发布门禁（与 CI 同款）：隐藏构建机 JS 源后产物必须仍能完成 `oj build`
# —— deno_core 0.411 曾把扩展 JS 绝对路径烧进二进制（v0.1.12 教训）。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo xtask smoke --bin /build/bin/oj

# glibc 基线自检：主程序 + 全部插件 cdylib 引用的 GLIBC_2.x 符号不得高于 2.31。
# 若未来依赖引入更高版本符号（如新版 glibc 才有的函数），此处 fail-fast 提示
# 基线被破坏，而不是等用户在老系统上撞 `GLIBC_x.y not found`。
# 注意：strip 后动态符号仍在，自检不受影响。
RUN strip bin/oj bin/plugins/*/*.so
RUN worst=0; \
    for f in bin/oj bin/plugins/*/*.so; do \
        m=$(objdump -T "$f" | grep -o 'GLIBC_2\.[0-9]*' | sort -uV | tail -1 | cut -d. -f2); \
        if [ -n "$m" ] && [ "$m" -gt "$worst" ]; then worst=$m; fi; \
    done; \
    echo "max glibc requirement: 2.$worst (baseline 2.31)"; \
    if [ "$worst" -gt 31 ]; then \
        echo "::error:: glibc 基线被破坏（需要 2.$worst > 2.31），检查依赖是否引入新版符号" >&2; \
        exit 1; \
    fi

# ---- 运行阶段：distroless cc（glibc + libstdc++，已含 ca-certificates，约 20MB）----
# 镜像自带 glibc，与宿主机的 glibc 版本彻底解耦——发布镜像后不再受老系统 GLIBC_x.y
# 报错的困扰；二进制只需和本阶段运行时（debian 11，glibc 2.31）自洽即可。
# 若 gcr.io 不可达，可改用 `debian:11-slim` 并 `apt-get install -y ca-certificates`。
FROM gcr.io/distroless/cc-debian11

# 无 shell，ENSRYPT 用 exec 形式（见 ENTRYPOINT）。插件目录同形。
COPY --from=builder /build/bin/oj /usr/local/bin/oj
# 插件发现四级顺序的第一级：OJ_PLUGINS_DIR（内含 <host-triple>/ 子目录，
# 与发布包布局同形）。
COPY --from=builder /build/bin/plugins /usr/local/oj/plugins
ENV OJ_PLUGINS_DIR=/usr/local/oj/plugins

WORKDIR /app
EXPOSE 9778

ENTRYPOINT ["oj", "server"]
CMD ["-c", "config.yaml", "--api-path", "src"]
