# mineconda

`mineconda` 是一个用 Rust 编写的 Minecraft 模组/整合包管理器，设计目标是提供类似 `uv` 的极速、可复现包管理体验。

详尽使用文档见：[docs/usage.md](docs/usage.md)。

当前仓库是第一版骨架，已包含：

- `mineconda init` 初始化项目清单
- `mineconda add/remove` 管理模组声明并自动刷新 lock（可用 `--no-lock` 关闭）
- `mineconda search` 从模组源网站搜索（默认 `modrinth`，TTY 下默认交互模式，支持本地搜索缓存与加载动画）
- 支持项目级自定义 S3 源与 S3 缓存后端（`[sources.s3]` / `[cache.s3]`）
- `mineconda ls` 查看清单/锁文件/同步状态（`--status --info`）
- `mineconda update` / `mineconda pin` 更新与固定版本约束（`update` 支持 `upgrade` 别名）
- `mineconda env` 运行时管理（Java 版本安装/切换/查看）
- `mineconda lock` 递归解析依赖并预校验冲突，生成带来源元数据的锁文件
- `mineconda sync` 下载/缓存/安装模组并对齐 `mods/` 目录（支持 `--locked/--frozen/--offline/--jobs/--verbose-cache`）
- `mineconda cache` 查看/校验/治理全局缓存（`dir/ls/stats/verify/clean/purge/remote-prune`）
- `mineconda doctor` 诊断项目环境与配置问题
- `mineconda export` 导出常见整合包格式（基础结构）
- `mineconda import` 自动识别整合包格式并导入（当前支持 Modrinth `.mrpack`）
- `mineconda run` 启动开发模式实例，支持 `--mode client/server/both`

## 快速开始

```bash
cargo run -p mineconda-cli -- init mypack --minecraft 1.20.1 --loader fabric --loader-version 0.16.9
cargo run -p mineconda-cli -- add sodium --source modrinth --version 0.5.11
cargo run -p mineconda-cli -- remove sodium --source modrinth
cargo run -p mineconda-cli -- search sodium --limit 5
cargo run -p mineconda-cli -- search iris --source mcmod --limit 10 --page 1
cargo run -p mineconda-cli -- search --source modrinth --non-interactive sodium
cargo run -p mineconda-cli -- --lang en search sodium --limit 5
cargo run -p mineconda-cli -- search sodium --install-first
cargo run -p mineconda-cli -- search sodium --install-first --install-version OihdIimA
cargo run -p mineconda-cli -- add jei-private --source s3 --version packs/dev/jei/jei-1.21.1.jar
cargo run -p mineconda-cli -- ls --status --info
cargo run -p mineconda-cli -- update sodium --source modrinth --to latest
cargo run -p mineconda-cli -- pin sodium --source modrinth
cargo run -p mineconda-cli -- env install 21 --use-for-project
cargo run -p mineconda-cli -- env which
cargo run -p mineconda-cli -- lock
cargo run -p mineconda-cli -- sync
cargo run -p mineconda-cli -- sync --offline --jobs 4 --verbose-cache
cargo run -p mineconda-cli -- sync --locked
cargo run -p mineconda-cli -- doctor
cargo run -p mineconda-cli -- cache ls
cargo run -p mineconda-cli -- cache stats --json
cargo run -p mineconda-cli -- cache verify
cargo run -p mineconda-cli -- cache remote-prune --s3 --max-age-days 30 --dry-run
cargo run -p mineconda-cli -- export --format mrpack --output dist/mypack
cargo run -p mineconda-cli -- export --format mods-desc --output dist/mods
cargo run -p mineconda-cli -- import dist/mypack.mrpack --format auto --side client
cargo run -p mineconda-cli -- import https://example.com/pack.mrpack --format mrpack --side client
cargo run -p mineconda-cli -- run --mode client --dry-run --launcher-jar .mineconda/dev/launcher.jar
cargo run -p mineconda-cli -- run --mode server --dry-run --server-jar server.jar
cargo run -p mineconda-cli -- run --mode both --dry-run --launcher-jar .mineconda/dev/launcher.jar --server-jar server.jar
```

## 测试管线

本仓库提供一条统一的测试管线，GitHub Actions 与本地命令保持一致：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p mineconda-cli --release
MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh
```

`scripts/ci-smoke.sh` 会在 `.test/ci-smoke` 下执行一轮端到端烟测：

- `init` 初始化 `1.21.1 + neoforge` 项目
- `search iris --page 2`（默认 `modrinth`）验证搜索链路与翻页
- 再次执行同参数 `search`，验证本地搜索缓存命中路径
- `search --install-first` 验证从搜索结果自动导入模组
- 通过 PTY 驱动交互式 `search`（发送 `Enter`）验证交互安装链路
- 用 `local` 源添加 JEI 占位模组并 `sync --jobs 2 --verbose-cache`
- `cache stats` / `cache verify` 与 `sync --offline` 路径
- 可选：通过 SSH + WSL 部署 MinIO，并验证公开 `sources.s3`、私有 `cache.s3(auth=sigv4)` 与 `cache remote-prune --s3`（`MINECONDA_ENABLE_S3_SMOKE=1`）
- `sync --locked` 验证可复现同步防护链路
- `ls/update/pin/cache` 命令基础链路验证
- `env install/use/list/which` 与受管 Java 运行时链路
- `run --dry-run` 与真实 `run client/server/both` 进程链路
- `export --format mrpack` / `export --format curseforge` / `export --format mods-desc` 验证导出流程
- `import <mrpack>` 自动识别 + 严格导入验证

此外，`cargo test --workspace` 中包含 resolver 的 fixture 快照测试（`local-pack` -> `expected.lock.toml`），用于固定 lock 解析输出。
`mineconda-export` 还包含导出格式 fixture 快照测试（`export-pack`），固定 `mrpack/curseforge/multimc/mods-desc` 导出 JSON 结构。

## Workspace 结构

- `crates/mineconda-cli`：CLI 入口，命令编排
- `crates/mineconda-core`：清单/锁文件模型与读写
- `crates/mineconda-resolver`：依赖解析与模组源搜索
- `crates/mineconda-export`：整合包导入导出（CurseForge/MRPACK/MultiMC）
- `crates/mineconda-runner`：`mineconda run` 开发实例启动器
- `crates/mineconda-runtime`：受管 Java 运行时安装与定位
- `crates/mineconda-sync`：下载缓存与本地安装同步
- `docs/architecture.md`：项目结构与后续设计规划

## 配置文件

- `mineconda.toml`：项目清单（期望状态）
- `mineconda.lock`：锁文件（可复现状态）

`mineconda.toml` 中新增了运行时配置（示例）：

```toml
[runtime]
java = "21"
provider = "temurin"
auto_install = true
```

`mineconda.lock` 现在包含：

- 源下载地址（`download_url`）
- 源引用（`source_ref`）
- 文件大小（`file_size`）
- 多算法哈希（`hashes`，同步后会补齐 `sha256`）

`lock` 阶段会做两件事：

1. 自动补全可解析的传递依赖（例如 Modrinth `required` 依赖）
2. 在写 lock 之前检查版本冲突与 `incompatible` 依赖冲突

版本约束支持（`add --version ...`）：

- `latest`：最新可用版本（默认）
- 精确值：如 `OihdIimA`、`mc1.20.1-0.5.13-fabric`
- 范围值：如 `^0.5.0`、`>=0.5.0,<0.6.0`、`<1.0.0`

`mineconda run` 会根据 `mineconda.toml` 中的 loader 自动优先匹配对应启动引导（Fabric/Forge/NeoForge/Quilt），并按顺序回退到通用命名。

`mineconda run --mode client` 查找顺序（示例）：

1. `.mineconda/dev/<loader>-client-launch.jar`（如 `neoforge-client-launch.jar`）
2. `.mineconda/dev/launcher.jar`
3. `<loader>-client-launch.jar` / `<loader>-client.jar`
4. `launcher.jar`
5. 其他已知命名（如 `minecraft-client.jar`）

`mineconda run --mode server` 查找顺序（示例）：

1. `.mineconda/dev/<loader>-server-launch.jar`（如 `neoforge-server-launch.jar`）
2. `.mineconda/dev/server-launcher.jar`
3. `<loader>-server-launch.jar` / `<loader>-server.jar`
4. `server.jar`
5. 其他已知命名（如 `minecraft-server.jar`）

如果显式传入 `--server-jar <PATH>`，除了 jar，也支持 NeoForge/Forge 安装后的 `unix_args.txt`。这条路径会继续使用 `mineconda` 受管 Java，而不是退回系统 `java`。

导出行为补充：

- `export --format mrpack`：严格按 Modrinth 规范写入 `modrinth.index.json`（`fileSize`、`hashes`、`downloads`、`env`）；不满足规范（如非 HTTPS 下载、缺少 `sha1/sha512`）会直接失败
- `export --format curseforge`：`manifest.json` 仅写入可解析为数字 `projectID/fileID` 的 CurseForge 条目
- 若 lock 中存在无法写入 CurseForge 标准 `files` 的条目，会记录在 `manifest.json` 的 `x-mineconda.skipped_non_curseforge_entries`
- `export --format mods-desc`：`resolved_mods` 现在包含 `source_ref`
- `import <file-or-url>`：自动识别格式并严格导入（`--format auto|mrpack`）；当前支持 Modrinth `.mrpack`，并按 `--side` 应用 `overrides/client-overrides/server-overrides`

## Search 源说明

- `modrinth`：`search` 默认源，可直接调用公开 API
- `mcmod`：可通过 `--source mcmod` 指定，支持 `--page` 翻页（基于 MC百科搜索页，仅返回模组条目）
- `curseforge`：需要设置 `CURSEFORGE_API_KEY` 环境变量后使用

## 自定义 S3 源与缓存

`mineconda` 支持将 S3/兼容 S3 存储配置为自定义模组源（`--source s3`）和模组缓存后端（`[cache.s3]`）。

在 `mineconda.toml` 中配置：

```toml
[sources.s3]
bucket = "my-mod-bucket"
region = "ap-southeast-1"
# endpoint = "https://minio.example.com"   # 可选：S3 兼容服务
# path_style = true                        # 可选：endpoint 场景常用
# public_base_url = "https://cdn.example.com/mods" # 可选：优先用于下载 URL
# key_prefix = "packs/dev"                 # 可选：自动为对象 key 添加前缀

[cache.s3]
enabled = true
bucket = "my-mod-bucket"
# region = "ap-southeast-1"
# endpoint = "https://minio.example.com"
# path_style = true
# prefix = "cache"
# auth = "auto" # auto | anonymous | sigv4
# upload_enabled = true
# access_key_env = "AWS_ACCESS_KEY_ID"
# secret_key_env = "AWS_SECRET_ACCESS_KEY"
# session_token_env = "AWS_SESSION_TOKEN"
```

然后添加模组（`--version` 填对象 key，或 `s3://bucket/key`）：

```bash
cargo run -p mineconda-cli -- add iris-private --source s3 --version packs/dev/iris/iris-1.21.1.jar
```

注意：

- `s3` 源目前要求精确对象 key，不支持 `latest`/范围约束
- `sync` 会通过配置生成 HTTP 下载地址，请确保对象可被访问（公开读或可访问网关/CDN）
- `cache.s3` 启用后，`sync` 使用 `local -> s3 -> origin` 的缓存读取顺序
- `sync --offline` 会关闭一切网络访问，只允许从本地缓存恢复
- `sync --jobs <N>` 只并发缓存准备阶段，不改变 lock/install 结果顺序
- `sync --verbose-cache` 会打印每个包的命中来源（`local|s3|origin`）
- 当从源站下载成功时会回填本地缓存并尝试上传到 S3；上传失败仅告警，不中断同步
- `cache.s3.auth=auto` 会在检测到凭据环境变量时自动走 SigV4，否则匿名访问
- `cache.s3.auth=sigv4` 适合私有桶；推荐同时配置 `endpoint/path_style/region` 与 `*_env`
- `cache stats` / `cache verify` 用于本地缓存观测与校验；`cache remote-prune --s3` 用于按前缀和 TTL 治理远端缓存

搜索缓存说明：

- 默认缓存目录：`~/.mineconda/cache/search`
- 若设置 `MINECONDA_CACHE_DIR`，缓存目录为 `$MINECONDA_CACHE_DIR/search-results`
- 可通过 `MINECONDA_SEARCH_CACHE_DIR` 单独覆盖搜索缓存目录
- 默认缓存 TTL：30 分钟
- 可通过 `MINECONDA_NO_SPINNER=1` 关闭 `search` 加载动画
- 可通过 `MINECONDA_NO_PROXY=1` 强制查询直连（忽略系统代理/环境代理）
- 语言可通过 `--lang auto|en|zh-cn` 或 `MINECONDA_LANG` 切换（优先级：`--lang` > `MINECONDA_LANG` > 系统语言）
- 可通过 `MINECONDA_SYNC_RETRIES` 设置 `sync` 远程下载重试次数（默认 `3`）
- `search` 在 TTY 下默认进入交互界面：`↑/↓` 选择、`Enter/V` 进入版本安装、`L` 快速安装当前环境可用的最新版本、`q/Esc` 退出
- 交互模式下若检测到 `mineconda.toml`，会自动按当前项目环境（`minecraft + loader`）筛选搜索结果
- 可用 `--non-interactive` 强制使用旧的文本输出模式
- 可用 `--install-first` 在非交互模式下安装第一个搜索结果（用于脚本/测试）
- 可用 `--install-version <VERSION>` 指定安装版本（需配合 `--install-first` 或交互安装）
- 为兼容 `tmux`，交互模式默认不启用 alternate screen；可用 `MINECONDA_ALT_SCREEN=1` 强制开启

可选 S3 烟测说明（SSH + WSL）：

- 定位：本地/自托管环境专用，不在 GitHub 托管 runner 默认启用

- 开关：`MINECONDA_ENABLE_S3_SMOKE=1`
- 目标主机：`MINECONDA_S3_SSH_TARGET`（默认 `wsl`）
- 端口：`MINECONDA_S3_REMOTE_PORT`（默认 `19000`）、`MINECONDA_S3_LOCAL_PORT`（默认 `39000`）
- Source bucket/对象：`MINECONDA_S3_SOURCE_BUCKET`（默认 `mineconda-source-smoke`）、`MINECONDA_S3_OBJECT_KEY`（默认 `packs/dev/iris-s3.jar`）
- Cache bucket：`MINECONDA_S3_CACHE_BUCKET`（默认 `mineconda-cache-smoke`）
- 远端裁剪对象：`MINECONDA_S3_PRUNE_OBJECT_KEY`（默认 `prune-test/old-probe.jar`）
- 凭据：`MINECONDA_S3_ACCESS_KEY`、`MINECONDA_S3_SECRET_KEY`（默认均为 `minioadmin`）
- 若远端 Docker 需要提权：可设 `MINECONDA_S3_SUDO_PASSWORD`（脚本会优先尝试 `sudo -n docker`，再回退到密码模式）

启用后，`scripts/ci-smoke.sh` 会调用 `scripts/s3-smoke-wsl.sh`：远端拉起 MinIO、创建公开 `source` bucket 和私有 `cache` bucket、建立本地 SSH 隧道，并验证 `add --source s3`、私有 `cache.s3(auth=sigv4)` 读穿回填、以及 `cache remote-prune --s3`。

## doctor 与 locked sync

- `mineconda doctor`：检查 `manifest/lock` 一致性、运行时、`[sources.s3]`/`[cache.s3]` 配置、SigV4 凭据与缓存目录可用性
- `mineconda doctor --strict`：将警告也视为失败（适合 CI 门禁）
- `mineconda sync --locked`（或 `--frozen`）：禁止 `sync` 隐式写回 lockfile 元数据
- `.github/workflows/test.yml`：稳定基线
- `.github/workflows/s3-smoke.yml`：自托管/手动触发的增强 S3 烟测

增强 S3 workflow 的仓库侧配置：

- Secret：`MINECONDA_S3_SUDO_PASSWORD`，仅当远端 Docker 需要密码 sudo 时才需要
- Variables（可选）：`MINECONDA_S3_SSH_TARGET`、`MINECONDA_S3_REMOTE_PORT`、`MINECONDA_S3_LOCAL_PORT`、`MINECONDA_S3_SOURCE_BUCKET`、`MINECONDA_S3_CACHE_BUCKET`
- `workflow_dispatch` 也可直接覆盖上述值；未配置时默认使用 `wsl / 19000 / 39000 / mineconda-source-smoke / mineconda-cache-smoke`
