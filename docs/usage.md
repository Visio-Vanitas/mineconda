# mineconda 使用文档（详尽版）

本文档面向日常使用与运维场景，覆盖：

- 命令行能力与参数
- `mineconda.toml` / `mineconda.lock` 配置与字段说明
- 多源（Modrinth / CurseForge / MC百科搜索 / Local / URL / S3）与 `cache.s3` 缓存后端使用方式
- 交互搜索、依赖解析、同步、导出、运行时与开发实例启动
- CI/烟测与可选 SSH+WSL S3 联调
- 常见故障排查

## 1. 项目定位

`mineconda` 是一个 Rust 编写的 Minecraft 模组/整合包管理器，目标体验接近 `uv`：

- 声明式清单（`mineconda.toml`）
- 可复现锁文件（`mineconda.lock`）
- 解析依赖并做冲突预检查
- 一键同步到 `mods/`
- 支持导出常见整合包格式与开发模式运行

## 2. 安装与构建

在仓库根目录执行：

```bash
cargo build -p mineconda-cli --release
```

二进制位置：

- `target/release/mineconda`

查看帮助：

```bash
./target/release/mineconda --help
```

## 3. 核心文件与目录

项目根目录关键文件：

- `mineconda.toml`：你维护的“期望状态”
- `mineconda.lock`：解析后“固定状态”
- `mods/`：同步后的真实模组文件
- `.mineconda/`：运行实例、缓存辅助目录

全局缓存（默认）：

- `~/.mineconda/cache/mods`：下载缓存
- `~/.mineconda/cache/search`：搜索缓存

可由环境变量覆盖，见“环境变量”章节。

## 4. 快速开始

```bash
# 1) 初始化整合包
mineconda init mypack --minecraft 1.21.1 --loader neoforge --loader-version latest

# 2) 搜索并安装（非交互）
mineconda search embeddium --install-first --non-interactive

# 3) 查看状态
mineconda ls --status --info

# 4) 同步到 mods 目录
mineconda sync

# 5) 开发模式启动（dry-run）
mineconda run --mode client --dry-run
```

## 5. 命令总览

```text
mineconda [--root <ROOT>] [--no-color] <COMMAND>
```

命令列表：

- `init`
- `add`
- `remove`
- `ls`
- `search`
- `update`（别名：`upgrade`）
- `pin`
- `lock`
- `cache`（`dir/ls/stats/verify/clean/purge/remote-prune`）
- `env`（`install/use/list/which`）
- `sync`
- `doctor`
- `run`
- `export`
- `import`

## 6. 命令详解

### 6.1 `init`

初始化项目清单，可选创建基础目录。

```bash
mineconda init <NAME> \
  --minecraft 1.21.1 \
  --loader neoforge \
  --loader-version latest
```

参数：

- `--minecraft`：Minecraft 版本，默认 `1.20.1`
- `--loader`：`fabric|forge|neoforge|quilt`
- `--loader-version`：Loader 版本，默认 `latest`
- `--bare`：仅写清单，不创建基础目录

### 6.2 `add`

向清单添加模组声明，并默认自动刷新 lock。

```bash
mineconda add <ID> \
  --source modrinth \
  --version latest \
  --side both
```

参数：

- `--source`：`modrinth|curseforge|url|local|s3`
- `--version`：版本约束（见“版本约束”）
- `--side`：`both|client|server`
- `--no-lock`：只改清单，不立即更新锁文件

说明：

- `s3` 源必须提供精确对象 key（或 `s3://bucket/key`），不支持 `latest`/范围约束。

### 6.3 `remove`

从清单移除模组声明，并默认刷新 lock。

```bash
mineconda remove <ID> [--source <SOURCE>] [--no-lock]
```

### 6.4 `ls`

查看清单与锁状态。

```bash
mineconda ls --status --info
```

参数：

- `--status`：显示同步状态（例如 `synced/not-synced`）
- `--info`：显示更多元数据（哈希、下载地址、缓存命中等）

### 6.5 `search`

搜索模组，默认源 `modrinth`。

```bash
mineconda search <QUERY> \
  --source modrinth \
  --limit 10 \
  --page 1 \
  --lang auto
```

参数：

- `--source`：`modrinth|curseforge|mcmod`
- `--limit`：每页条数
- `--page`：页码（从 1 开始）
- `--non-interactive`：禁用交互界面，使用文本输出
- `--install-first`：直接安装首个结果（适合脚本）
- `--install-version`：指定安装版本（配合 `--install-first` 或交互安装）
- `--lang`：`auto|en|zh-cn`，命令级语言选择（全局参数）

交互模式（TTY 默认）：

- `↑/↓` 或 `j/k` 选择条目
- `Enter` 或 `V` 进入版本选择后安装
- `L` 快速安装当前条目的最新兼容版本
- `q` / `Esc` 退出

### 6.6 `update` / `upgrade`

更新约束或批量刷新 lock。

```bash
# 更新某个 mod 的约束
mineconda update sodium --source modrinth --to latest

# 不指定 id：刷新 lock（等价于 upgrade）
mineconda update
mineconda upgrade
```

### 6.7 `pin`

固定版本。未显式给 `--version` 时会尝试从 lock 推断。

```bash
mineconda pin jei --source local
mineconda pin sodium --source modrinth --version 0.5.13
```

### 6.8 `lock`

解析依赖并写入锁文件。

```bash
mineconda lock
mineconda lock --upgrade
```

行为：

- 自动补全可解析传递依赖（例如 Modrinth required）
- 写入前做冲突预检查（版本冲突、incompatible 冲突）

### 6.9 `cache`

缓存管理。

```bash
mineconda cache dir
mineconda cache ls
mineconda cache stats --json
mineconda cache verify
mineconda cache verify --repair
mineconda cache clean
mineconda cache purge
mineconda cache remote-prune --s3 --max-age-days 30 --dry-run
```

说明：

- `stats`：输出本地缓存文件数、总大小、被当前 lock 引用/未引用占比
- `verify`：校验当前 lock 引用缓存的哈希与大小；`--repair` 会删除损坏条目
- `remote-prune --s3`：按前缀和 TTL 清理远端 S3 缓存对象

### 6.10 `env`

运行时（Java）管理。

```bash
mineconda env install 21 --provider temurin --use-for-project
mineconda env use 21
mineconda env list
mineconda env which
```

基线 smoke 现在会真实执行这条链路：安装受管 Java、更新项目 runtime，并用该 runtime 驱动 `run` 的真实进程测试。

### 6.11 `sync`

按 lock 下载并安装到 `mods/`。

```bash
mineconda sync
mineconda sync --no-prune
mineconda sync --offline
mineconda sync --jobs 4 --verbose-cache
mineconda sync --locked   # 或 --frozen
```

参数：

- `--no-prune`：不同步清理多余旧模组
- `--offline`：禁止一切网络访问，仅允许从本地缓存恢复
- `--jobs <N>`：并发准备缓存，默认自动选择
- `--verbose-cache`：打印每个包命中的缓存来源（`local|s3|origin`）
- `--locked` / `--frozen`：禁止同步过程中隐式改写 lock 元数据

### 6.12 `doctor`

诊断项目状态与环境可用性。

```bash
mineconda doctor
mineconda doctor --strict
```

`--strict`：把 warning 也视为失败（适合 CI 门禁）。

### 6.13 `run`

开发模式启动，支持 `client/server/both`。

```bash
mineconda run --mode client
mineconda run --mode server
mineconda run --mode both
mineconda run --mode client --dry-run
```

参数：

- `--java <JAVA>`
- `--memory <MEMORY>`（默认 `4G`）
- `--jvm-arg <ARG>`（可重复）
- `--mode client|server|both`
- `--username <USERNAME>`（默认 `DevPlayer`）
- `--instance <INSTANCE>`（默认 `dev`）
- `--launcher-jar <PATH>`：客户端 launcher
- `--server-jar <PATH>`：服务端入口；支持 jar，也支持 NeoForge/Forge 安装后的 `unix_args.txt`
- `--dry-run`：只打印命令，不实际启动

自动 launcher 识别：

- 会按 `mineconda.toml` 的 loader 优先匹配 `<loader>-client/server-launch.jar` 等命名，再回退通用命名。
- 当显式指定 `unix_args.txt` 时，`mineconda` 会继续使用受管 Java，并自动附加实例目录下的 `user_jvm_args.txt`（如果存在）。

### 6.14 `export`

导出整合包格式。

```bash
mineconda export --format mrpack --output dist/mypack
mineconda export --format curseforge --output dist/pack
mineconda export --format multimc --output dist/pack
mineconda export --format mods-desc --output dist/mods
```

格式：

- `mrpack`
- `curseforge`
- `multimc`
- `mods-desc`

导出细节：

- MRPACK：严格按 Modrinth 官方格式导出；不合规条目（如非 HTTPS 下载、缺少 `sha1/sha512`）直接报错失败。
- CurseForge：仅写入可映射到数字 `projectID/fileID` 的条目；无法映射项记录在 `x-mineconda` 扩展字段。
- mods-desc：包含 `source_ref` 等可追踪信息。

### 6.15 `import`

自动识别整合包格式并导入到当前项目根目录（当前仅支持 Modrinth `.mrpack`）。

```bash
mineconda import ./pack.mrpack --side client
mineconda import https://example.com/pack.mrpack --format auto --side client
mineconda import ./pack.mrpack --format mrpack --side client
```

参数：

- `input`：整合包文件路径或 `http(s)` URL
- `--format`：`auto|mrpack`（默认 `auto`；当前仅支持 `mrpack`）
- `--side`：`client|server|both`，控制应用 `overrides/client-overrides/server-overrides` 的顺序（默认 `client`）
- `--force`：覆盖已有 `mineconda.toml` / `mineconda.lock`

导入行为：

- 自动识别格式（通过包内索引文件），当前仅识别 `modrinth.index.json`
- 严格校验 `mrpack` 规范；不合规内容直接失败
- 导入后写入 `mineconda.toml`、`mineconda.lock`，并落盘对应 overrides 文件

## 7. 版本约束与源语义

`--version` 常见写法：

- `latest`
- 精确值：`OihdIimA`、`mc1.20.1-0.5.13-fabric`
- 范围值：`^0.5.0`、`>=0.5.0,<0.6.0`、`<1.0.0`

注意：

- `s3` / `local` / `url` 通常要求精确值（路径、URL、对象 key）。

## 8. 配置文件说明

### 8.1 `mineconda.toml` 示例

```toml
[project]
name = "mypack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "latest"

[[mods]]
id = "embeddium"
source = "modrinth"
version = "latest"
side = "client"

[runtime]
java = "21"
provider = "temurin"
auto_install = true

[server]
java = "java"
memory = "4G"
jvm_args = []

[sources.s3]
bucket = "my-mod-bucket"
region = "ap-southeast-1"
# endpoint = "https://minio.example.com"
# path_style = true
# public_base_url = "https://cdn.example.com/mods"
# key_prefix = "packs/dev"

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

`cache.s3` 启用后，`sync` 会按 `local -> s3 -> origin` 顺序读取缓存；从源下载成功后会回填本地缓存并尝试上传到 S3（上传失败仅告警）。
`cache.s3.auth=auto` 会在检测到 `access_key_env/secret_key_env` 对应环境变量时自动走 SigV4，否则匿名访问。
`cache.s3.auth=sigv4` 适合私有桶；推荐同时配置 `endpoint/path_style/region`。`public_base_url` 更适合匿名下载网关，不参与签名。

缓存相关命令：

- `mineconda cache stats`：统计本地缓存使用情况
- `mineconda cache verify [--repair]`：校验并可选修复损坏缓存
- `mineconda cache remote-prune --s3`：清理远端 S3 缓存对象

### 8.2 `mineconda.lock` 关键字段

- `id/source/version/side`
- `file_name/file_size`
- `download_url`
- `sha256` + `hashes[]`
- `source_ref`（源侧追踪信息）

## 9. 环境变量总表

### 9.1 通用运行

- `MINECONDA_HOME`：全局根目录覆盖（影响 runtime/cache 默认路径）
- `MINECONDA_CACHE_DIR`：全局缓存目录覆盖
- `MINECONDA_LANG`：界面语言（`en`/`zh-cn`，默认自动跟随系统）
- `NO_COLOR`：禁用彩色输出

### 9.2 搜索相关

- `MINECONDA_SEARCH_CACHE_DIR`：搜索缓存目录
- `MINECONDA_NO_SPINNER=1`：关闭搜索加载动画
- `MINECONDA_NO_PROXY=1`：搜索请求忽略系统/环境代理
- `MINECONDA_ALT_SCREEN=1`：交互搜索强制使用 alternate screen
- `CURSEFORGE_API_KEY`：CurseForge 搜索/解析必须

### 9.3 同步相关

- `MINECONDA_SYNC_RETRIES`：远程下载重试次数，默认 `3`

### 9.4 烟测脚本相关

`scripts/ci-smoke.sh`：

- `MINECONDA_BIN`：指定二进制路径
- `MINECONDA_ENABLE_S3_SMOKE=1`：启用可选 S3 烟测

`scripts/s3-smoke-wsl.sh`（可选）：

- `MINECONDA_S3_SSH_TARGET`（默认 `wsl`）
- `MINECONDA_S3_REMOTE_PORT`（默认 `19000`）
- `MINECONDA_S3_LOCAL_PORT`（默认 `39000`）
- `MINECONDA_S3_SOURCE_BUCKET`（默认 `mineconda-source-smoke`）
- `MINECONDA_S3_CACHE_BUCKET`（默认 `mineconda-cache-smoke`）
- `MINECONDA_S3_OBJECT_KEY`（默认 `packs/dev/iris-s3.jar`）
- `MINECONDA_S3_PRUNE_OBJECT_KEY`（默认 `prune-test/old-probe.jar`）
- `MINECONDA_S3_ACCESS_KEY` / `MINECONDA_S3_SECRET_KEY`（默认 `minioadmin`）
- `MINECONDA_S3_SUDO_PASSWORD`（远端 docker 需密码 sudo 时使用）
- `MINECONDA_S3_REMOTE_WORKDIR`
- `MINECONDA_S3_CONTAINER`

## 10. CI 与测试管线

本地建议顺序：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p mineconda-cli --release
MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh
```

可选 S3 烟测：

```bash
MINECONDA_ENABLE_S3_SMOKE=1 \
MINECONDA_S3_SUDO_PASSWORD='<远端sudo密码>' \
MINECONDA_BIN="$(pwd)/target/release/mineconda" \
bash scripts/ci-smoke.sh
```

说明：

- 该 S3 烟测为本地/自托管环境联调能力，不在 GitHub 托管 runner 默认开启。
- GitHub Actions `test.yml` 保持稳定基线烟测：包含 `env install/use/list/which` 与真实 `run client/server/both` 进程验证，但不注入 S3 联调变量。
- `.github/workflows/s3-smoke.yml` 提供自托管增强烟测入口，用 release 二进制验证私有 `cache.s3(auth=sigv4)`、`sync --offline` 与 `cache remote-prune --s3`。
- 仓库侧可配置：
  - Secret：`MINECONDA_S3_SUDO_PASSWORD`
  - Variables：`MINECONDA_S3_SSH_TARGET`、`MINECONDA_S3_REMOTE_PORT`、`MINECONDA_S3_LOCAL_PORT`、`MINECONDA_S3_SOURCE_BUCKET`、`MINECONDA_S3_CACHE_BUCKET`
  - 手动触发 `s3-smoke.yml` 时也可直接填写这些输入覆盖默认值

## 11. 常见问题排查

### 11.1 搜索失败或超时

- 先确认网络/DNS可用。
- 代理环境复杂时尝试 `MINECONDA_NO_PROXY=1`。
- CurseForge 必须设置 `CURSEFORGE_API_KEY`。

### 11.2 `sync --locked` 失败

含义通常是：当前 lock 元数据需要更新但被锁定模式阻止。

处理方式：

1. 先运行一次普通 `mineconda sync`（允许回填元数据）。
2. 再运行 `mineconda sync --locked` 作为可复现校验。

### 11.3 S3 模组/缓存不可用

检查：

- `mineconda.toml` 的 `[sources.s3]` 是否正确
- `mineconda.toml` 的 `[cache.s3]`（若启用）是否正确
- `public_base_url`/`endpoint` 是否可访问
- 对象 key 是否精确匹配
- 私有桶场景下 `cache.s3.auth/access_key_env/secret_key_env/region` 是否完整
- 若是 `sync --offline` 失败，先执行一次在线 `mineconda sync` 预热本地缓存
- 运行 `mineconda doctor`，查看 S3 配置与凭据环境变量告警

### 11.4 交互搜索在 tmux/iTerm 显示异常

- 默认已避免强制 alternate screen
- 可显式设置 `MINECONDA_ALT_SCREEN=1` 或禁用交互 `--non-interactive`

## 12. 推荐工作流

### 12.1 新建并迭代整合包

1. `mineconda init`
2. `mineconda search` + `mineconda add/remove`
3. `mineconda lock`
4. `mineconda sync`
5. `mineconda run --dry-run` 或实际运行
6. `mineconda export`

### 12.2 CI 门禁

1. `mineconda doctor --strict`
2. `mineconda sync --locked`
3. `cargo test --workspace`
4. `scripts/ci-smoke.sh`
