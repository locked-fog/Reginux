# Reginux 0.4.0

Reginux 是一个以现有 Linux 配置和应用控制面为事实来源的 Rust TUI。它把分散的
文件、CLI、D-Bus 和 Unix socket 状态投影为统一条目，并通过“读取、暂存、校验、
审阅、确认、备份、应用、验证”的流程降低误操作风险。

> The registry Linux never needed — without becoming a registry.

## 0.4.0 能力

- 纯 Rust Core、Ratatui 前端、沙箱 launcher、polkit helper 与测试；
- hostname、locale、environment、sysctl、hosts 内建 Provider；
- Form、Raw、Diff、Info、Plugins、Help 视图，可搜索、可配置 Vim 风格键位；
- 普通文件原子替换、全量与写前冲突复检、备份、失败逆序回滚；
- 系统文件经 root-owned helper 执行集合级 compare-and-replace 与系统备份；
- Schema 插件：白名单环境变量、绝对路径、include 图、循环/逃逸检测；
- Kitty/KV、TOML、INI、KDL 共用解析/回写路径，保留未修改格式与注释；
- Adapter 插件：Command、session/system D-Bus、同机 Unix socket 三种 transport；
- JSON、JSON Lines、KV、INI、CSV、分隔表、固定状态正则、单值内建 Decoder；
- 静态 Node/Field 模板与动态稳定资源键；
- Adapter validate、前置条件、类型化调用、verify、compensation 和事务保证；
- Command Adapter 强制经过 Landlock、seccomp、`no_new_privs` 与 rlimit；隔离不可用时拒绝执行；
- Lua 5.4 Transform：无 I/O/进程/网络/FFI，限制内存、指令、结果和编辑范围；
- 插件 ID+组合摘要审批、TUI 内审阅/批准/撤销、摘要变化自动失效；
- 刷新失败保留上次成功快照，明确标记 stale 并强制只读；
- 重复插件 ID、字段 ID、写入所有权全部禁用冲突方，不按扫描顺序猜测。

## 构建与运行

要求 Linux、UTF-8 终端和 `rust-toolchain.toml` 固定的 Rust 1.97.1（包含 Cargo、
rustfmt、Clippy）。

```bash
cargo run -p reginux-tui --bin reginux
cargo run -p reginux-tui --bin reginux -- --reset-keybindings
```

查看五个示例插件：

```bash
cargo run -p reginux-tui --bin reginux -- --plugin-dir plugins/examples
```

Adapter/Transform 在批准前不会运行。可在 Plugins 视图选择插件，按 `a` 审阅并
批准当前摘要，按 `x` 撤销；`--allow-plugin ID` 只为本次进程临时批准。

## 安装

普通用户安装会同时安装主程序和强制命令沙箱：

```bash
./scripts/install-local.sh
./scripts/install-local.sh --prefix /opt/reginux
```

要应用系统文件，管理员需安装 root-owned helper 和 polkit policy：

```bash
sudo ./scripts/install-local.sh --prefix /usr/local --with-helper
```

主程序始终以普通用户运行。系统变更还必须在 Reginux 的 Safety 设置中显式开启，
确认后由 `/usr/bin/pkexec` 启动 `/usr/libexec/reginux-helper`；helper 不接受命令、
shell 或自由路径。

## 用户流程

```text
发现真实来源 → 刷新/解码 → 选择条目 → 暂存并校验
             → Diff 与影响确认 → 来源复检 → 用户/系统备份
             → 文件计划和 Adapter 计划 → 验证 → 刷新最终状态
```

运行时来源不伪装成文件，也没有 Raw 文件编辑。文件来源显示真实路径；import 字段
显示实际生效文件；Adapter 条目显示 transport、快照时间和 stale 状态。

## 验证与源码结构

```bash
./scripts/check.sh
cargo build --release --locked --workspace
```

`scripts/check.sh` 执行格式检查、workspace 编译、全部测试、`-D warnings` Clippy、
安装脚本语法和示例命令检查。

```text
crates/reginux-core/       模型、Provider、插件、沙箱、授权与事务引擎
crates/reginux-tui/        Ratatui 前端和独立 privileged helper
plugins/examples/          Schema、Command、D-Bus、Socket、Transform 示例
resources/polkit/          精确绑定 helper 路径的 polkit policy
config/default.toml        默认配置与键位
docs/                       用户、插件、安全、开发、设计和发布文档
```

详细操作见 `docs/USER.md`，插件规范见 `docs/PLUGIN.md`，安全保证与平台要求见
`docs/SECURITY.md`。
