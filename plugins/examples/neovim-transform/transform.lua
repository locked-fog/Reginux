-- Reginux Lua Transform v1 example. The sandbox supplies data only; this
-- script has no os, io, package, debug, network, process or filesystem API.

local function find_tabstop(text)
  local _, assignment_end = text:find("vim%.o%.tabstop%s*=%s*")
  if assignment_end == nil then
    return nil
  end
  local value_start, value_end = text:find("%d+", assignment_end + 1)
  if value_start == nil then
    return nil
  end
  return value_start, value_end, tonumber(text:sub(value_start, value_end))
end

function decode(input)
  local text = input.sources.init_lua or ""
  local value_start, value_end, value = find_tabstop(text)
  if value_start == nil then
    return { bindings = {} }
  end
  return {
    bindings = {
      ["editor.tabstop"] = {
        value = value,
        source_id = "init_lua",
        -- Lua string positions are one-based and inclusive; Reginux ranges
        -- are zero-based and end-exclusive UTF-8 byte offsets.
        range = { start = value_start - 1, ["end"] = value_end },
      },
    },
  }
end

function plan(input)
  local text = input.sources.init_lua or ""
  local value_start, value_end = find_tabstop(text)
  if value_start == nil then
    return { edits = {} }
  end
  return {
    edits = {
      {
        source_id = "init_lua",
        expected_sha256 = input.expected_sha256,
        start = value_start - 1,
        ["end"] = value_end,
        replacement = tostring(input.value),
      },
    },
  }
end
