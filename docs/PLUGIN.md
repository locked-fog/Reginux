# Reginux 插件规范 v1（Reginux 1.0.0）

本文是插件作者文档。`docs/PLUGIN-SYSTEM-DESIGN-V1.md` 解释设计原则；本文给出当前
Rust 实现接受的 manifest、运行语义和示例。

## 1. 共同模型

所有插件是 TOML：

```toml
schema_version = 1

[plugin]
id = "org.example.app"
name = "Example"
version = "1.0.0"
kind = "schema" # schema | adapter | transform
description = "Manage selected Example settings"
```

ID、section、field、operation、node 使用受限 ASCII 标识符。未知字段、未知版本、
重复插件 ID、重复全局字段 ID和重复写目标都会禁用相关插件/条目并产生诊断。

路径变量白名单：`${HOME}`、`${XDG_CONFIG_HOME}`、`${XDG_STATE_HOME}`、
`${XDG_DATA_HOME}`、`${XDG_RUNTIME_DIR}`。展开后必须为绝对路径；不支持 `$NAME`、
shell substitution 或任意环境继承。

Adapter 和 Transform 的批准摘要覆盖 manifest；Transform 还覆盖脚本，Command
Adapter 还覆盖程序。TUI Plugins 视图按 `a` 批准当前 ID+摘要，`x` 撤销，`r`
刷新；CLI `--allow-plugin ID` 仅临时批准一个明确 ID。

## 2. Schema 插件

### 2.1 来源

```toml
[sources.main]
path = "${XDG_CONFIG_HOME}/kitty/kitty.conf"
format = "kitty"
scope = "user" # user | system
max_bytes = 1048576
```

支持格式：

| format | key 语法 | 回写 |
|---|---|---|
| `kitty` / `whitespace` | `key value` | 最后有效赋值，保留 inline comment |
| `key_value` / `equals` | `key=value` | 最后有效赋值，保留 inline comment |
| `toml` | `section.key` | `toml_edit` 保格式 AST |
| `ini` | `section.key` 或 `key` | section-aware 保留其他行 |
| `kdl` | `parent.node` | KDL AST，第一个 positional value |
| `lua` | 仅 Transform 来源 | 由 Lua binding/plan 处理 |

TOML/INI/KDL 不允许行式 imports。Kitty/KV 可声明：

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

Core 按文本顺序解析 include 图，检测循环、symlink、`..`、glob 逃逸、深度、数量和
总大小。`origin` 写回真正产生最后有效值的文件。

### 2.2 字段

```toml
[fields.appearance.font_size]
source = "main"
key = "font_size"
label = "Font size"
description = "Default size in points"
type = "float"
min = 6
max = 72
default = 11
write_target = "origin" # origin | root | explicit_source
insert = "end"          # 值可能缺失时必填：end | section
```

`write_target = "explicit_source"` 时同时设置 `explicit_source = "overrides"`，值必须
是另一个已声明 source ID。字段类型为 `boolean`、`integer`、`float`、`string`、
`enum`、`path`、`list`、`raw`、`secret`；enum 使用 `values`。`secret`/`sensitive`
会脱敏并强制只读，防止把掩码写回。

system source 只允许安装在 `/usr/share/reginux/plugins` 且整条路径 root-owned、不可
组/全局写。最终写入由 privileged helper 再验证 manifest 摘要和来源图。

## 3. Adapter 插件

Adapter manifest 静态声明 transport、operation 和条目模板；插件不提供可执行
`apply` hook。

### 3.1 Command transport

```toml
[transport]
kind = "command"
program = "/usr/bin/examplectl"
read_paths = ["${XDG_CONFIG_HOME}/example"]
network = "none" # none | local | internet
```

program 必须是绝对非 symlink 普通文件。参数始终为数组，不经过 shell。执行环境只含
固定 PATH/LANG/LC_ALL，并强制进入 Landlock+seccomp+rlimit 沙箱。read_paths 只增
加只读访问；network 决定允许的 socket 范围。

### 3.2 D-Bus transport

```toml
[transport]
kind = "dbus"
bus = "session" # session | system
service = "org.example.Service"
object_path = "/org/example/Service"
interface = "org.example.Service"

[operations.snapshot]
member = "ListItems"
arg_types = []
args = []
reply_type = "json"
decoder = "json"
```

D-Bus operation 必须有 member，且 `arg_types` 与 args 一一对应。参数类型使用字段类型
中的 boolean/integer/float/string/enum/path/secret；返回类型支持 `unit`、`string`、
`json`、`bytes`、`boolean`、`integer`、`float`、`string_array`、`string_map`。D-Bus
wire body 与解码后的响应均限制为 1 MiB；超限调用失败。

### 3.3 Unix socket transport

```toml
[transport]
kind = "unix_socket"
endpoint = "${XDG_RUNTIME_DIR}/example.sock"
peer = "self" # self | root | numeric UID

[operations.snapshot]
request = "status"
framing = "line" # line | eof | length_prefixed
decoder = "json"
timeout_ms = 1000
```

Core 只连接该 endpoint，并以 Linux `SO_PEERCRED` 验证对端 UID。request 支持受限
占位符，不提供通用 socket 能力；请求和响应各限制为 1 MiB。适配器超时、断连或调用
失败时，如果 operation 声明了 compensation，Core 会先尝试补偿并区分主失败与补偿失败。

### 3.4 Decoder

operation 的原始 bytes 必须先转为 JSON 基础数据：

| decoder | 配置/输出 |
|---|---|
| `json` | JSON value |
| `json_lines` | 每个非空行一个 JSON value，输出数组 |
| `key_value` | 严格 `key=value` 对象 |
| `ini` | section 变嵌套对象 |
| `csv` | 默认首行 header；`delimiter` 可改一个 byte；`headers=false` 时声明 columns |
| `delimited` | 必须声明一个 byte `delimiter` 和 `columns`，支持标准 quoting |
| `fixed_status` | 每行匹配受大小限制的命名 capture 正则 `pattern` |
| `single` | trimmed string |
| `single_record` | `[{id="singleton", value=...}]` 形状 |
| `lua` | `[transform]` 的 decode entrypoint |

`decoder_config.collection = "items"` 会把结果包成 `{ "items": result }`。例如：

```toml
[operations.snapshot]
args = ["list", "--csv"]
decoder = "csv"

[operations.snapshot.decoder_config]
collection = "items"
delimiter = ","
headers = true
```

### 3.5 Node/Field 投影

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
type = "boolean"
value = "/autoconnect"
operation = "set_autoconnect"
```

collection、key、label、value 使用 JSON Pointer。资源 key 必须稳定且不含 `/`、`\\`
或 `.`；数组索引和显示名称不是稳定 key。最终 ID 为
`plugin.node.resource_key.field`。插件输出不能注入节点、控件或动作。

### 3.6 修改操作

```toml
[operations.set_autoconnect]
args = ["set", "${resource.uuid}", "${value}"]
timeout_ms = 3000
scope = "user"
guarantee = "compensatable"
validate = "validate_autoconnect"
precondition = "read_autoconnect"
compensation = "restore_autoconnect"
verify = "read_autoconnect"

[operations.validate_autoconnect]
args = ["validate", "${value}"]
expected_stdout = "ok"

[operations.read_autoconnect]
args = ["get", "${resource.uuid}"]
expected_stdout = "${old_value}"

[operations.restore_autoconnect]
args = ["set", "${resource.uuid}", "${old_value}"]
```

占位符只有 `${resource.NAME}`、`${value}`、`${old_value}`。所有可编辑 operation 必须
声明 precondition 和 verify；validate 可选并在暂存时运行，verify 在执行后运行。保证：

- `atomic`：外部接口原子完成；
- `compensatable`：必须同时有 compensation 和 verify；
- `best_effort`：可执行但不保证自动恢复；
- `irreversible`：字段强制只读。

`refresh` operation 若存在，用户按 `r` 时替代 snapshot；失败会保留上次成功快照，
标记 stale 并禁止编辑。

## 4. Transform 插件与 Adapter Lua Decoder

```toml
[sources.init_lua]
path = "${XDG_CONFIG_HOME}/nvim/init.lua"
format = "lua"
scope = "user"

[transform]
script = "transform.lua"
decode_entrypoint = "decode"
plan_entrypoint = "plan"

[fields.editor.tabstop]
source = "init_lua"
binding = "editor.tabstop"
type = "integer"
```

decode 返回纯数据 binding：

```lua
return {
  bindings = {
    ["editor.tabstop"] = {
      value = 4,
      source_id = "init_lua",
      range = { start = 102, ["end"] = 103 },
    },
  },
  diagnostics = {},
}
```

plan 输入含 binding、value、expected_sha256 和已声明 source 文本；输出：

```lua
return { edits = {{
  source_id = "init_lua",
  expected_sha256 = input.expected_sha256,
  start = 102,
  ["end"] = 103,
  replacement = "2",
}} }
```

范围是零起点、end-exclusive UTF-8 byte range。Core 拒绝错误 source、摘要、越界、
重叠、NUL 和过大 replacement。动态表达式可返回无 binding；条目会显示不可安全
编辑，用户仍可进入真实源文件的 Raw 视图。

Lua 只有 table/string/math/utf8，无 os/io/package/debug/require/FFI/网络/进程/文件。
内存、指令、数据深度、节点和结果均有限制。Adapter 可设置 decoder=`lua`，此时
decode 输入为 `{stdout, stderr="", status=0}`，输出仍必须是静态 Snapshot 数据。

## 5. 示例与检查

`plugins/examples` 包含：

- `kitty-schema`：include 图、origin 写回和显式 insert；
- `clock-adapter`：强制沙箱 Command 与 JSON snapshot；
- `dbus-adapter`：session broker GetId 与 single-record 解码；
- `socket-adapter`：same-UID line-framed JSON 状态模板；
- `neovim-transform`：静态 `vim.o.tabstop` binding 与范围计划。

```bash
cargo run -p reginux-tui --bin reginux -- --plugin-dir plugins/examples
./scripts/check.sh
```

发布插件前至少验证：未批准不执行、摘要变化失效、错误输出受清洗、刷新失败变 stale、
外部状态变化触发 precondition、文件变化触发摘要冲突、补偿/verify 的最终状态可解释。
