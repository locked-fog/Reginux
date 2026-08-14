# Reginux 0.4.0 用户文档

## 1. 核心原则

Reginux 不建立第二份配置数据库。`/etc`、`~/.config` 等真实文件始终是唯一
事实来源。所有编辑先进入内存 transaction；只有用户审阅 Diff 并确认 Apply
后，Reginux 才尝试写回。

始终以普通用户启动。系统路径写入默认关闭；需要时由 TUI 通过已安装的 polkit
helper 请求一次管理员认证。Reginux 不安装 setuid，也不在 Apply 后隐式重载服务。

## 2. 启动与恢复

~~~bash
reginux
~~~

开发目录运行：

~~~bash
cargo run -p reginux-tui --bin reginux
~~~

参数：

~~~text
--safe                         忽略用户配置，使用安全默认值
--reset-keybindings            重建 ~/.config/reginux/config.toml 的默认快捷键
--plugin-dir PATH              本次启动增加一个插件父目录
--allow-plugin ID              本次启动临时批准一个 Adapter/Transform 插件 ID
--no-confirm                   本次启动跳过 Apply 确认（不建议日常使用）
~~~

配置损坏时先运行 `reginux --safe`。如果只是快捷键错误，运行
`reginux --reset-keybindings`；无效用户配置也会自动回退默认配置并显示警告。

## 3. 首次进入与任务范围

界面由左侧配置列表、右侧内容区、顶部范围/统计和底部状态/快捷键组成。

默认进入 `Overview`，只呈现内建 provider、Reginux 自身项和插件项，避免把
扫描到的普通文件全部堆到首屏。使用 `[s` / `]s` 在以下范围间切换：

| 范围 | 用途 |
|---|---|
| Overview | 常用内建项（不展开环境变量）、Reginux 与插件项 |
| System | hostname、locale、environment、sysctl、hosts 等 |
| Applications | 应用及插件提供的配置 |
| Reginux | 界面、编辑器、安全和插件策略 |
| Config files | `~/.config` 与 `/etc` 中通过安全过滤的普通文本文件 |

`/` 搜索会跨全部条目。按 Enter 保留结果并进入 `Search results`；Esc 取消并
恢复原范围、列表位置。普通文件扫描跳过敏感名称、私钥、符号链接、非普通
文件、二进制/非 UTF-8 文件以及大于 1 MiB 的文件。

## 4. 视图与默认快捷键

### 导航和范围

~~~text
j / Down       向下；在长内容视图中向下滚动
k / Up         向上；在长内容视图中向上滚动
gg / G         顶部 / 底部
Ctrl+u/d       向上 / 向下翻页
h / Left       返回 Form
l / Right      在 Form 与 Raw 间切换
[s / ]s        上一个 / 下一个任务范围
Enter          激活当前项
~~~

### 视图和操作

~~~text
e              编辑当前项或 staged Raw 副本
r              刷新当前来源（文件重读、Adapter refresh、Transform 重解析）
d              Diff
i              Info
p              Plugins
Tab            Form → Raw → Diff → Info → Plugins → Form
Ctrl+s         Apply staged changes
u              撤销最近一次 staged 操作
?              完整快捷键表
Q              退出；存在 staged 变更时要求确认
~~~

底部提示来自实际 keymap。用户改变绑定后提示同步改变。Form context 会优先于
Browser context，同时继承列表导航，不会因为进入 Form 而丢失 `j/k`。

## 5. 编辑体验

### Form

Form 用于 provider 或 schema 暴露的结构化字段。按 `e` 进入行内编辑：

~~~text
Left/Right     按 Unicode 字符移动
Home/End       行首 / 行尾
Backspace      删除前一字符
Delete         删除后一字符
Ctrl+w         删除前一个词
Ctrl+u         删除到行首
Enter          校验并 stage
Esc            取消，不改变 staged state
~~~

结构化写回会尽量保留前导空白、引号风格、行尾注释和未知键。重复键只修改最终
生效的一项。Locale 的 `<inherit>` 和 schema 的 `<unset>` 会删除对应赋值，
不会把占位文本写进配置。

### Raw

Raw 显示当前 staged working state。按 `e` 时：

1. 创建权限为 0600 的临时 working copy；
2. 暂时恢复普通终端并启动外部编辑器；
3. 编辑器退出后验证临时文件仍是普通文件并读回；
4. 只把内容 stage 到 transaction；
5. 删除临时文件并恢复 TUI。

外部编辑器绝不直接打开真实 `/etc/...` 文件。命令按参数列表解析，不经过
shell；如果未写 `{file}`，文件参数会自动追加。配置示例：

~~~text
nvim {file}
code --wait {file}
helix {file}
~~~

### Diff、Info、Plugins 与 Help

这些页面都支持 `j/k`、方向键和 `Ctrl+u/d` 滚动。Diff 汇总全部 staged
文件；Info 显示 ID、来源、provider、权限和元数据；Plugins 显示加载状态、
transport、摘要、快照时间和声明权限。Plugins 中 `j/k` 选择插件，`a` 审阅并批准
当前摘要，`x` 撤销，`r` 刷新；Help 显示当前实际快捷键。

## 6. Apply 安全流程

`Ctrl+s` 后的确认页逐文件显示：

- USER 或 SYSTEM 范围；
- create 或 replace；
- 新增/删除行数；
- Adapter transport、类型化参数、precondition/validate/verify/compensation 与保证；
- Apply 后不会隐式执行 manifest 未声明的服务动作。

确认后按以下顺序执行：

1. 验证全部 staged 内容；
2. 比较全部源文件与 stage 时捕获的原始字节；
3. 在第一笔用户写入前准备全部用户备份；helper 在第一笔系统写入前准备全部系统备份；
4. 每一笔写入前再次检查源文件是否被外部修改；
5. 同目录创建临时文件，保留 mode、owner 和 Linux extended attributes；
6. `fsync` 临时文件，原子 `rename`，再 `fsync` 父目录；
7. 后续写入失败时，仅在内容仍等于本 transaction 写入值时逆序恢复；发现新的外部
   修改就停止覆盖并明确报告回滚冲突；
8. 成功后重新发现配置；失败时保留 staged state 供审阅或重试。

符号链接、硬链接和非普通文件会被拒绝，避免原子替换改变链接语义。并发冲突
也会被拒绝，Reginux 不会以过期 staged 内容静默覆盖外部编辑。

用户备份：

~~~text
~/.local/state/reginux/backups/<transaction>/
~~~

系统备份：

~~~text
/var/lib/reginux/backups/<transaction>/
~~~

应用内会自动回滚本次失败事务；跨会话恢复使用上面的持久备份。文件+外部 Adapter
不可能获得跨内核/进程的统一原子性，因此按 operation 的 atomic、compensatable 或
best-effort 保证执行；断电和介质故障仍需系统快照或配置管理备份。

## 7. 系统写入

`allow_system_writes` 默认是 false。确需写系统文件时：

1. 在 `Reginux` 范围打开 `Allow system writes`；
2. 单独 Apply 该用户配置；
3. 审阅目标系统文件的 Diff；
4. 普通用户确认 Apply，并在 polkit 提示中完成管理员认证。

该开关只是第二道确认。管理员需先执行：

~~~bash
sudo ./scripts/install-local.sh --prefix /usr/local --with-helper
~~~

安装会把 root-owned helper 放在 `/usr/libexec/reginux-helper` 并部署精确 polkit
policy。helper 重新校验来源、原文、插件摘要和允许路径，创建系统备份，并对文件
集合失败回滚；不要把它设成 setuid，也不要从未知路径复制替代品。

## 8. 插件

默认目录：

~~~text
~/.local/share/reginux/plugins
/usr/share/reginux/plugins
~~~

Schema 插件是声明式配置，默认加载。Adapter 和 Transform 默认不执行；
Adapter 按 ID+manifest/程序摘要批准。Command 强制进入内核沙箱；D-Bus/Unix
socket 只访问 manifest 的固定接口；Transform 按 ID+manifest/脚本摘要批准，在
受限 Lua 沙箱中运行。具体边界见
`SECURITY.md`。即使如此，也只应批准已审阅的外部代码插件。

第三方插件没有签名商店。启用前必须审阅 manifest、命令和权限声明。详见
`docs/PLUGIN.md`。

## 9. 常见问题

### Reload 为什么要求确认？

同一来源文件已有 staged 内容时，Reload 会丢弃该文件的全部 staged 字段，
因此必须确认；其他 staged 文件不受影响。

### Apply 成功但程序行为没变化？

Reginux 只写配置文件，不自动重载 daemon、桌面会话或内核参数。请按目标软件
文档手工执行重载或重启。

### Adapter 显示 STALE？

最近一次 refresh 失败。Reginux 保留上次成功快照便于诊断，但强制只读；检查插件
详情中的 Last error，恢复外部服务后在 Plugins 视图按 `r`。不要根据 stale 值应用
修改。

### Command Adapter 报 Landlock/seccomp 不可用？

宿主内核或容器策略禁止发布版要求的隔离能力。Reginux 会失败关闭而非无沙箱执行。
在支持非特权 Landlock ABI V3 和 seccomp 的 Linux 主机运行，或继续使用不执行外部
程序的 Schema/Transform 功能。

### 文件没有出现在 Config files？

它可能是链接、敏感文件、二进制/非 UTF-8 文件、超过 1 MiB，或超过单次扫描
上限。使用专用 schema/provider 比通用 Raw 编辑更合适。

`/etc/environment` 中名称疑似 password/token/secret/credential/auth/cookie/API key
的变量，以及 URL authority 含 userinfo 的值也不会进入目录，避免在 TUI 中意外
显示凭据。

### 为什么硬链接文件不能编辑？

原子 rename 只替换一个目录项，会断开硬链接共享 inode 的语义。为避免隐式
行为变化，Reginux 要求用户显式处理真实文件关系。
