# 1. 本地编译（利用本地缓存，速度快）

cargo build --release

# 2. 编辑配置

vim config.toml # 填入 webhook

# 3. 构建镜像（秒级完成，只打包文件）

docker-compose build --no-cache

# 4. 启动

docker-compose up -d

# 5. 查看日志

docker-compose logs -f port-sentinel-rs
