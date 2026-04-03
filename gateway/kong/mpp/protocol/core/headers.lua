local challenge = require('mpp.protocol.core.challenge')
local types = require('mpp.protocol.core.types')
local json = require('mpp.util.json')

local M = {
  WWW_AUTHENTICATE_HEADER = 'www-authenticate',
  AUTHORIZATION_HEADER = 'authorization',
  PAYMENT_RECEIPT_HEADER = 'payment-receipt',
  PAYMENT_SCHEME = 'Payment',
}

local max_token_len = 16 * 1024

local function escape_quoted(value)
  value = tostring(value)
  value = value:gsub('\\', '\\\\'):gsub('"', '\\"'):gsub('\r', ''):gsub('\n', '')
  return value
end

local function strip_payment_scheme(header)
  local trimmed = header:gsub('^%s+', '')
  if trimmed:sub(1, #M.PAYMENT_SCHEME):lower() ~= string.lower(M.PAYMENT_SCHEME) then
    return nil
  end
  return trimmed:sub(#M.PAYMENT_SCHEME + 1):gsub('^%s+', '')
end

local function parse_auth_params(input)
  local params = {}
  local i = 1
  while i <= #input do
    while i <= #input and input:sub(i, i):match('[%s,]') do
      i = i + 1
    end
    if i > #input then
      break
    end
    local eq = input:find('=', i, true)
    if not eq then
      error('invalid auth parameter')
    end
    local key = input:sub(i, eq - 1):gsub('%s+$', '')
    i = eq + 1
    local value
    if input:sub(i, i) == '"' then
      i = i + 1
      local out = {}
      local escaped = false
      while i <= #input do
        local ch = input:sub(i, i)
        i = i + 1
        if escaped then
          out[#out + 1] = ch
          escaped = false
        elseif ch == '\\' then
          escaped = true
        elseif ch == '"' then
          break
        else
          out[#out + 1] = ch
        end
      end
      value = table.concat(out)
    else
      local next_comma = input:find(',', i, true)
      if next_comma then
        value = input:sub(i, next_comma - 1):gsub('%s+$', '')
        i = next_comma + 1
      else
        value = input:sub(i):gsub('%s+$', '')
        i = #input + 1
      end
    end
    if params[key] ~= nil then
      error('duplicate parameter: ' .. key)
    end
    params[key] = value
  end
  return params
end

function M.extract_payment_scheme(header)
  for part in header:gmatch('[^,]+') do
    local trimmed = part:gsub('^%s+', ''):gsub('%s+$', '')
    if trimmed:sub(1, #M.PAYMENT_SCHEME + 1):lower() == string.lower(M.PAYMENT_SCHEME) .. ' ' then
      return trimmed
    end
  end
  return nil
end

function M.parse_www_authenticate(header)
  local rest = strip_payment_scheme(header)
  if not rest then
    error('expected "Payment" scheme')
  end
  local params = parse_auth_params(rest)
  if not params.request or params.request == '' then
    error('missing "request" field')
  end
  local request_bytes, decode_err = types.base64url_decode(params.request)
  if not request_bytes then
    error('invalid request field: ' .. decode_err)
  end
  local ok = pcall(json.decode, request_bytes)
  if not ok then
    error('invalid JSON in request field')
  end
  local method = types.new_method_name(params.method or '')
  if not types.is_valid_method(method) then
    error('invalid method: ' .. tostring(params.method))
  end
  if not params.id or params.id == '' or not params.realm or params.realm == '' or not params.intent or params.intent == '' then
    error('missing required challenge fields')
  end
  return challenge.challenge_from_table({
    id = params.id,
    realm = params.realm,
    method = method,
    intent = types.new_intent_name(params.intent),
    request = params.request,
    expires = params.expires,
    description = params.description,
    digest = params.digest,
    opaque = params.opaque,
  })
end

function M.format_www_authenticate(value)
  local plain = challenge.challenge_to_plain(value)
  local parts = {
    'id="' .. escape_quoted(plain.id) .. '"',
    'realm="' .. escape_quoted(plain.realm) .. '"',
    'method="' .. escape_quoted(plain.method) .. '"',
    'intent="' .. escape_quoted(plain.intent) .. '"',
    'request="' .. escape_quoted(plain.request) .. '"',
  }
  if plain.expires and plain.expires ~= '' then
    parts[#parts + 1] = 'expires="' .. escape_quoted(plain.expires) .. '"'
  end
  if plain.description and plain.description ~= '' then
    parts[#parts + 1] = 'description="' .. escape_quoted(plain.description) .. '"'
  end
  if plain.digest and plain.digest ~= '' then
    parts[#parts + 1] = 'digest="' .. escape_quoted(plain.digest) .. '"'
  end
  if plain.opaque and plain.opaque ~= '' then
    parts[#parts + 1] = 'opaque="' .. escape_quoted(plain.opaque) .. '"'
  end
  return M.PAYMENT_SCHEME .. ' ' .. table.concat(parts, ', ')
end

function M.parse_authorization(header)
  local token = M.extract_payment_scheme(header)
  if not token then
    error('expected "Payment" scheme')
  end
  token = token:sub(#M.PAYMENT_SCHEME + 1):gsub('^%s+', '')
  if #token > max_token_len then
    error('token exceeds maximum length of ' .. max_token_len .. ' bytes')
  end
  local payload, decode_err = types.base64url_decode(token)
  if not payload then
    error(decode_err)
  end
  local ok, value = pcall(json.decode, payload)
  if not ok then
    error('invalid credential JSON: ' .. value)
  end
  value.challenge = challenge.challenge_from_table(value.challenge)
  return value
end

function M.format_authorization(value)
  local payload = json.encode(challenge.credential_to_plain(value))
  return M.PAYMENT_SCHEME .. ' ' .. types.base64url_encode(payload)
end

function M.parse_receipt(header)
  if #header > max_token_len then
    error('receipt exceeds maximum length of ' .. max_token_len .. ' bytes')
  end
  local payload, decode_err = types.base64url_decode((header:gsub('^%s+', ''):gsub('%s+$', '')))
  if not payload then
    error(decode_err)
  end
  local ok, value = pcall(json.decode, payload)
  if not ok then
    error('invalid receipt JSON: ' .. value)
  end
  return value
end

function M.format_receipt(value)
  return types.base64url_encode(json.encode(challenge.receipt_to_plain(value)))
end

return M
