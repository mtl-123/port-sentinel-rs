FROM debian:bookworm-slim

WORKDIR /app

# 🔧 直接复制本地编译好的二进制 + 配置
COPY target/release/port-sentinel-rs ./
COPY config.toml ./

CMD ["./port-sentinel-rs"]
