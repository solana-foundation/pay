local M = {}

local null_sentinel = {}
M.null = null_sentinel

local function is_array(value)
  if type(value) ~= 'table' then
    return false
  end
  local max = 0
  local count = 0
  for k, _ in pairs(value) do
    if type(k) ~= 'number' or k < 1 or k % 1 ~= 0 then
      return false
    end
    if k > max then
      max = k
    end
    count = count + 1
  end
  return max == count
end

local function encode_string(value)
  local replacements = {
    ['\\'] = '\\\\',
    ['"'] = '\\"',
    ['\b'] = '\\b',
    ['\f'] = '\\f',
    ['\n'] = '\\n',
    ['\r'] = '\\r',
    ['\t'] = '\\t',
  }
  return '"' .. value:gsub('[%z\1-\31\\"]', function(ch)
    return replacements[ch] or string.format('\\u%04x', ch:byte())
  end) .. '"'
end

local function encode_value(value)
  local kind = type(value)
  if value == null_sentinel then
    return 'null'
  elseif kind == 'nil' then
    return 'null'
  elseif kind == 'boolean' then
    return value and 'true' or 'false'
  elseif kind == 'number' then
    if value ~= value or value == math.huge or value == -math.huge then
      error('cannot encode non-finite number')
    end
    local formatted = string.format('%.14g', value)
    return formatted
  elseif kind == 'string' then
    return encode_string(value)
  elseif kind == 'table' then
    if is_array(value) then
      local parts = {}
      for i = 1, #value do
        parts[#parts + 1] = encode_value(value[i])
      end
      return '[' .. table.concat(parts, ',') .. ']'
    end
    local keys = {}
    for key, _ in pairs(value) do
      keys[#keys + 1] = key
    end
    table.sort(keys)
    local parts = {}
    for i = 1, #keys do
      local key = keys[i]
      local encoded = encode_value(value[key])
      if encoded ~= nil then
        parts[#parts + 1] = encode_string(key) .. ':' .. encoded
      end
    end
    return '{' .. table.concat(parts, ',') .. '}'
  end
  error('unsupported JSON type: ' .. kind)
end

function M.encode(value)
  return encode_value(value)
end

local Parser = {}
Parser.__index = Parser

function Parser:new(input)
  return setmetatable({ input = input, pos = 1, len = #input }, self)
end

function Parser:peek()
  return self.input:sub(self.pos, self.pos)
end

function Parser:next()
  local ch = self:peek()
  self.pos = self.pos + 1
  return ch
end

function Parser:skip_ws()
  while self.pos <= self.len do
    local ch = self:peek()
    if ch == ' ' or ch == '\n' or ch == '\r' or ch == '\t' then
      self.pos = self.pos + 1
    else
      break
    end
  end
end

function Parser:expect(text)
  if self.input:sub(self.pos, self.pos + #text - 1) ~= text then
    error('expected ' .. text .. ' at position ' .. self.pos)
  end
  self.pos = self.pos + #text
end

function Parser:parse_string()
  self:expect('"')
  local out = {}
  while self.pos <= self.len do
    local ch = self:next()
    if ch == '"' then
      return table.concat(out)
    elseif ch == '\\' then
      local esc = self:next()
      if esc == '"' or esc == '\\' or esc == '/' then
        out[#out + 1] = esc
      elseif esc == 'b' then
        out[#out + 1] = '\b'
      elseif esc == 'f' then
        out[#out + 1] = '\f'
      elseif esc == 'n' then
        out[#out + 1] = '\n'
      elseif esc == 'r' then
        out[#out + 1] = '\r'
      elseif esc == 't' then
        out[#out + 1] = '\t'
      elseif esc == 'u' then
        local hex = self.input:sub(self.pos, self.pos + 3)
        if #hex ~= 4 or not hex:match('^[0-9a-fA-F]+$') then
          error('invalid unicode escape at position ' .. self.pos)
        end
        self.pos = self.pos + 4
        local code = tonumber(hex, 16)
        if code < 128 then
          out[#out + 1] = string.char(code)
        elseif code < 2048 then
          out[#out + 1] = string.char(192 + math.floor(code / 64), 128 + (code % 64))
        else
          out[#out + 1] = string.char(
            224 + math.floor(code / 4096),
            128 + (math.floor(code / 64) % 64),
            128 + (code % 64)
          )
        end
      else
        error('invalid escape character at position ' .. self.pos)
      end
    else
      out[#out + 1] = ch
    end
  end
  error('unterminated string')
end

function Parser:parse_number()
  local start = self.pos
  local allowed = '[0-9+%-eE%.]'
  while self.pos <= self.len and self:peek():match(allowed) do
    self.pos = self.pos + 1
  end
  local text = self.input:sub(start, self.pos - 1)
  local value = tonumber(text)
  if value == nil then
    error('invalid number at position ' .. start)
  end
  return value
end

function Parser:parse_array()
  self:expect('[')
  self:skip_ws()
  local out = {}
  if self:peek() == ']' then
    self.pos = self.pos + 1
    return out
  end
  while true do
    out[#out + 1] = self:parse_value()
    self:skip_ws()
    local ch = self:next()
    if ch == ']' then
      return out
    elseif ch ~= ',' then
      error('expected , or ] at position ' .. self.pos)
    end
    self:skip_ws()
  end
end

function Parser:parse_object()
  self:expect('{')
  self:skip_ws()
  local out = {}
  if self:peek() == '}' then
    self.pos = self.pos + 1
    return out
  end
  while true do
    local key = self:parse_string()
    self:skip_ws()
    self:expect(':')
    self:skip_ws()
    out[key] = self:parse_value()
    self:skip_ws()
    local ch = self:next()
    if ch == '}' then
      return out
    elseif ch ~= ',' then
      error('expected , or } at position ' .. self.pos)
    end
    self:skip_ws()
  end
end

function Parser:parse_value()
  self:skip_ws()
  local ch = self:peek()
  if ch == '"' then
    return self:parse_string()
  elseif ch == '{' then
    return self:parse_object()
  elseif ch == '[' then
    return self:parse_array()
  elseif ch == '-' or ch:match('%d') then
    return self:parse_number()
  elseif ch == 't' then
    self:expect('true')
    return true
  elseif ch == 'f' then
    self:expect('false')
    return false
  elseif ch == 'n' then
    self:expect('null')
    return null_sentinel
  end
  error('unexpected token at position ' .. self.pos)
end

function M.decode(input)
  local parser = Parser:new(input)
  local value = parser:parse_value()
  parser:skip_ws()
  if parser.pos <= parser.len then
    error('unexpected trailing input at position ' .. parser.pos)
  end
  return value
end

return M
