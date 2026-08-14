# Changelog

## 1.0.0 — 2026-08-14

### 稳定版发布

- 将 Reginux Core、TUI、CLI 和发布文档统一为 `1.0.0`；插件 `schema_version = 1`
  协议保持兼容。
- 增加可重复的 Linux 构建归档、SHA-256 校验和、CycloneDX 1.5 SBOM 与构件验证脚本。
- CI 增加 release 构建、release 测试、CLI 启动检查，并在能力受限时将 Landlock、D-Bus
  和 Unix socket 集成测试从“跳过”提升为发布门禁失败。
- 固化 v1.0.0 支持范围、平台前提、兼容性规则、发布清单和人工验收记录格式。

### 兼容性承诺

- `schema_version = 1` 的现有插件清单继续受支持；未知字段按既有拒绝策略处理。
- 1.0.x 只接受向后兼容的插件协议和配置迁移；破坏性协议变更进入新的主版本。
- Command Adapter 必须配套 `reginux-sandbox`，系统写入必须配套 root-owned helper 与
  polkit policy；平台不具备必要安全能力时不会降级执行。

## 0.4.0 — 2026-08-12

### 完整插件运行时

- 实现 Command、session/system D-Bus、Unix socket Adapter transport；socket 验证
  peer UID，三类 transport 共用 typed operation、超时和输出限制。
- 新增 INI、CSV、quoted delimited、fixed-status regex、single-record Decoder；
  Snapshot 和 Lua 结果增加深度、节点与大小限制。
- Schema 新增 TOML/INI/KDL 保格式读写、explicit source 和确定插入策略；读取与写回
  复用同一实现。
- Adapter 增加 validate、precondition、verify、compensation；确认页显示真实计划。
- 刷新失败保留 stale 只读快照；重复插件/字段/写目标禁用全部冲突方。

### 安全与发布 UX

- Command Adapter 强制经过 Landlock V3、seccomp、no_new_privs 与 rlimit；隔离不可用
  时失败关闭。
- sandbox 以已校验可执行文件描述符授权并 `fexecve`，不再按可竞态替换的路径执行。
- 系统事务接入 root-owned polkit helper，重新验证原文、允许路径、系统插件摘要和
  来源图，准备 `/var/lib/reginux/backups` 并执行集合回滚。
- 文件事务以父目录文件描述符和 `openat/renameat` 消除路径分量符号链接竞态；
  helper 调用增加 120 秒超时及并发有界输出读取。
- Plugins 视图新增选择、权限审阅、ID+摘要批准和撤销；摘要变化后自动失效。
- 安装脚本部署 sandbox；`--with-helper` 以 root 部署 helper 和 polkit policy。
- 示例扩充为 Kitty、Clock Command、D-Bus、Unix Socket、Neovim Transform 五个。

## 0.3.0 — 2026-08-12

### 插件系统

- 用统一 Provider → Decoder → Snapshot/Document → Presentation → Plan 模型
  替换旧 YAML Schema 与 Procedural JSON-lines 协议。
- Schema 改用严格 TOML，支持白名单环境变量绝对路径、多来源与安全 include 图。
- 新增 command Adapter、JSON/JSON Lines/KV/Delimited/Single Decoder、动态资源稳定
  ID、受约束 argv 计划、事务保证和执行后 verify。
- 新增内嵌 Lua 5.4 Transform，移除 I/O/进程/网络模块并限制内存与指令数。
- Adapter/Transform 批准绑定插件 ID、manifest 与程序/脚本 SHA-256；变化后失效。
- 新增 Kitty Schema、Clock Adapter 与 Neovim Transform 示例。

### 安全与 UX

- 来源不再一律伪装成路径；TUI 区分 File、Imported 与 Runtime source。
- 编辑能力替换单一 writable 布尔值；Adapter 保证等级进入 Diff。
- command Adapter 使用绝对程序、无 shell、清空环境、固定 PATH、超时和输出限制。
- 插件详情显示信任、批准、摘要、来源、能力和命令权限。
- 旧 YAML/Procedural 明确拒绝并提供迁移诊断。

## 0.2.0 — 2026-08-12

### 安全

- 系统写入改为默认关闭，Apply 保持默认确认和备份。
- Stage 拒绝符号链接、硬链接、非普通文件和不可读来源。
- Apply 增加全文件预检、逐文件写前复检、并发冲突拒绝和逆序尽力回滚。
- 原子替换增加 mode、uid/gid、Linux xattr 保留及父目录 `fsync`。
- helper 收窄为路径白名单 `replace_file`，强制比较 `expected_original`。
- 通用扫描过滤敏感名称、私钥、二进制、非 UTF-8 和超大文件。
- Procedural 插件增加 3 秒超时、1 MiB 输出限制、进程组终止和路径越界拒绝；
  本版本强制只读。

### 用户体验

- 新增 Overview/System/Applications/Reginux/Config files 范围与跨范围搜索。
- 新增 Plugins 状态页及 Raw/Diff/Info/Help 长内容滚动。
- Apply 确认展示用户/系统范围、创建/替换和行数影响。
- Reload、Quit 和 Search 取消恢复用户上下文；失败后保留 staged state。
- 行内编辑支持 Unicode 光标、Home/End/Delete、Ctrl+W、Ctrl+U。
- keymap 使用 context 分层，动态提示限制长度并链接完整 Help。
- 统一终端 Shift 字符事件，使 `Q`/`?` 在不同终端稳定生效；空闲时不再轮询重绘。
- 外部编辑器丢失临时文件时不再把源内容误 stage 为空。

### 兼容与工程

- 产品主线和测试均为 Rust；procedural 协议示例改为 POSIX shell。
- 默认安装只包含 `reginux`，helper 需 `--with-helper` 显式选择。
- 增加严格 Clippy、发布构建和 28 项自动化回归覆盖。

## 0.1.0

- 首个 Rust TUI 技术原型，包含 provider、staging、diff、插件和 helper 骨架。
