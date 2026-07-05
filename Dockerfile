# syntax=docker/dockerfile:1

# ---- 构建阶段:编译出 release 二进制 ----
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --locked

# ---- 运行阶段:仅携带二进制的精简镜像 ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/polypulse /app/polypulse
# 后台套利 worker:不监听端口,故无 EXPOSE
# 容器无 TTY,程序自动走纯日志模式(见 src/main.rs 的 is_terminal 判断)
# 私钥 / Builder API 等敏感配置由 zeabur 环境变量运行时注入,不打进镜像
CMD ["/app/polypulse"]
