# Reginux — Linux 通用配置管理器 TUI 原型草案

> 历史设计输入，仅用于追踪产品方向；0.4.0 的实际行为和安全边界以
> `README.md`、`docs/USER.md`、`docs/PLUGIN.md` 与 `docs/SECURITY.md` 为准。

> Registry + Linux  
> 一个面向 Linux 的统一、可扩展配置管理层。

---

## 1. 项目概述

**Reginux** 是一个面向 Linux 的统一配置管理工具。

其设计灵感来源于 Windows Registry，但不试图在 Linux 上重新实现一个集中式注册表，而是在不破坏 Linux 现有配置体系的前提下，对分散在：

- `/etc`
- `~/.config`
- `/usr/lib`
- `/etc/*/*.d`
- systemd 等系统组件
- 各类 CLI 配置工具

中的配置进行统一抽象、发现、展示、修改、验证和回滚。

Reginux 的核心目标是：

> 在保留 Linux 原生配置文件和工具链的同时，为它们提供统一、结构化、安全且可扩展的配置访问层。

第一版以 **TUI** 为主要交互界面。

未来可在相同核心架构之上增加：

- CLI
- GUI
- Agent / AI Tool API

TUI 不作为一次性原型，而作为 Reginux 的正式前端之一长期维护。

---

# 2. 核心设计原则

## 2.1 不替代原有配置系统

Reginux 不创建自己的中心数据库作为配置真源。

实际配置仍然存储于原有位置，例如：

```text
/etc/hostname
/etc/locale.conf
/etc/sysctl.d/
/etc/ssh/sshd_config

~/.config/niri/config.kdl
~/.config/kitty/kitty.conf
```

Reginux 只负责：

```text
发现
↓
读取
↓
解析
↓
结构化
↓
修改
↓
验证
↓
安全写回
```

因此即使完全卸载 Reginux，原系统配置仍然保持有效。

---

## 2.2 Files remain the source of truth

Reginux 不拥有配置本身。

Reginux 提供的是：

- 统一视图
- 结构化语义
- 安全修改
- 来源追踪
- Diff
- Backup / Rollback
- 权限管理
- 插件扩展

而配置文件、系统工具及各软件自身机制仍然是最终事实来源。

---

## 2.3 插件提供增强体验，而不是访问资格

没有插件时，用户仍然可以：

- 浏览配置文件
- 查看 Raw
- 搜索
- 查看 Diff
- 使用通用结构化能力（若格式支持）

安装插件后，再获得：

- 专用配置树
- 字段说明
- 类型约束
- 验证逻辑
- Apply / Reload
- 有效配置来源追踪

因此 Reginux 对软件的支持状态应倾向于：

```text
Generic Support
Enhanced Support
```

而不是：

```text
Supported
Unsupported
```

---

# 3. 产品定位

Reginux 不应定位为：

> 给 Linux 新手使用的设置 GUI。

更合适的定位是：

> 一个面向 Linux 的统一、可扩展配置管理层。

或者：

> The registry Linux never needed — without actually becoming a registry.

Reginux 不改变 Linux 的配置哲学，而是在其上建立统一视图。

核心原则可以概括为：

> Files remain the source of truth.  
> Reginux makes them understandable, editable, traceable and safe.

---

# 4. 总体架构

推荐整体架构：

```text
                    Reginux Core
                         │
          ┌──────────────┼──────────────┐
          │              │              │
         TUI            CLI            GUI
      第一正式前端      后续提供        后续提供
                         │
                         ↓
                   Agent / Tool API
```

核心层负责：

```text
Config Model
Provider
Discovery
Schema
Plugin Runtime
Diff
Validation
Transaction
Backup
Rollback
Privilege
```

任何前端都不得重复实现这些逻辑。

---

# 5. 项目结构建议

建议使用模块化 Rust workspace：

```text
reginux/
├── crates/
│   ├── reginux-core/
│   │   ├── config model
│   │   ├── filesystem
│   │   ├── provider
│   │   ├── discovery
│   │   ├── transaction
│   │   ├── validation
│   │   ├── backup
│   │   └── diff
│   │
│   ├── reginux-schema/
│   │   ├── schema model
│   │   ├── schema parser
│   │   └── field definition
│   │
│   ├── reginux-plugin/
│   │   ├── plugin loader
│   │   ├── plugin manifest
│   │   └── permission model
│   │
│   ├── reginux-helper/
│   │   └── privileged operations
│   │
│   ├── reginux-tui/
│   │
│   └── reginux-cli/
│
├── providers/
├── schemas/
├── plugins/
└── docs/
```

推荐技术：

```text
Language: Rust
TUI: Ratatui
CLI: clap
Serialization: serde
```

核心层不得依赖 Ratatui。

---

# 6. 配置逻辑树

Reginux 将不同来源的配置统一映射为逻辑配置树。

例如：

```text
System
├── Identity
│   ├── Hostname
│   └── Hosts
│
├── Locale
│   ├── Language
│   ├── Timezone
│   └── Locale Variables
│
├── Kernel
│   └── Sysctl
│
├── Storage
│   └── Mounts
│
└── Environment
    └── System Environment

Applications
├── Kitty
├── Niri
├── Fcitx5
└── MPV

Reginux
├── General
├── Interface
├── Editor
├── Keybindings
├── Safety
└── Plugins
```

用户首先面对的是逻辑配置结构，而非单纯的文件系统目录。

但 Reginux 必须始终允许用户查看真实来源。

例如：

```text
System > Locale > Language

Value:
en_US.UTF-8

Source:
/etc/locale.conf

Key:
LANG

Provider:
linux.locale
```

---

# 7. 配置来源与覆盖关系

一个配置项未来应允许拥有多个来源。

例如：

```text
Application Default
        ↓
/etc/foo.conf
        ↓
/etc/foo/conf.d/*.conf
        ↓
~/.config/foo/foo.conf
        ↓
Environment Variable
        ↓
Command Line
```

最终可以显示：

```text
Timeout

Effective value:
30

Source:
User configuration

~/.config/foo/foo.conf:12
```

覆盖关系示例：

```text
DEFAULT   10
SYSTEM    20
USER      30   ← Effective
```

第一版可以先实现单来源配置，但核心数据模型不得假设配置项永久只有单一来源。

---

# 8. Config Model

每一个配置项统一表示为类似：

```text
ConfigEntry

id
label
description
value
default_value
value_type
source
effective_source
writable
privilege
validation
metadata
```

示例：

```text
id:
linux.locale.lang

label:
Language

type:
string

value:
en_US.UTF-8

source:
/etc/locale.conf

key:
LANG

privilege:
system
```

字段类型至少支持：

```text
boolean
integer
float
string
enum
path
list
```

后续可扩展：

```text
color
keybinding
duration
size
IP address
CIDR
command
```

---

# 9. Provider

Provider 表示 Linux 中一种配置能力。

Provider 不一定等于配置文件。

例如：

```text
linux.hostname
linux.locale
linux.timezone
linux.hosts
linux.sysctl
linux.mounts
```

每个 Provider 负责：

```text
probe
read
parse
modify
validate
apply
```

例如 hostname：

```text
linux.hostname

probe:
    检测当前系统支持的 hostname 配置机制

backend:
    systemd-hostnamed
    /etc/hostname
    other
```

因此 Reginux 不应简单假设：

```text
Hostname == /etc/hostname
```

而应该是：

```text
Hostname
    ↓
Provider
    ↓
当前系统适用 backend
```

---

# 10. Capability Detection

Linux 各发行版和系统组件差异较大。

Reginux 应基于能力探测，而不是固定发行版假设。

例如：

```text
Bootloader

GRUB
✓ Installed

systemd-boot
✗ Not detected
```

或者：

```text
Network

NetworkManager
✓ Active

systemd-networkd
○ Installed
○ Inactive
```

Provider 与插件可以通过 probe 判断是否应该显示。

---

# 11. 内置 Linux 基础 Provider

第一版优先支持 Linux 中较为通用、实现简单并适合验证架构的配置。

## 11.1 Identity

```text
Hostname
Hosts
```

可能来源：

```text
/etc/hostname
/etc/hosts
```

## 11.2 Locale

```text
LANG
LC_TIME
LC_NUMERIC
LC_MESSAGES
...
```

可能来源：

```text
/etc/locale.conf
```

## 11.3 Environment

可能来源：

```text
/etc/environment
```

## 11.4 Sysctl

支持：

```text
/etc/sysctl.conf
/etc/sysctl.d/*.conf
```

第一版可先实现简单键值编辑。

例如：

```text
vm.swappiness = 60
```

## 11.5 Storage

初步只读支持：

```text
/etc/fstab
```

后续再开放结构化修改。

由于 fstab 修改错误可能导致启动问题，其写入功能不应作为第一优先级。

---

# 12. Generic Configuration Explorer

Reginux 应提供通用配置浏览器。

用户可以浏览：

```text
~/.config
/etc
```

例如：

```text
Config Files

~/.config
├── kitty
├── niri
├── fcitx5
└── mpv

/etc
├── ssh
├── systemd
├── pacman.conf
└── mkinitcpio.conf
```

第一版至少允许：

```text
查看
搜索
Raw View
Diff
```

后续增加通用结构化编辑。

---

# 13. TUI 基本布局

第一版推荐两栏结构。

```text
┌─ Reginux ──────────────────────────────────────────────────────┐
│ System   Applications   Config Files   Plugins   Reginux     │
├─────────────────────┬─────────────────────────────────────────┤
│ Configuration       │ Locale                                  │
│                     │                                         │
│ > Identity          │ LANG                                    │
│   Locale            │ > en_US.UTF-8                           │
│   Environment       │                                         │
│   Kernel            │ LC_TIME                                 │
│   Storage           │   <inherit>                             │
│                     │                                         │
│ Applications        │ Source                                  │
│   Kitty             │ /etc/locale.conf                       │
│   Niri              │                                         │
│                     │                                         │
├─────────────────────┴─────────────────────────────────────────┤
│ NORMAL │ Locale │ 2 staged changes │ /etc/locale.conf        │
│ j/k Move  h/l Open  e Edit  d Diff  / Search  ^S Apply  ? Help│
└───────────────────────────────────────────────────────────────┘
```

底部状态区域建议分成两行：

第一行显示：

```text
当前模式
当前页面
staged changes 数量
当前配置来源
```

第二行显示当前 context 最重要的快捷键。

终端高度不足时可退化为单行。

---

# 14. 配置页面视图

每个配置页面提供：

```text
Form
Raw
Diff
Info
```

四种视图必须共享同一个 staged configuration state，而不是维护多份独立数据。

---

## 14.1 Form

结构化编辑。

例如：

```text
Language

LANG
> en_US.UTF-8

LC_TIME
  <inherit>

LC_NUMERIC
  <inherit>
```

字段类型映射：

```text
boolean
    checkbox / toggle

enum
    selection list

integer
    numeric editor

float
    numeric editor

string
    text editor

path
    text editor

list
    list editor
```

---

## 14.2 Raw

显示实际文本内容。

例如：

```text
# System locale

LANG=en_US.UTF-8
LC_TIME=en_GB.UTF-8
```

原则：

> View internally, edit externally.

Reginux 不需要实现自己的完整文本编辑器。

---

## 14.3 Diff

显示所有 staged changes。

例如：

```diff
-LANG=zh_CN.UTF-8
+LANG=en_US.UTF-8
```

---

## 14.4 Info

显示：

```text
Provider:
linux.locale

Source:
/etc/locale.conf

Owner:
root:root

Permissions:
0644

Writable:
Privileged

Parser:
key-value

Validator:
built-in
```

---

# 15. Vim 风格键盘操作

Reginux TUI 从第一版开始提供 Vim 风格快捷键。

同时保留方向键。

默认导航：

```text
j / ↓
向下移动

k / ↑
向上移动

h / ←
返回上级 / 收起节点

l / →
进入 / 展开节点

Enter
进入当前项目 / 确认

g g
跳至顶部

G
跳至底部

Ctrl+u
向上翻页

Ctrl+d
向下翻页
```

通用操作：

```text
/
搜索

n
下一个搜索结果

N
上一个搜索结果

e
编辑当前配置项

r
Reload

d
打开 Diff

i
打开 Info

Tab
切换视图

Ctrl+s
Apply staged changes

u
撤销最近一次 staged 修改

?
打开快捷键帮助

Esc
取消当前操作 / 返回

q
退出当前页面 / 返回

Q
退出 Reginux
```

这些均为默认值，不得硬编码为不可修改行为。

---

# 16. Action-Based Keybinding System

快捷键不得直接与 UI handler 绑定。

Reginux 应首先定义稳定的内部 Action：

```text
navigation.down
navigation.up
navigation.left
navigation.right

navigation.top
navigation.bottom
navigation.page_up
navigation.page_down
navigation.activate

view.next
view.form
view.raw
view.diff
view.info

config.edit
config.reload

changes.undo
changes.apply

search.open
search.next
search.previous

help.keybindings

application.back
application.quit
```

快捷键只是 Action 的映射。

例如：

```text
j
↓
navigation.down
```

核心原则：

> Actions are stable; keybindings are user policy.

---

# 17. Context-Aware Keybindings

快捷键系统必须支持 context。

例如：

```text
global
browser
form
raw
diff
search
dialog
plugin_manager
```

因此同一个按键可在不同环境中执行不同操作。

例如：

```text
browser:
    e → config.edit

diff:
    e → diff.expand

search:
    e → 普通文本输入
```

---

# 18. 多键快捷键

快捷键系统应支持 Vim 风格 key sequence。

例如：

```text
gg
gr
]c
[c
Space e
```

因此数据结构不能假定一个动作只对应单个 KeyEvent。

示例：

```toml
[keybindings.browser]
"j" = "navigation.down"
"k" = "navigation.up"
"gg" = "navigation.top"
"G" = "navigation.bottom"
```

需要支持 sequence timeout。

例如：

```text
Key sequence timeout
500 ms
```

---

# 19. 自定义快捷键

用户可以自由重新绑定 Action。

例如：

```toml
[keybindings.global]
"Ctrl+s" = "changes.apply"
"q" = "application.back"
"Q" = "application.quit"

[keybindings.browser]
"j" = "navigation.down"
"k" = "navigation.up"
"h" = "navigation.left"
"l" = "navigation.right"

[keybindings.diff]
"]c" = "diff.next_change"
"[c" = "diff.previous_change"
```

同一个 Action 可以拥有多个快捷键。

例如：

```toml
[keybindings.browser]
"j" = "navigation.down"
"Down" = "navigation.down"
```

用户可以添加、删除或重置绑定。

---

# 20. 快捷键冲突检查

修改快捷键时应检测：

```text
完全相同的 binding
同一 context 中的 sequence prefix 冲突
无法解析的按键名称
不存在的 Action
```

例如：

```text
g
gg
```

如果实现采用 sequence timeout，应明确提示其行为。

---

# 21. 底部快捷键速览

TUI 底部始终保留 context-aware 快捷键提示栏。

例如主界面：

```text
j/k Move   h/l Open   / Search   e Edit   Tab View   ^S Apply   q Back
```

Diff 页面：

```text
j/k Move   ]c/[c Change   Enter Expand   e Edit   q Back
```

搜索状态：

```text
Enter Select   n/N Next/Prev   Esc Cancel
```

提示栏必须依据：

```text
当前 context
+
用户实际 keybinding
```

动态生成。

不得硬编码默认快捷键文本。

如果用户把：

```text
config.edit
```

从：

```text
e
```

改为：

```text
Space e
```

底部速览应自动变成：

```text
Space e Edit
```

---

# 22. 快捷键帮助页面

默认：

```text
?
```

打开完整快捷键帮助。

例如：

```text
Keybindings

Navigation
────────────────────────────
j / ↓       Down
k / ↑       Up
h / ←       Back
l / →       Open
gg          Top
G           Bottom

Configuration
────────────────────────────
e           Edit
r           Reload
d           Diff
Ctrl+s      Apply

Application
────────────────────────────
q           Back
Q           Quit
```

帮助页面展示实际配置，而非写死的默认值。

---

# 23. Reginux 配置 Reginux

Reginux 自身配置也遵循 Reginux 的配置理念。

用户配置文件：

```text
~/.config/reginux/config.toml
```

同时，Reginux 自身应出现在配置树：

```text
Reginux
├── General
├── Interface
├── Editor
├── Keybindings
├── Safety
└── Plugins
```

也就是说：

> Reginux 可以使用自己的配置系统配置自己。

---

# 24. Reginux 自身配置页面

例如：

```text
Reginux > Interface

Show key hints
[✓]

Key sequence timeout
[ 500 ms ]

Confirm before apply
[✓]

Default view
> Form
```

Editor 页面：

```text
Reginux > Editor

External editor
> vim

Editor arguments
> {file}

Use environment editor
[ ]
```

Keybindings：

```text
Reginux > Keybindings

Browser

Move Down
    j
    Down

Move Up
    k
    Up

Edit
    e

Apply
    Ctrl+s

                [Add] [Remove] [Reset]
```

绝大多数用户不需要手写 `config.toml`。

---

# 25. 自配置安全与恢复

修改 Reginux 自身配置也必须走 staged transaction：

```text
Edit
↓
Stage
↓
Validate
↓
Diff
↓
Apply
↓
Hot Reload
```

快捷键配置应用后应可立即热加载。

如果：

```text
~/.config/reginux/config.toml
```

无法解析，Reginux 不得因此无法启动。

应显示：

```text
Reginux configuration error

~/.config/reginux/config.toml:24

Unknown action:
navigation.dwon

Reginux started with default configuration.

> Open configuration
  Continue
```

同时提供：

```bash
reginux --safe
```

忽略所有用户配置并使用默认设置。

还可提供：

```bash
reginux --reset-keybindings
```

用于快捷键严重损坏时恢复操作能力。

---

# 26. Raw View 与 External Editor

Raw 模式第一版即支持：

```text
Raw View
External Edit
```

示例：

```text
┌─ Raw: /etc/locale.conf ──────────────────────────────────────┐
│                                                             │
│ # System locale                                             │
│ LANG=en_US.UTF-8                                            │
│ LC_TIME=en_GB.UTF-8                                         │
│                                                             │
├─────────────────────────────────────────────────────────────┤
│ e Editor   d Diff   r Reload   q Back                       │
└─────────────────────────────────────────────────────────────┘
```

按 `e` 启动配置好的外部编辑器。

---

# 27. 默认 Editor

默认编辑器：

```text
vim
```

同时支持用户显式配置。

推荐解析顺序：

```text
Reginux explicit configuration
↓
$VISUAL
↓
$EDITOR
↓
vim
```

如果用户开启：

```text
Use environment editor
```

则优先使用 `$VISUAL` / `$EDITOR`。

如果最终解析出的编辑器程序不存在，则显示：

```text
Editor 'vim' was not found.

Configure another editor?

> Configure
  Cancel
```

允许：

```text
vim
nvim
nano
helix
emacs
micro
code
自定义程序
```

---

# 28. Editor Command

编辑器配置不能只视为单个 executable。

需要支持 program + arguments。

例如：

```text
vim {file}
nvim {file}
code --wait {file}
emacsclient -c -a emacs {file}
```

其中：

```text
{file}
```

表示 Reginux 创建的 staged working file。

实现时不得使用简单 shell 字符串拼接执行，以避免 shell injection。

应解析为：

```text
program
arguments[]
```

然后通过安全的进程 API 启动。

---

# 29. External Editor 安全模型

External Editor **不得直接编辑原始配置文件**。

禁止：

```text
vim /etc/locale.conf
```

禁止：

```text
sudo vim /etc/locale.conf
```

正确流程：

```text
Original Config
      ↓
Read
      ↓
Create staged working copy
      ↓
Launch Editor
      ↓
User edits staged copy
      ↓
Editor exits
      ↓
Read staged copy
      ↓
Parse
      ↓
Validate
      ↓
Generate Diff
      ↓
Add to current transaction
```

真正 Apply 时才执行：

```text
Backup
↓
Privilege request
↓
Atomic write
↓
Validation
↓
Apply / Reload
```

因此 Vim、Neovim、Helix、Nano 等 editor 永远只需要普通用户权限。

---

# 30. External Editor 生命周期

打开终端全屏编辑器时：

```text
Reginux TUI
↓
暂时退出 alternate screen
↓
恢复正常 terminal
↓
启动 editor
↓
等待 editor 完全退出
↓
重新进入 alternate screen
↓
重新绘制 TUI
```

对于 GUI editor，例如 VS Code：

```text
code --wait {file}
```

必须等待编辑器结束，Reginux 才能重新读取 staged file。

---

# 31. Raw 与 Form 同步

Form、Raw、Diff 不维护三份独立数据。

它们都映射到同一个：

```text
Staged Configuration Model
```

例如先在 Form 修改：

```text
LANG
zh_CN.UTF-8
→
en_US.UTF-8
```

Raw 中立即反映：

```text
LANG=en_US.UTF-8
```

随后用户从 Vim 增加：

```text
LC_TIME=en_GB.UTF-8
```

退出 Vim 后 Form 也必须同步：

```text
LANG
en_US.UTF-8

LC_TIME
en_GB.UTF-8
```

---

# 32. Raw Edit 后重新解析

Editor 退出后，Reginux 必须重新解析 working copy。

成功：

```text
Editor exited.

2 staged changes detected.
```

语法错误：

```text
Configuration could not be parsed.

Line 14:
Unexpected '='

> Reopen Editor
  Discard Raw Changes
  Keep Raw Changes
```

如果插件支持 validator：

```text
Parse
↓
Plugin Validation
```

例如 OpenSSH：

```text
sshd -t
```

验证失败时仍停留在 staged state，不得写入系统。

---

# 33. 修改流程

所有修改必须首先进入 staged state。

流程：

```text
Read
 ↓
Parse
 ↓
Config Model
 ↓
User Edit
 ↓
Staged Changes
 ↓
Diff
 ↓
Validate
 ↓
Confirm
 ↓
Backup
 ↓
Atomic Write
 ↓
Apply / Reload
```

编辑时不立即覆盖原文件。

---

# 34. Apply

例如用户修改：

```text
Hostname
Locale
Sysctl
```

按：

```text
Ctrl+S
```

显示：

```text
Pending Changes

3 settings changed

/etc/hostname
/etc/locale.conf
/etc/sysctl.d/99-reginux.conf

[View Diff]

Apply changes?

> Apply
  Cancel
```

需要系统权限时才触发：

```text
Authentication required
```

随后通过 helper + polkit 完成写入。

---

# 35. 权限模型

Reginux 主程序永远以普通用户身份运行。

正常启动：

```bash
reginux
```

不推荐且无需：

```bash
sudo reginux
```

用户可以直接：

- 浏览 `/etc`
- 阅读允许普通用户读取的系统配置
- 编辑自己的 `~/.config`

只有真正写入需要 root 权限的位置时才提权。

---

# 36. Privileged Helper

Reginux 使用独立、最小权限的 helper 完成系统级修改。

架构：

```text
reginux-tui
     │
     │ IPC
     ↓
reginux-core
     │
     │ privileged request
     ↓
reginux-helper
     │
     ↓
polkit
     │
     ↓
system configuration
```

helper 不提供：

```text
run_as_root(command)
```

这样的万能 root shell。

只暴露受控能力，例如：

```text
write_file
replace_file
create_file
remove_file
set_permissions
reload_service
run_registered_validator
```

---

# 37. 原子写入

修改配置不得直接 truncate 原文件。

推荐：

```text
读取原文件
↓
生成新内容
↓
写临时文件
↓
fsync
↓
rename
```

并尽可能保留：

```text
owner
group
mode
```

---

# 38. Backup 与 Rollback

每次修改前创建备份。

用户配置：

```text
~/.local/state/reginux/backups/
```

系统配置：

```text
/var/lib/reginux/backups/
```

备份按 transaction 管理。

例如：

```text
2026-08-09T10-42-31/
├── manifest.json
├── etc_hostname
└── etc_locale.conf
```

未来提供：

```text
History
├── Transaction 42
├── Transaction 41
└── Transaction 40
```

允许：

```text
View
Diff
Rollback
```

---

# 39. Lossless Editing

Reginux 不应简单使用：

```text
parse
↓
object
↓
serialize
```

然后破坏原有文件格式。

应尽可能保留：

```text
comments
whitespace
ordering
unknown fields
formatting
```

例如：

```text
# I need this because foo breaks on my machine
vm.example = 1
```

修改其他字段后，这条注释不得消失。

对于暂时无法实现 lossless parser 的格式，应优先采用最小文本 patch，而非完整重写。

---

# 40. 插件系统

Reginux 的非基础系统配置主要通过插件提供。

插件分为：

```text
Schema Plugin
Procedural Plugin
```

---

# 41. Schema Plugin

声明式插件。

不执行任意代码。

例如：

```yaml
id: kitty
name: Kitty

probe:
  files:
    - ~/.config/kitty/kitty.conf

files:
  - id: main
    path: ~/.config/kitty/kitty.conf
    format: kitty

sections:
  appearance:
    label: Appearance

    fields:
      font_size:
        label: Font Size
        type: float
        min: 6
        max: 72
        default: 11
```

安装插件后自动生成：

```text
Applications
└── Kitty
    └── Appearance
        └── Font Size
```

Schema Plugin 应是插件生态的首选形式。

优势：

```text
无代码
↓
容易审核
↓
安全风险低
↓
容易贡献
↓
容易维护
```

---

# 42. Procedural Plugin

用于无法通过声明式 schema 表达的系统。

例如：

```text
NetworkManager
systemd
GRUB
mkinitcpio
复杂 KDL
PipeWire
```

插件 API 示例：

```text
probe()
read()
get_schema()
get_values()
validate()
apply()
```

Procedural Plugin 默认运行在普通用户权限下。

---

# 43. 插件权限

插件 manifest 必须声明权限。

例如：

```yaml
id: networkmanager

permissions:
  read:
    - /etc/NetworkManager/**

  write:
    - /etc/NetworkManager/**

  commands:
    - /usr/bin/nmcli

  services:
    - NetworkManager.service
```

Reginux 不允许插件直接获得 root shell。

需要 root 权限的行为必须由 helper 根据 manifest 验证后完成。

---

# 44. 插件信任等级

界面可显示：

```text
Built-in
Trusted

Schema
Safe

Code Plugin
User Code

Privileged Plugin
System Access
```

其中 Schema Plugin 风险最低。

---

# 45. 插件分类建议

插件不一定等于单个应用程序。

可以按：

```text
plugins/
├── apps/
│   ├── kitty
│   ├── niri
│   └── mpv
│
├── desktop/
│   ├── gtk
│   ├── qt
│   └── xdg
│
├── system/
│   ├── systemd
│   ├── networkmanager
│   ├── pipewire
│   └── grub
│
└── distro/
    ├── arch
    ├── debian
    └── fedora
```

组织。

核心程序不需要理解 `pacman.conf`、APT、Niri 等具体软件。

---

# 46. 搜索

按：

```text
/
```

进入全局搜索。

例如：

```text
Search: proxy
```

返回：

```text
Environment
    HTTP_PROXY

Git
    http.proxy

NetworkManager
    Proxy

Pacman
    XferCommand
```

搜索对象包括：

```text
配置项名称
描述
配置路径
Provider
Plugin
```

---

# 47. 第一版默认快捷键

建议默认值：

```text
Navigation
────────────────────────

j / Down          navigation.down
k / Up            navigation.up
h / Left          navigation.left
l / Right         navigation.right

gg                navigation.top
G                 navigation.bottom

Ctrl+u            navigation.page_up
Ctrl+d            navigation.page_down

Enter             navigation.activate


General
────────────────────────

Esc               application.back

/                 search.open
n                 search.next
N                 search.previous

e                 config.edit
r                 config.reload
d                 view.diff
i                 view.info

Tab               view.next

u                 changes.undo
Ctrl+s            changes.apply

?                 help.keybindings

q                 application.back
Q                 application.quit
```

默认 profile 应可以完整重置。

---

# 48. 第一版明确不做

为保证原型聚焦，暂不实现：

```text
GUI

在线插件商店

AI 配置助手

复杂插件 sandbox

远程主机管理

配置同步

多用户集中管理

云同步

完整 distro package manager GUI

systemd 全功能管理器

网络配置管理器

复杂 bootloader 编辑

复杂 fstab 修改
```

---

# 49. 第一版建议支持

## Core

```text
Provider architecture
Config model
Staged changes
Diff
Validation
Backup
Atomic write
Privilege boundary
```

## TUI

```text
Navigation
Vim-style keybindings
Configurable keymap
Dynamic key hints
Form
Raw
External Editor
Diff
Info
Search
Apply
Self-configuration
```

## Linux Built-in Providers

```text
Hostname
Hosts
Locale
Environment
Sysctl
```

## File Format

优先：

```text
key=value
simple line-oriented configuration
```

不以 TOML / YAML / JSON 的格式数量作为第一版目标。

---

# 50. 第一版架构验证目标

第一版最重要的任务不是“支持多少软件”，而是证明以下完整链路成立：

```text
Detect
↓
Read
↓
Parse
↓
Model
↓
TUI
↓
Edit
↓
Stage
↓
Diff
↓
Validate
↓
Backup
↓
Privilege
↓
Atomic Write
↓
Reload
```

同时验证：

> 在不修改 Reginux 核心代码的情况下，通过新增一个 Schema Plugin，即可为一个新的应用程序提供结构化设置界面。

建议第一个 Schema Plugin：

```text
Kitty
```

原因：

```text
配置简单
文本格式直观
风险低
容易验证
适合 ~/.config 用户级权限测试
```

---

# 51. 未来 CLI

Core 不得依赖 TUI，因此未来可自然提供：

```bash
reginux get system.hostname
```

```bash
reginux get linux.locale.lang
```

```bash
reginux set kitty.font_size 12
```

```bash
reginux diff
```

```bash
reginux apply
```

```bash
reginux history
```

```bash
reginux rollback 42
```

---

# 52. 未来 GUI

未来 GUI 只是另一层 frontend：

```text
                Reginux Core
                     │
        ┌────────────┼────────────┐
        │            │            │
       TUI          CLI          GUI
```

Provider、schema、插件、权限、事务、diff 等逻辑均不得重复实现。

---

# 53. 未来 Agent API

未来 AI Agent 不需要直接：

```bash
sed -i ...
```

而可以请求：

```text
set(
    "kitty.font_size",
    12
)
```

Reginux 仍然负责：

```text
Schema validation
Diff
Permission check
Backup
Transaction
Write
```

从而把“修改 Linux 配置”变成安全、结构化、可审计的操作。

---

# 54. 长期方向

Reginux 最终可以成为 Linux 系统上的统一配置访问层。

传统配置：

```text
Application
↓
Config File
```

Reginux：

```text
Application / System
        ↓
Provider / Plugin
        ↓
Reginux Config Model
        ↓
┌──────────┬──────────┬───────────┐
│   TUI    │   CLI    │    GUI    │
└──────────┴──────────┴───────────┘
                     │
                     ↓
                Agent API
```

它不替代 Linux 原本的配置机制，而是为其提供：

- 统一理解
- 统一展示
- 安全编辑
- 来源追踪
- 权限控制
- 修改审计
- 回滚能力
- 可扩展接口

最终目标不是“把 Linux 变成 Windows Registry”，而是：

> 为 Linux 已有的分散配置世界建立一个统一、可扩展且不夺取所有权的访问层。
