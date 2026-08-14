# Reginux 插件系统设计方案 v1

状态：1.0.0 已实现并冻结的 v1 规范
适用范围：Reginux Core、TUI，以及未来 CLI/GUI 前端  
本文不规定具体 Rust 类型或实现顺序；它定义插件系统应遵循的稳定边界与用户可见行为。

## 1. 目标与边界

Reginux 是 Linux 配置的统一、可追溯访问层，而不是新的配置数据库。配置文件、系统服务和应用自身接口仍然是事实来源（source of truth）。插件的职责是让 Core 能够理解这些来源，而不是接管它们。

插件系统的目标：

- 支持纯配置文件、包含导入关系的配置文件、CLI/IPC 控制面，以及难以用通用规则解析的脚本化配置。
- 让所有来源在界面中呈现为一致的条目树、状态、暂存修改、校验和变更预览。
- 将读取、Diff、备份、事务、回滚、权限提升和最终应用集中在 Reginux Core。
- 让低风险插件足够易于编写；让高能力插件具有可审计、可批准、可撤销的权限边界。

非目标：

- 不执行用户应用的完整配置语言，也不尝试模拟 Neovim、Shell 等应用的真实运行时。
- 不让插件获得 root shell、直接写文件或自行重载服务。
- 不将 TUI 控件、快捷键或任意终端文本作为插件协议的一部分。
- 不承诺所有外部 CLI/IPC 操作都可以拥有文件事务同等的原子性和回滚能力。

## 2. 核心原则

### 2.1 Core 是唯一执行者

插件可以提供描述、解码、校验和计划；Core 是唯一能够：

- 解析和授权真实路径；
- 发起 CLI、D-Bus 或受控 IPC 调用；
- 保存原始快照、计算 Diff、备份和回滚；
- 写入文件；
- 在精确、最小的写入步骤申请权限；
- 执行已经确认的外部操作并验证结果。

因此，插件协议中不提供可由插件任意实现的 `apply`、`shell` 或 `run` 钩子。

### 2.2 插件类型是组合，不是三套孤岛

“声明式、交互式、脚本式”是用户和作者理解插件的三种入口，但 Core 内部应将其拆解为可组合层：

| 层 | 输入 | 输出 | 例子 |
|---|---|---|---|
| Provider | 外部来源 | 原始结果 | 文件、CLI、D-Bus、Unix socket |
| Resolver | 根来源 | 来源图 | Kitty `include`、Niri `include` |
| Decoder / Transform | 原始字节或消息 | 标准化 Snapshot / Document Model | JSON、KV、Lua 解析 |
| Presentation | Snapshot / Document Model | 条目树和字段 | 分组、动态资源、状态条目 |
| Planner | 暂存变更 | 受约束的 Plan | 文本编辑、命令调用、回滚信息 |

对应关系：

| 面向作者的类型 | 典型组合 |
|---|---|
| 声明式 Schema | File Provider + Include Resolver + Declarative Decoder + File Planner |
| 交互式 Adapter | Command / IPC Provider + Decoder + Presentation + Adapter Planner |
| 脚本式 Transform | 作为 Decoder 或 Planner 挂接在 Schema / Adapter 上 |

这意味着 Transform 不是绕开 Schema/Adapter 的第四套应用框架；它是受限的转换能力。

### 2.3 所有类型共享用户流程

```text
发现并校验插件
  → 读取来源 / 获取状态
  → 标准化为字段与状态
  → 用户暂存修改
  → 校验并生成计划
  → 统一 Diff、风险提示、确认
  → Core 应用、验证、备份或补偿
  → 刷新来源并报告最终状态
```

前端不得为任意插件提供独立的“直接执行”按钮。用户应始终知道：这一次修改影响了哪些真实文件或外部状态、是否可回滚、需要何种权限以及最终验证是否成功。

## 3. 通用数据模型

### 3.1 SourceRef：来源不是总是文件

Core 应将来源建模为带类型的引用，而不是一律使用路径字符串：

```text
SourceRef
├── File { source_id, absolute_path, scope }
├── Command { provider_id, operation_id }
├── DBus { bus, service, object_path, interface }
└── Socket { endpoint_id }
```

`plugin://...`、CLI 输出和 IPC 状态不能伪装为本地 `PathBuf`。文件来源才支持 Raw 文件视图；运行时来源应显示受清洗、截断的原始响应、刷新时间和协议描述。

### 3.2 Field：统一的用户可编辑单元

所有前端只消费 Core 输出的 `Field` 与 `Node`，而不直接消费插件原始输出。字段至少应包含：

```text
field_id              稳定、全局唯一的标识
label / description   已清洗的展示文本
value                 标准化值
value_type            string | integer | float | bool | enum | list | secret
source_ref            实际来源
edit_capability       none | file | adapter | transform
sensitivity           normal | secret
validation            类型、范围、枚举或受限规则
origin                插件、文档位置或状态路径
```

`writable: bool` 不足以表达现实情况：文件可编辑并不意味着当前用户有权限；Adapter 可提供可修改操作，但可能没有安全回滚；运行状态可能只读。因此编辑能力、实际权限、事务保证和当前可编辑性应分别显示。

### 3.3 Snapshot：解码后的纯数据快照

Provider 的原始响应先经 Decoder 变成受限数据，不直接进入 UI。标准化 Snapshot 仅允许 JSON 基础值：对象、数组、字符串、数值、布尔和 null。

```json
{
  "protocol": 1,
  "captured_at": "2026-08-12T12:00:00Z",
  "data": {
    "connections": [
      {
        "uuid": "8c0c...",
        "name": "Home Wi-Fi",
        "autoconnect": true,
        "state": "activated"
      }
    ]
  },
  "diagnostics": []
}
```

Snapshot 不包含 UI 控件定义、任意命令、终端转义序列或可执行代码。

### 3.4 Plan：唯一可应用的变更计划

用户暂存修改后，插件或内建 Planner 只能生成 Plan。Core 校验 Plan 后，才将其呈现给用户并执行。

Plan 由以下两种操作组成：

```text
FileEdit
  source_id
  expected_digest
  byte_range / structured_patch
  replacement

AdapterOperation
  operation_id
  typed_arguments
  precondition_snapshot_or_digest
  optional_compensation_operation
  verification_operation
```

Plan 不得携带 shell 字符串、未声明路径、任意可执行文件或“由插件调用的 apply 函数”。所有操作必须引用 manifest 中预先声明的来源或 operation。

## 4. Manifest 通用结构

所有类型使用 TOML，减少解析器、工具与文档的分裂。

```toml
schema_version = 1

[plugin]
id = "org.example.kitty"
name = "Kitty"
version = "1.0.0"
kind = "schema" # schema | adapter | transform
description = "Manage selected Kitty settings"
```

通用校验要求：

- `id` 使用受限 ASCII 标识符，并在所有已加载插件中唯一。
- 文本字段有长度上限，且禁止控制字符、ANSI 转义和换行注入。
- manifest、脚本、单个来源、递归来源总数与总字节数都有明确上限。
- manifest 路径、插件目录、命令和来源文件均需经过规范化和符号链接策略检查。
- 同一目录中多个 manifest、重复 ID 或被遮蔽插件必须产生可见诊断，不能静默“先到先得”。

## 5. 声明式 Schema 插件

### 5.1 文件图与环境变量

Schema 插件通过 `sources` 声明根文件。路径模板仅允许 Core 白名单中的变量，例如 `HOME`、`XDG_CONFIG_HOME`、`XDG_STATE_HOME`。展开后必须得到绝对路径。

变量应基于启动 Reginux 的原始普通用户会话解析；即使最终写入需要提权，也不得将 `HOME` 偷换为 `/root`。

```toml
[sources.main]
path = "${XDG_CONFIG_HOME}/kitty/kitty.conf"
format = "kitty"
scope = "user"
max_bytes = 1048576
```

用户 Schema 默认只能声明用户目录内的来源；系统来源需要系统级受信任插件和更严格的目录所有权/权限校验。

### 5.2 引用与导入解析

支持引用的格式通过 `imports` 描述，而非让 Core 猜测语法：

```toml
[sources.main.imports]
keyword = "include"
syntax = "shell_words"
relative_to = "including_file"
glob = true
recursive = true
max_depth = 16
max_files = 128
allowed_roots = ["${XDG_CONFIG_HOME}/kitty"]
```

Resolver 必须：

- 构建显式来源图并检测循环；
- 对每个被引用文件执行同样的路径、符号链接、大小和权限检查；
- 保留来源关系，允许 UI 展示“字段来自哪个 import 文件”；
- 防止 `..`、绝对路径、glob 和符号链接绕过 `allowed_roots`；
- 在图不完整或解析失败时给出诊断，不以不可靠的猜测继续写入。

### 5.3 字段与写入目标

```toml
[fields.appearance.font_size]
source = "main"
key = "font_size"
type = "float"
min = 6
max = 72
write_target = "origin" # origin | root | explicit_source
```

字段 ID 由 `plugin_id.section.field` 构成，不能只由键名构成。`write_target = origin` 表示修改真正定义该值的文件；若未出现，manifest 必须定义确定的插入策略，例如主文件末尾、指定 section 或显式覆盖文件。Core 不可自行猜测插入位置。

所有格式解析与写回应共享同一解析器或 AST；读取阶段“取第一个值”、写入阶段“取最后一个值”是禁止的行为。

## 6. 交互式 Adapter 插件

### 6.1 适用范围

Adapter 用于状态或修改语义必须通过 CLI、D-Bus、Unix socket 等接口完成的应用。它不是“可以执行任意命令的 Procedural 插件”。

Adapter 把外部接口映射为有限的操作：

| 操作 | 含义 |
|---|---|
| `snapshot` | 获取可展示的当前状态 |
| `refresh` | 重新获取动态状态 |
| `validate` | 校验暂存修改是否可接受 |
| `plan` | 将已校验修改映射为受约束的 AdapterOperation |
| `verify` | 执行后再次读取并确认结果 |

不提供独立的插件 `apply`。

### 6.2 Provider 与执行安全

命令 transport 使用绝对程序路径和参数数组，禁止 shell：

```toml
[transport]
kind = "command"
program = "/usr/bin/nmcli"

[operations.snapshot]
args = ["--terse", "--fields", "NAME,UUID,AUTOCONNECT", "connection", "show"]
decoder = "delimited"
```

命令参数只能引用 manifest 已声明、具有明确类型的占位符。Core 必须清空继承环境，只传递最小白名单变量与固定安全 `PATH`。Adapter 的命令、D-Bus 接口、潜在服务影响、网络需求和权限要求必须在插件详情页显示。

Core 应优先提供内建 D-Bus transport；插件不得拿到可任意连接 socket 的通用能力。运行过程必须有超时、输出限制、进程组终止和资源限制。

### 6.3 Decoder：从杂乱输出到 Snapshot

CLI 与 IPC 的原始响应不需要天然遵循 Reginux 协议。要求的是：它们必须经 Decoder 转换为标准化 Snapshot。

提供三级 Decoder：

1. **内建 Decoder**：JSON、JSON Lines、`key=value`、INI、单值文本、受控 CSV/分隔符表格、固定状态行。
2. **声明式投影**：在内建 Decoder 得到的数据树上，使用受限 JSON Pointer 风格路径提取集合、稳定键、标签和字段。
3. **Lua Transform Decoder**：仅在前两者无法可靠描述人类可读表格、复杂转义或特殊协议时，将原始 bytes 转换为 Snapshot。

正则仅适合明确的单值提取，不应作为复杂结构的默认解析方案。

### 6.4 条目树：manifest 定义，Snapshot 填充

Adapter 输出不能任意绘制 UI；条目结构由 manifest 静态声明，Snapshot 只提供动态数据。

```toml
[[nodes]]
id = "connections"
kind = "group"
label = "Connections"

[[nodes]]
id = "connection"
kind = "resource"
parent = "connections"
collection = "/connections"
key = "/uuid"
label = "/name"

[[nodes.fields]]
id = "autoconnect"
label = "Auto-connect"
type = "bool"
value = "/autoconnect"
operation = "set_autoconnect"

[[nodes.fields]]
id = "state"
label = "Current state"
type = "enum"
value = "/state"
read_only = true
```

运行时字段 ID 使用稳定资源键：

```text
org.example.network.connections.8c0c....autoconnect
```

不得使用数组下标或显示名称作为 ID。名称会变，数组顺序会变；UUID 或由插件明确声明的稳定键才可用于暂存、刷新和 diff 对齐。

### 6.5 回滚语义

Adapter 不一定能实现原子事务。每个修改能力必须声明事务保证：

```text
atomic              由外部接口保证原子性
compensatable       Core 可基于前置快照尝试补偿
best_effort         可验证但无法可靠回滚
irreversible        不允许由普通配置编辑流程调用
```

UI 的确认页必须展示这一点。若主调用已经返回失败（包括超时、断连或非零结果），且
manifest 声明了 compensation，Core 必须先尝试补偿，再报告“主操作失败并已补偿”或
“主操作失败且补偿失败”。若命令成功返回但 `verify` 未达到预期，也必须报告“操作已执行，
最终状态未验证”，而不是报告成功。

## 7. 脚本式 Transform 插件

### 7.1 角色

Transform 处理无法用内建解析器可靠表达的配置：复杂文本语法、局部 DSL，或像 Neovim `init.lua` 那样的脚本化静态配置。它既可作为 Schema 的文件 Decoder/Planner，也可作为 Adapter 的 Snapshot Decoder。

Transform 不获得真正的 I/O 或执行权；它是纯转换沙箱：

```text
已声明来源的文本 / Provider 原始结果
  → Lua Transform
  → Document Model、Snapshot、诊断或编辑计划
```

### 7.2 语言选择

v1 建议仅支持 Lua 5.4，并由 Rust 以 `mlua` 嵌入和捆绑。理由：轻量、成熟、与大量 Linux 配置语境和 Neovim 用户习惯契合。

不依赖用户系统 Lua，也不同时引入 Rhai 等第二种脚本 ABI。若未来需要面向 Reginux 自身的规则语言，再独立评估；v1 不应承担两套沙箱和生态维护成本。

### 7.3 沙箱能力

Lua runtime 仅保留语言基础库和 Core 注入的纯函数 API。明确移除：

- `os`、`io`、`package`、动态 `require`、`debug`；
- 网络、FFI、进程创建、时钟和随机外部副作用；
- 真实绝对路径与任何未声明来源的访问；
- 直接应用修改的函数。

Core 还应限制执行时间、指令数、内存和结果大小。

### 7.4 Document Model 与编辑计划

manifest 声明字段与条目位置，Lua 提供绑定值和文本位置：

```toml
[[nodes]]
id = "editor"
kind = "group"
label = "Editor"

[[nodes.fields]]
id = "tabstop"
label = "Tab width"
type = "integer"
binding = "editor.tabstop"
source = "init_lua"
```

Lua 返回：

```json
{
  "bindings": {
    "editor.tabstop": {
      "value": 4,
      "source_id": "init_lua",
      "range": { "start": 102, "end": 103 }
    }
  }
}
```

用户暂存新值后，Transform 只返回受约束的文本编辑：

```json
{
  "edits": [
    {
      "source_id": "init_lua",
      "expected_sha256": "...",
      "start": 102,
      "end": 103,
      "replacement": "2"
    }
  ]
}
```

Core 校验来源、摘要、范围、重叠、大小和重新解析结果，再把它纳入普通文件事务。

### 7.5 Neovim 的明确边界

v1 可以识别并编辑静态、直接的赋值：

```lua
vim.o.tabstop = 4
vim.opt.number = true
```

对于依赖 `require()`、函数、条件分支、插件加载或外部环境的动态表达式，必须显示为“检测到，但不可安全编辑”，并允许用户进入 Raw 视图。Reginux 不能为了便利而执行用户的完整 `init.lua`。

## 8. Presentation Model：如何形成条目与条目树

Schema、Adapter 与 Transform 最终都投影到同一种节点模型：

```text
Node
├── Group：静态分组
├── Resource：由 Snapshot / Document Model 动态生成的资源
├── Field：可读或可暂存修改的属性
└── Action：只允许 Core 认可的刷新、验证或计划动作
```

展示结构由 manifest 预声明；动态数据只能填充已声明的 `Resource` 模板。Transform 可报告发现了哪些资源，manifest 决定这些资源位于何处、使用哪些字段和何种编辑能力。

例如 Transform 可返回：

```json
{
  "records": [
    {
      "kind": "lsp_server",
      "key": "lua_ls",
      "properties": { "name": "lua_ls", "enabled": true }
    }
  ]
}
```

而 manifest 声明 `lsp_server` 被放在“Language Servers”组中，`enabled` 是布尔编辑字段。这样插件输出不能注入任意层级、任意动作或不受审计的显示文本。

## 9. 安全与信任模型

### 9.1 插件来源

| 来源 | 默认行为 |
|---|---|
| Core 内置 | 可用 |
| 系统 Schema | 目录与文件满足所有权、权限检查后可用 |
| 用户 Schema | 可用，但来源默认受限于用户目录 |
| Adapter | 只读能力可显示；修改能力须逐插件批准 |
| Transform | 仅在沙箱执行；第三方 Transform 须逐插件批准 |

批准绑定插件 ID、manifest 哈希、脚本哈希和 Adapter 程序哈希。任一哈希变化后应重新批准。全局“允许所有 Procedural 插件”只能作为开发模式开关，不能作为发布版默认策略。

### 9.2 路径与内容安全

- 拒绝通过 `..`、绝对引用、glob 或符号链接逃逸允许根目录。
- 对系统插件要求不可被普通用户写入；对来源文件检查常规文件属性、大小与链接情况。
- manifest、文件内容、CLI/IPC 输出和脚本结果都应限制大小。
- 所有插件显示文本、错误输出和非敏感原始响应均清理 C0 控制字符与 ESC，并截断到安全长度；
  原始文件视图保留换行布局但移除终端控制序列。含 `secret`/`sensitive` 字段的 Adapter
  不把原始响应放入条目元数据。
- `secret` 字段默认脱敏，不进入搜索、诊断、备份说明或普通日志。

### 9.3 权限提升

主进程始终以普通用户运行。公共低级 staging 只允许 HOME 或临时目录下的绝对路径；需要修改系统文件时，仅在 Core 已完成校验、生成精确计划之后，通过最小权限 helper 或 polkit 写入明确的目标。helper 必须重新验证来源、摘要、操作范围和 manifest 信任状态，而不是接受自由路径或自由命令。

## 10. UX 约定

### 10.1 配置页

每个字段应标识来源类型：

- `File-backed`：显示精确文件路径，可进入 Raw。
- `Imported`：显示来自哪个 include 文件和引用链。
- `Runtime state`：显示为状态，不伪装成文件，也不提供 Raw 编辑。
- `Enhanced parser`：由 Transform 提供，但仍显示真实源文件。

按 `r` 的行为应统一为“刷新当前来源”：文件重新读取，Adapter 调用 `refresh`，Transform 重新解析。若不可刷新，必须给出明确状态，而不是静默无效。

### 10.2 插件详情页

每个插件应显示：

- 类型、版本、来源、信任级别、批准状态；
- manifest / 脚本 / 命令哈希；
- 文件图、引用关系、允许根目录和受影响来源；
- Adapter 的命令/IPC、声明能力、回滚保证和可能影响的服务；
- 最近成功刷新时间、最近诊断与可执行的“重新读取”；
- 对敏感来源、不可回滚操作和未批准修改能力的明确警告。

### 10.3 确认页

Diff 和确认页以真实影响为中心：

- 文件变更按文件分组，显示备份与原子写入策略；
- Adapter 操作显示调用的声明操作、类型化参数、前置条件、验证方式和回滚保证；
- 未验证、不可回滚或需要提权的操作必须单独突出；
- 仅在所有计划都通过 Core 校验后允许确认。

## 11. 兼容性与失败行为

- 插件 schema 或 protocol 版本不兼容时，禁用插件并说明原因；不得半加载。
- Decoder 失败时，保留诊断和安全截断的原始结果；不得将不确定文本当作配置值。
- 刷新失败不清除上一次已验证快照，但必须标记为过期并记录失败时间。
- 暂存修改前后来源摘要变化时，废弃相关暂存并要求用户重新读取，防止覆盖外部修改。
- 多插件声明同一字段 ID、同一来源写入权或同一资源键时，Core 必须拒绝歧义，而不是按扫描顺序选择。

## 12. 规范收敛结论

v1 应稳定以下跨类型协议：

```text
ProviderResult → Decoder → Snapshot / Document Model → Presentation Model → Field Tree
Staged Changes → Planner → Plan → Core Transaction 或 Adapter Execution → Verification
```

其中 `Snapshot`、`Document Model`、`Field` 和 `Plan` 是稳定公共接口；Schema、Adapter、Transform 只是它们的不同组合。这样既能覆盖包含 import 的传统配置文件、CLI/IPC 状态，也能处理 Neovim 一类脚本化配置，同时不会放弃 Reginux 最重要的可追溯、安全与统一体验。

## 13. v1 固定决策

1.0.0 已将原开放问题固定为以下协议，不再由单个插件自行选择：

1. 路径变量白名单为 `HOME`、`XDG_CONFIG_HOME`、`XDG_STATE_HOME`、
   `XDG_DATA_HOME`、`XDG_RUNTIME_DIR`；system source 不借用 root HOME。
2. v1 行式 import 固定为 shell-words、including-file 相对路径和 manifest allowed
   roots；TOML、INI、KDL 不声明行式 import。
3. Adapter 内建 Command、session/system D-Bus 和固定 Unix socket transport；所有
   端点、方法、参数类型、read paths、network 和 peer 都在 manifest 静态声明。
4. Core 在 compensatable operation 失败时自动执行 manifest 的补偿；该保证必须有
   verify 和 compensation，最终错误明确区分验证失败与补偿失败。
5. Transform ABI 固定为 vendored Lua 5.4 纯数据函数；内存、指令、数据深度、节点、
   结果与编辑大小均由 Core 限制。
6. `secret`/`sensitive` 在 v1 脱敏并强制只读，不进入搜索或普通诊断，也不允许把
   掩码通过 Raw/结构化编辑写回。
7. v1 不定义签名仓库；Adapter/Transform 使用 ID+manifest+脚本/程序组合 SHA-256
   批准，系统 Schema 使用 root-owned 安装目录和 helper 二次验证。
8. 无静态 binding 的动态配置统一显示不可安全编辑；文件来源仍提供 Raw 引导，
   runtime stale 状态保留快照但强制只读。

当前 manifest 字段、限制值与完整示例以 `docs/PLUGIN.md` 为准。
