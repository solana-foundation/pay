local M = {}

local function normalize(value)
  local text = tostring(value or "")
  if not text:match("^%d+$") then
    error("invalid unsigned integer: " .. text)
  end
  text = text:gsub("^0+", "")
  if text == "" then
    return "0"
  end
  return text
end

function M.normalize(value)
  return normalize(value)
end

function M.compare(left, right)
  left = normalize(left)
  right = normalize(right)
  if #left < #right then
    return -1
  elseif #left > #right then
    return 1
  end
  if left < right then
    return -1
  elseif left > right then
    return 1
  end
  return 0
end

function M.add(left, right)
  left = normalize(left)
  right = normalize(right)
  local i = #left
  local j = #right
  local carry = 0
  local out = {}

  while i > 0 or j > 0 or carry > 0 do
    local a = i > 0 and tonumber(left:sub(i, i)) or 0
    local b = j > 0 and tonumber(right:sub(j, j)) or 0
    local sum = a + b + carry
    out[#out + 1] = tostring(sum % 10)
    carry = math.floor(sum / 10)
    i = i - 1
    j = j - 1
  end

  local chars = {}
  for idx = #out, 1, -1 do
    chars[#chars + 1] = out[idx]
  end
  return table.concat(chars)
end

function M.sub(left, right)
  left = normalize(left)
  right = normalize(right)
  if M.compare(left, right) < 0 then
    error("unsigned subtraction underflow")
  end
  local i = #left
  local j = #right
  local borrow = 0
  local out = {}

  while i > 0 do
    local a = tonumber(left:sub(i, i)) - borrow
    local b = j > 0 and tonumber(right:sub(j, j)) or 0
    if a < b then
      a = a + 10
      borrow = 1
    else
      borrow = 0
    end
    out[#out + 1] = tostring(a - b)
    i = i - 1
    j = j - 1
  end

  local chars = {}
  for idx = #out, 1, -1 do
    chars[#chars + 1] = out[idx]
  end
  local value = table.concat(chars):gsub("^0+", "")
  if value == "" then
    return "0"
  end
  return value
end

return M
