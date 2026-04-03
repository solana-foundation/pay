local json = require('mpp.util.json')

local MemoryStore = {}
MemoryStore.__index = MemoryStore

function MemoryStore:new()
  return setmetatable({ data = {} }, self)
end

function MemoryStore:get(key)
  local value = self.data[key]
  if value == nil then
    return nil, false
  end
  return json.decode(value), true
end

function MemoryStore:put(key, value)
  self.data[key] = json.encode(value)
end

function MemoryStore:delete(key)
  self.data[key] = nil
end

function MemoryStore:put_if_absent(key, value)
  if self.data[key] ~= nil then
    return false
  end
  self.data[key] = json.encode(value)
  return true
end

local M = {}

function M.memory()
  return MemoryStore:new()
end

M.MemoryStore = MemoryStore

return M
