# mineconda

[English](README.md) | 简体中文

> 文档说明：本文档部分内容由 GPT-5.4 编写，可能存在表述不当或信息过时的情况。如有疑义，请以 CLI 帮助输出和当前代码行为为准。

`mineconda` 是一个使用 Rust 编写的 Minecraft 模组包管理 CLI，设计思路参考 `uv`。

它提供基于清单和锁文件的可复现工作流，覆盖依赖解析、缓存感知同步、运行时管理，以及模组包导入导出。

## 为什么是 mineconda

- **可复现**：使用 `mineconda.toml` + `mineconda.lock`
- **工作流直接**：声明式 `add/remove`、`lock`、`sync`
- **多源支持**：Modrinth / CurseForge / mcmod 搜索 + URL / local，并提供实验性的 S3 源与缓存支持
- **运行时感知**：通过 `mineconda env` 管理 Java 运行环境
- **整合包友好**：支持常见整合包工作流，目前稳定路径为 `.mrpack`

## 当前状态

项目仍在持续演进，但核心链路已经可用：

- `init`、`add`、`remove`、`ls`
- `search`（交互式 / 非交互式，支持从结果直接安装）
- `group`、`tree`、`why`
- `lock`、`status`、`sync`、`cache`、`doctor`
- `env`、`run`、`import`、`export`

当前稳定基线：

- `search` / `add` / `lock` / `sync` / `run`
- `import` 导入 Modrinth `.mrpack`
- `export` 导出 Modrinth `.mrpack`

兼容 / 实验能力：

- `export --format curseforge`
- `export --format multimc`
- `[sources.s3]` 和 `[cache.s3]`

## 安装

### 从源码构建

```bash
cargo build -p mineconda-cli --release
./target/release/mineconda --help
```

## 快速开始

```bash
# 1) 初始化项目
mineconda init mypack --minecraft 1.21.1 --loader neoforge

# 2) 搜索并安装第一条结果
mineconda search embeddium --install-first --non-interactive

# 3) 查看当前状态
mineconda ls --status --info

# 4) 将锁定的包同步到工作区
mineconda sync

# 5) 启动开发实例（仅预览命令）
mineconda run --mode client --dry-run
```

## CLI 概览

```text
mineconda [--root <PATH>] [--member <MEMBER>] [--profile <NAME>] [--no-color] [--lang <auto|en|zh-cn>] <COMMAND>
```

主要命令：

- `init` / `add` / `remove` / `ls`
- `group` / `tree` / `why`
- `profile` / `workspace`
- `search` / `update` / `pin` / `lock` / `status`
- `sync` / `cache` / `doctor`
- `env` / `run`
- `import` / `export`

适合查看包状态的命令：

- `mineconda lock diff`：只预览锁文件变化，不写回
- `mineconda lock --check`：只校验所选锁定面，不改写 `mineconda.lock`
- `mineconda status`：汇总所选 groups 的 manifest / lock / sync 漂移情况
- `mineconda sync --check`：只校验所选锁定包是否已安装，不改动工作区
- 两个命令都支持 `--json`，可用于脚本集成，并保持稳定的 `0/2/1` 退出码
- `mineconda ls --json`、`mineconda tree --json`、`mineconda why <id> --json` 可输出结构化依赖数据

## Dependency Groups

`mineconda` 支持命名依赖组，用来把一个项目拆成多个可选安装面，语义上接近 `uv`
里的 optional dependency groups。

模型：

- 顶层 `mods = [...]` 就是默认组 `default`
- 可选组写在 `[groups.<name>]` 下
- 组名必须是小写 kebab-case
- 命令默认只激活 `default`，需要显式传 `--group <name>` 或 `--all-groups` 才会启用额外组

示例：

```toml
[project]
name = "mypack"
minecraft = "1.21.1"

[project.loader]
kind = "neo-forge"
version = "21.1.227"

mods = [
  { id = "jei", source = "modrinth", version = "latest", side = "both" }
]

[groups.client]
mods = [
  { id = "iris", source = "modrinth", version = "latest", side = "client" }
]

[groups.dev]
mods = [
  { id = "spark", source = "modrinth", version = "latest", side = "both" }
]
```

常见工作流：

```bash
# 添加到默认组
mineconda add jei

# 创建并写入额外组
mineconda group add client
mineconda add iris --group client

# 只查看某个组
mineconda ls --group client
mineconda tree --group client
mineconda why iris --group client

# 一次解析所有组
mineconda lock --all-groups

# 为本地开发实例同步 default + client
mineconda sync --group client
mineconda run --mode client --group client
```

说明：

- 只要选择了额外组，`default` 总会一并激活
- `lock`、`sync`、`tree`、`why`、`run`、`export` 都支持 `--group` / `--all-groups`
- 旧锁文件可能没有 group metadata；如果命令要求，请重新执行一次 `mineconda lock`
- `run --mode client|server|both` 不会自动选择组，组激活始终是显式行为

## Profiles

Profile 是一组 group 选择的命名别名，用来减少重复输入，适合日常开发和调试场景。

示例：

```toml
[profiles.client-dev]
groups = ["client", "dev"]
```

用法：

```bash
mineconda profile add client-dev --group client --group dev
mineconda sync --profile client-dev
mineconda run --profile client-dev --mode client
mineconda tree --profile client-dev
```

规则：

- 项目级 profile 写在 `mineconda.toml`
- workspace 级 profile 写在 `mineconda-workspace.toml`
- member 本地 profile 会覆盖同名 workspace profile
- `--profile` 和 `--group` 会合并，`default` 仍然始终激活

## Workspace

`mineconda` 支持在一个 workspace 根目录下管理多个整合包 member。

workspace 文件示例：

```toml
members = ["packs/client", "packs/server"]

[workspace]
name = "demo"

[profiles.client-dev]
groups = ["client", "dev"]
```

常见工作流：

```bash
mineconda workspace init demo
mineconda workspace add packs/client
mineconda workspace add packs/server

mineconda --member client init client-pack --minecraft 1.21.1 --loader neoforge
mineconda --member client add jei
mineconda --member client lock

mineconda --all-members status
mineconda --all-members lock diff --json
```

当前边界：

- 每个 member 仍各自维护 `mineconda.toml` 和 `mineconda.lock`
- 目前支持 `status` 和 `lock diff` 的 `--all-members` 聚合
- `lock`、`sync` 以及其他 member 级命令仍要求显式 `--member`，`lock --check` / `sync --check` 也不例外

## JSON 输出

当前支持结构化 JSON 输出的命令：

- `mineconda lock diff --json`
- `mineconda status --json`
- `mineconda ls --json`
- `mineconda tree --json`
- `mineconda why <id> --json`

在 workspace 根目录配合 `--all-members` 使用时，`status --json` 和 `lock diff --json`
会返回按 member 聚合的结果以及对应退出码。

## 搜索交互

- 在 TTY 环境下默认启用交互模式
- 快捷键：
  - `↑/↓` 或 `j/k`：移动选中项
  - `Enter` / `V`：进入版本选择并安装
  - `L`：快速安装最新兼容版本
  - `q` / `Esc`：退出

语言选择：

- CLI 参数：`--lang auto|en|zh-cn`
- 环境变量：`MINECONDA_LANG`
- 优先级：`--lang` > `MINECONDA_LANG` > 系统 locale

## 配置亮点

- 项目文件：
  - `mineconda.toml`：期望状态
  - `mineconda.lock`：可复现的解析结果
- 可选 S3 源：
  - `[sources.s3]`（实验性）
- 可选 S3 缓存后端：
  - `[cache.s3]`（实验性）

完整配置请直接查看：

- `mineconda --help`
- `mineconda <command> --help`

## 开发

推荐本地验证流程：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p mineconda-cli --release
MINECONDA_BIN="$(pwd)/target/release/mineconda" bash scripts/ci-smoke.sh
```

## 贡献

欢迎提交 Issue 和 PR。请尽量保持改动聚焦，在合适的地方补充测试，并在提交前确保验证流程通过。

## 许可证

MIT OR Apache-2.0
