# mineconda 架构草案

## 目标

- 提供类似 `uv` 的包管理体验：快速、可复现、锁文件驱动
- 抽象多模组源（Modrinth/CurseForge）
- 支持项目配置自定义 S3 模组源与 S3 缓存后端（匿名/私有 SigV4）
- 统一导出常见整合包格式
- 支持 `mineconda run` 启动开发模式 client/server/both 实例

## 模块拆分

### `mineconda-cli`

- 负责参数解析与命令调度
- 只做编排，不承载具体协议和格式逻辑

### `mineconda-core`

- 声明 `mineconda.toml` / `mineconda.lock` 数据模型
- 负责序列化与反序列化
- 作为其他 crate 的稳定基础层
- 维护项目级源/缓存配置（如 `[sources.s3]` / `[cache.s3]`）

### `mineconda-resolver`

- 依赖解析器（manifest -> lockfile）
- 递归补全可解析的传递依赖（支持 Modrinth `required` 与 CurseForge `RequiredDependency`）
- 预校验冲突（版本冲突、`incompatible` 依赖冲突）
- 支持版本范围约束（`^`, `~`, `>=`, `<=`, `<`, `>` 等）
- 多源搜索能力（`search` 命令）
- 后续扩展为版本约束求解与镜像缓存策略

### `mineconda-export`

- 输出整合包格式：
  - CurseForge ZIP
  - Modrinth MRPACK
  - MultiMC ZIP
- MRPACK 会携带 lock 中的 `fileSize/hashes` 与 side->env 映射
- CurseForge 导出会筛选可映射到 `projectID/fileID` 的条目

### `mineconda-runner`

- 统一开发实例入口，构建 Java 启动参数
- 支持 `client/server/both` 模式与 dry-run
- `both` 模式下可并行双端调试（先起服务端，再起客户端）

### `mineconda-runtime`

- 管理受控 Java 运行时目录（默认 `~/.mineconda/runtimes/java`）
- 对接 Adoptium API 安装指定版本
- 为 `mineconda env` / `mineconda run` 提供统一运行时解析

### `mineconda-sync`

- 读取 lock 并执行下载/安装流程
- 使用全局缓存目录 `~/.mineconda/cache/mods`
- 支持可选 `cache.s3`（`local -> s3 -> origin` 读穿，下载后回填 S3）
- 支持 `sync --offline`（仅本地缓存）、`sync --jobs` 并发准备缓存、`sync --verbose-cache` 命中观测
- 支持 `cache stats` / `cache verify` / `cache remote-prune --s3`
- 私有桶通过 SigV4 访问，匿名模式继续兼容公开桶/网关
- 同步后回填 `sha256` 与文件大小，确保 lock 可复验

## 关键文件

- `mineconda.toml`：用户维护的期望依赖
- `mineconda.lock`：解析后固定依赖与校验信息
- `~/.mineconda/runtimes/java`：受管 Java 运行时缓存
- `~/.mineconda/cache/mods`：模组下载全局缓存

## 命令生命周期

1. `init` 生成 `mineconda.toml`
2. `add/remove/search` 维护与查询依赖
3. `lock` 计算并写入 `mineconda.lock`
4. `lock` 阶段执行依赖冲突预检查，冲突即拒绝写入
5. `env install/use/list/which` 管理 Java 运行时
6. `doctor` 检查环境/配置/锁文件一致性
7. `sync` 根据 lock 执行下载/校验/落盘（支持缓存、prune、offline、jobs 与 `--locked`）
8. `export` 生成外部分发格式
9. `run` 启动开发实例（优先使用受管 Java）

## 下阶段建议

1. 为依赖解析加入版本约束求解器（semver/range 而非精确匹配）
2. 为 CurseForge 依赖关系补齐与统一冲突模型
3. 为 `run` 自动识别 Fabric/Forge/NeoForge 启动引导
4. 为导出流程补齐各格式标准字段与兼容性测试
5. 增加集成测试（fixture modpack + golden lockfile）
