local base64url = require('mpp.util.base64url')
local json = require('mpp.util.json')

local M = {}

local Base64URLJSON = {}
Base64URLJSON.__index = Base64URLJSON

function Base64URLJSON:raw()
  return self._raw
end

function Base64URLJSON:is_empty()
  return self._raw == nil or self._raw == ''
end

function Base64URLJSON:decode()
  local payload, err = base64url.decode(self._raw)
  if not payload then
    return nil, err
  end
  local ok, value = pcall(json.decode, payload)
  if not ok then
    return nil, value
  end
  return value
end

M.Base64URLJSON = Base64URLJSON

function M.new_method_name(name)
  return string.lower(name)
end

function M.is_valid_method(name)
  if type(name) ~= 'string' or name == '' then
    return false
  end
  return name:match('^[a-z]+$') ~= nil
end

function M.new_intent_name(name)
  return string.lower(name)
end

function M.is_charge_intent(name)
  return string.lower(name or '') == 'charge'
end

function M.new_base64url_json_raw(raw)
  return setmetatable({ _raw = raw }, Base64URLJSON)
end

function M.new_base64url_json_value(value)
  return setmetatable({ _raw = base64url.encode(json.encode(value)) }, Base64URLJSON)
end

function M.base64url_encode(value)
  return base64url.encode(value)
end

function M.base64url_decode(value)
  return base64url.decode(value)
end

M.RECEIPT_STATUS_SUCCESS = 'success'

return M
