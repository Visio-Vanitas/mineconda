# mineconda

[English](README.md) | 简体中文

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
- `lock`、`sync`、`cache`、`doctor`
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
mineconda [--root <PATH>] [--no-color] [--lang <auto|en|zh-cn>] <COMMAND>
```

主要命令：

- `init` / `add` / `remove` / `ls`
- `search` / `update` / `pin` / `lock`
- `sync` / `cache` / `doctor`
- `env` / `run`
- `import` / `export`

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
