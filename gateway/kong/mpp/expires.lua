local M = {}

local function days_from_civil(year, month, day)
  year = year - (month <= 2 and 1 or 0)
  local era = math.floor((year >= 0 and year or year - 399) / 400)
  local yoe = year - era * 400
  local mp = month + (month > 2 and -3 or 9)
  local doy = math.floor((153 * mp + 2) / 5) + day - 1
  local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
  return era * 146097 + doe - 719468
end

function M.parse_rfc3339(value)
  local year, month, day, hour, min, sec = value:match('^(%d%d%d%d)%-(%d%d)%-(%d%d)T(%d%d):(%d%d):(%d%d)Z$')
  if not year then
    return nil, 'invalid RFC3339 timestamp'
  end
  year = tonumber(year)
  month = tonumber(month)
  day = tonumber(day)
  hour = tonumber(hour)
  min = tonumber(min)
  sec = tonumber(sec)

  if month < 1 or month > 12 or day < 1 or day > 31 or hour > 23 or min > 59 or sec > 60 then
    return nil, 'invalid RFC3339 timestamp'
  end

  local days = days_from_civil(year, month, day)
  return ((days * 24 + hour) * 60 + min) * 60 + sec
end

function M.is_expired(value, now_epoch)
  if value == nil or value == '' then
    return false
  end
  local expires_at = M.parse_rfc3339(value)
  if not expires_at then
    return true
  end
  return expires_at <= now_epoch
end

return M
