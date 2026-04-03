local M = {}

local alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_'
local decode_map = {}

for i = 1, #alphabet do
  decode_map[alphabet:sub(i, i)] = i - 1
end

function M.encode(input)
  local out = {}
  local i = 1
  while i <= #input do
    local a = input:byte(i) or 0
    local b = input:byte(i + 1)
    local c = input:byte(i + 2)
    local triple = a * 65536 + (b or 0) * 256 + (c or 0)

    out[#out + 1] = alphabet:sub(math.floor(triple / 262144) + 1, math.floor(triple / 262144) + 1)
    out[#out + 1] = alphabet:sub((math.floor(triple / 4096) % 64) + 1, (math.floor(triple / 4096) % 64) + 1)
    if b ~= nil then
      out[#out + 1] = alphabet:sub((math.floor(triple / 64) % 64) + 1, (math.floor(triple / 64) % 64) + 1)
    end
    if c ~= nil then
      out[#out + 1] = alphabet:sub((triple % 64) + 1, (triple % 64) + 1)
    end

    i = i + 3
  end
  return table.concat(out)
end

function M.decode(input)
  local normalized = input:gsub('%+', '-'):gsub('/', '_'):gsub('=', '')
  local out = {}
  local i = 1

  while i <= #normalized do
    local c1 = decode_map[normalized:sub(i, i)]
    local c2 = decode_map[normalized:sub(i + 1, i + 1)]
    local c3 = decode_map[normalized:sub(i + 2, i + 2)]
    local c4 = decode_map[normalized:sub(i + 3, i + 3)]

    if c1 == nil or c2 == nil then
      return nil, 'invalid base64url input'
    end

    local triple = c1 * 262144 + c2 * 4096 + (c3 or 0) * 64 + (c4 or 0)
    out[#out + 1] = string.char(math.floor(triple / 65536) % 256)
    if c3 ~= nil then
      out[#out + 1] = string.char(math.floor(triple / 256) % 256)
    end
    if c4 ~= nil then
      out[#out + 1] = string.char(triple % 256)
    end

    i = i + 4
  end

  return table.concat(out)
end

return M
