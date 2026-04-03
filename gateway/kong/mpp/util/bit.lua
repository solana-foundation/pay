local M = {}

local UINT32 = 4294967296

local function normalize(value)
  value = value % UINT32
  if value < 0 then
    value = value + UINT32
  end
  return value
end

local function bitop(a, b, op)
  a = normalize(a)
  b = normalize(b)
  local result = 0
  local bit = 1
  while a > 0 or b > 0 do
    local abit = a % 2
    local bbit = b % 2
    local out = op(abit, bbit)
    if out == 1 then
      result = result + bit
    end
    a = math.floor(a / 2)
    b = math.floor(b / 2)
    bit = bit * 2
  end
  return normalize(result)
end

function M.band(a, b)
  return bitop(a, b, function(x, y)
    if x == 1 and y == 1 then
      return 1
    end
    return 0
  end)
end

function M.bor(a, b)
  return bitop(a, b, function(x, y)
    if x == 1 or y == 1 then
      return 1
    end
    return 0
  end)
end

function M.bxor(a, b)
  return bitop(a, b, function(x, y)
    if x ~= y then
      return 1
    end
    return 0
  end)
end

function M.bnot(a)
  return normalize(UINT32 - 1 - normalize(a))
end

function M.rshift(value, amount)
  return math.floor(normalize(value) / (2 ^ amount))
end

function M.lshift(value, amount)
  return normalize(normalize(value) * (2 ^ amount))
end

function M.rrotate(value, amount)
  amount = amount % 32
  local right = M.rshift(value, amount)
  local left = M.lshift(value, 32 - amount)
  return normalize(M.bor(right, left))
end

function M.add(...)
  local sum = 0
  local values = { ... }
  for i = 1, #values do
    sum = normalize(sum + values[i])
  end
  return sum
end

return M
