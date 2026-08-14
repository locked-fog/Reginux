# Reginux 0.4.0 安全模型

## 总原则

配置文件和应用接口仍是事实来源。插件只能描述来源、解码数据或生成受限计划；Core
是唯一执行者。不存在插件 `shell`、自由 `apply`、自由路径写入或 root hook。

## 文件事务

- 暂存时保存原始 bytes、存在性、mode 和摘要语义；Apply 前对所有来源预检，并在
  每次替换前再次检查，外部修改会拒绝而不是覆盖。
- 只接受普通非符号链接文件；逐级以 `openat(O_NOFOLLOW)` 打开目录，读写和
  `renameat` 始终锚定已验证的父目录文件描述符，拒绝目录分量替换/符号链接竞态；
  同时拒绝硬链接、NUL、无效 UTF-8 和越界 Transform 编辑。
- 用户文件使用同目录独占临时文件、`fsync`、原子 `renameat`，通过文件描述符保留
  owner、mode 和 Linux xattr；后续步骤失败会以刚写入内容为 precondition 逆序恢复，
  因此回滚不会覆盖事务期间出现的新外部修改。
- Schema 读取和写回使用同一格式实现。字段缺失时必须由 manifest 声明 `insert`，
  Core 不猜插入位置。

## 系统写入与 polkit

系统路径不会由 TUI 直接写入。事务把完整文件集合发送给
`/usr/libexec/reginux-helper`：

1. `/usr/bin/pkexec` 的 policy 精确绑定 helper 绝对路径；
2. helper 必须以 root 运行，协议、消息、文件数量和大小均有限制；客户端并发读取
   有界 stdout/stderr，认证或 helper 超过 120 秒会终止；
3. 每个文件重新验证绝对路径、普通文件属性和原始 bytes；
4. 内建写入只允许 hostname、locale、environment、hosts、sysctl 及 sysctl drop-in；
5. 系统 Schema 还要重新验证 `/usr/share/reginux/plugins` 下所有路径的 root owner、
   不可组/全局写、插件组合摘要和实际来源图；
6. 写前创建 `/var/lib/reginux/backups/<transaction>/...`，集合任一写入失败即回滚；
7. helper 不接受可执行文件、命令、shell 字符串或未声明路径。

Raw 编辑不能授权系统路径；只有由受信任条目生成的计划携带 helper 授权信息。

## Schema 路径安全

- 路径模板只允许 `HOME`、`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、
  `XDG_DATA_HOME`、`XDG_RUNTIME_DIR`，展开后必须是绝对路径。
- 用户 scope 必须位于普通用户 HOME；system scope 必须来自 root-owned 系统插件。
- import 有明确 keyword、shell-words 语法、深度/数量/总字节限制和 allowed roots；
  拒绝循环、`..`、符号链接和 glob 逃逸。
- manifest 256 KiB、单来源默认 1 MiB、来源图和解码结果 8 MiB。

## Adapter 安全

### Command

- program 为非符号链接绝对普通文件，批准摘要包含 manifest 与程序内容；sandbox
  以同一文件描述符完成摘要复检、Landlock 授权和 `fexecve`，路径替换不能偷换程序；
- 参数数组直接传递，不启动 shell；环境清空后只提供固定 PATH/LANG/LC_ALL；
- `reginux-sandbox` 在 exec 前强制设置 rlimit、`no_new_privs`、Landlock V3 和 seccomp；
- 文件系统默认只读且仅开放运行库、程序和 manifest 声明的 read paths；
- network 为 `none`、`local` 或 `internet`，seccomp 按声明限制 socket 系统调用；
  `none/local` 同时清理继承 FD，并覆盖批量 datagram syscall，不能借用父进程的网络句柄；
- mount、namespace、ptrace、BPF、内核模块、keyring、fork/clone 等高危调用被拒绝；
- 超时、stdout/stderr 各自受 1 MiB 限制，超时会终止进程组；
- Landlock 或 seccomp 无法完整安装时失败关闭，不以弱隔离继续运行。

### D-Bus 与 Unix socket

D-Bus 只连接 manifest 的 session/system bus、service、object path、interface 和 member，
参数按声明类型编码；wire body 与解码后的结果均有 1 MiB 上限。Unix socket 只连接展开后的
固定 endpoint，验证 `SO_PEERCRED` UID，并只支持 line、EOF、32-bit length-prefixed 三种
framing；请求和响应均有 1 MiB 上限。两者都有超时，不向插件暴露通用 socket API。

### 操作与状态

Snapshot 只能包含 JSON 基础数据；节点结构来自静态 manifest。编辑操作需要显式
precondition，可选 validate，执行后可 verify；compensatable 保证必须同时声明
compensation 和 verify。主操作发生超时、断连或其他调用失败时，只要声明了 compensation
也会尝试补偿；验证失败和补偿失败会在最终错误中明确区分。irreversible 操作不会进入
普通编辑流程。确认页显示参数、scope、保证和各验证步骤。

公共低级 staging API 只接受 HOME 或临时目录下的绝对路径；系统路径必须来自已声明并
授权的 ConfigEntry，不能通过自由路径绕过 helper。含 secret/sensitive 字段的 Adapter
不会把原始响应放入条目元数据；插件字段、错误和权限信息进入 UI 前会清理 C0/ESC 控制
字符，原始文件内容只保留换行/制表布局并移除终端控制序列。

刷新失败时旧快照只读保留并标记 stale，不能用过期状态继续修改。

## Transform 安全

Lua 5.4 由 `mlua` vendored 嵌入，只开放 table/string/math/utf8。`os`、`io`、
`package`、`debug`、require、FFI、进程、网络和文件系统均不可用。Core 限制 8 MiB
内存、200 万指令、64 层数据深度、10 万节点、结果大小、单编辑大小，并复检 source
ID、摘要、UTF-8 范围、重叠和 NUL。

## 审批与冲突

Adapter/Transform 审批绑定插件 ID 与组合 SHA-256：manifest、Transform 脚本和
Command 程序任一变化都会失效。用户可在 TUI 查看权限后批准或撤销。发布版没有
全局 allow-all；CLI 临时批准仍逐 ID。

重复插件 ID、全局字段 ID或同一文件字段写入所有权会禁用全部冲突者并产生诊断。

## 平台要求

Reginux 0.4.0 的 Command Adapter 要求 Linux 提供可由非特权进程使用的 Landlock
ABI V3 和 seccomp。容器或宿主若通过外层 seccomp 禁止 Landlock/socket，相关
Adapter 会明确失败关闭；这属于运行平台拒绝所需内核能力，不会触发不安全降级。
文件 Schema、Transform 和无需被宿主禁止能力的功能仍可使用。
