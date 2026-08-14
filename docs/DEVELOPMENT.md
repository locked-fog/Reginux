# Reginux 开发说明

## 架构

~~~text
reginux-core
├── model          SourceRef / Field capability / Backend / ValueType
├── provider       Linux 基础 provider
├── plugin         Schema / Adapter / Transform runtime
├── sandbox        Landlock / seccomp command launcher protocol
├── privileged     polkit helper protocol and authorization
├── structured     shared TOML / INI / KDL / line-format parser-writers
├── transaction    file + Adapter staged plan / diff / validation / apply
├── filesystem     atomic write / backup / editor
└── keybindings    action-based key sequences

reginux-tui
├── main           Ratatui frontend
└── reginux-helper Restricted privileged-operation source
~~~

Core 不导入 Ratatui。未来 CLI、GUI 或 Agent API 应直接复用 Core。

## 开发环境与本地检查

项目主线为 Rust，workspace 使用 `rust-toolchain.toml` 固定 Rust
1.97.1，并要求 `rustfmt` 与 `clippy`。使用 rustup 的开发机进入项目目录后，
Cargo 会自动选择该工具链：

~~~bash
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
/usr/bin/date '+[{"id":"local","current":"%FT%T%:z"}]'
~~~

也可以直接执行完整检查脚本：

~~~bash
./scripts/check.sh
~~~

Reginux 核心、TUI、helper 和测试均为 Rust。Lua 5.4 由 `mlua` 的 vendored
构建嵌入；开发机不需要另行安装 Lua。

## 设计约束

- 文件是 source of truth；
- 前端不能绕过 transaction 直接写源文件；
- 外部编辑器只能编辑 staged working copy；
- provider 和插件必须保留来源路径；
- 新 action 先加入稳定 action 表，再绑定默认 key；
- Schema 优先；Adapter 和 Transform 只用于声明式规则无法覆盖的来源；
- 任何系统级写入都必须保留权限边界和可回滚备份。
- Command Adapter 不得绕过 `reginux-sandbox`；隔离能力缺失时必须失败关闭；
- 新 Adapter transport 要有 manifest 静态校验、运行时超时/大小限制和来源类型；
- Schema 读取与写回必须复用 `structured` 中的同一格式实现；
- 可编辑 Adapter operation 必须有 precondition；compensatable 必须有 verify+compensation。
