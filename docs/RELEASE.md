# Reginux 0.4.0 发布说明

## 结论

0.4.0 完成插件系统 v1 的运行时重构。Schema、Command/D-Bus/Unix Socket Adapter、
Lua Transform、审批/撤销、失效快照、冲突拒绝、强制命令隔离和 polkit 系统写入均
进入同一 staged/Diff/Apply/verify 流程，可作为插件 v1 发布基线。

## 支持矩阵

| 项目 | 状态 |
|---|---|
| Schema | Kitty/KV、TOML、INI、KDL；多来源、include 图、显式插入/写目标 |
| Command Adapter | 强制 Landlock+seccomp+rlimit，摘要复检、超时和输出限制 |
| D-Bus Adapter | session/system，固定 service/path/interface/member，类型化参数/返回 |
| Unix Socket Adapter | 固定 endpoint、peer UID、line/EOF/length-prefix framing |
| Decoder | JSON/JSONL/KV/INI/CSV/delimited/fixed-status/single/single-record/Lua |
| Adapter 计划 | validate、precondition、typed invocation、verify、compensation |
| Lua Transform | vendored Lua 5.4 纯转换沙箱和受限文本计划 |
| 插件信任 | ID+组合摘要、TUI 批准/撤销、变更自动失效 |
| Snapshot | 刷新时间、原始响应摘要、失败保留 stale 只读快照 |
| 用户文件事务 | 原子替换、备份、冲突复检、失败回滚 |
| 系统文件事务 | polkit helper 集合事务、重新授权、系统备份和回滚 |
| 冲突处理 | 重复插件/字段/写目标全部禁用并诊断 |

## 与 0.3.0 的兼容性

- Rust crate 和 manifest `schema_version` 仍为 v1；
- 缺失字段的新 Schema 必须增加 `insert = "end"` 或 `insert = "section"`；
- 可编辑 Adapter operation 必须声明 `precondition`；
- `compensatable` operation 必须同时声明 `compensation` 和 `verify`；
- Command Adapter 安装时必须同时部署 `reginux-sandbox`；
- 系统写入需安装 helper/polkit，不再由普通 TUI 进程尝试直接写入。

## 发布门禁

```bash
./scripts/check.sh
cargo build --release --locked --workspace
cargo run -q -p reginux-tui --bin reginux -- --help
```

归档必须包含 `Cargo.toml`、`Cargo.lock`、crates、resources、scripts、config、docs、
五个示例插件、LICENSE 和 CHANGELOG；不得包含 target、本地工具链、用户配置、备份
或 working copy。

## 人工验收

1. Kitty include 字段显示并修改真正生效文件，注释和其他格式不变。
2. TOML、INI、KDL 示例数据读取与写回同一字段，缺失字段按 manifest 插入。
3. Plugins 视图审阅 Command 权限后批准；修改程序或 manifest 后审批自动失效。
4. 在支持 Landlock 的主机运行 Clock Adapter，确认固定环境和快照时间；禁用
   Landlock 的容器应明确拒绝，不得降级执行。
5. 在正常用户 session bus 批准 D-Bus 示例并读取 broker identity。
6. 为 Socket 示例提供同 UID endpoint，确认 peer 与 framing；错误 UID 必须拒绝。
7. 让 Adapter precondition 与旧值不符，Apply 必须要求刷新；verify 失败时执行声明
   的补偿并报告最终结果。
8. 制造 refresh 失败，旧条目保留但标记 STALE 且不可编辑。
9. 安装 polkit helper 后修改允许的系统条目，确认系统备份和事务报告；未声明路径拒绝。
10. 制造重复插件 ID/写目标，确认所有冲突条目都未加载。

## 运行平台前提

Command Adapter 依赖非特权 Landlock ABI V3 和 seccomp。平台禁止这些内核能力时，
该 transport 按设计失败关闭。D-Bus/Socket 在线验收同样需要宿主允许相应 socket
系统调用；构建与纯解析测试不绕过宿主安全策略。
