local M = {}

function M.parse_units(amount, decimals)
  amount = tostring(amount or ''):gsub('^%s+', ''):gsub('%s+$', '')
  if amount == '' then
    error('amount is required')
  end
  if amount:sub(1, 1) == '-' then
    error('amount cannot be negative')
  end
  local whole, fractional = amount:match('^(%d+)%.(%d+)$')
  if not whole then
    whole = amount:match('^(%d+)$')
    fractional = ''
  end
  if not whole then
    error('invalid amount: ' .. amount)
  end
  if #fractional > decimals then
    error('amount ' .. amount .. ' has too many decimal places for ' .. decimals .. ' decimals')
  end
  local value = whole .. fractional .. string.rep('0', decimals - #fractional)
  value = value:gsub('^0+', '')
  if value == '' then
    return '0'
  end
  return value
end

function M.parse_amount(request)
  local amount = tostring(request.amount or '')
  if not amount:match('^%d+$') then
    error('invalid amount: ' .. amount)
  end
  return amount
end

function M.validate_max_amount(request, max_amount)
  local actual = tonumber(M.parse_amount(request))
  local max_value = tonumber(max_amount)
  if max_value == nil then
    error('invalid max amount: ' .. tostring(max_amount))
  end
  if actual > max_value then
    error('amount ' .. tostring(actual) .. ' exceeds maximum ' .. tostring(max_value))
  end
end

return M
