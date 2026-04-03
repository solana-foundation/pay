local M = {}

function M.new(code, message, details)
  return {
    code = code,
    message = message,
    details = details,
  }
end

function M.raise(code, message, details)
  error(M.new(code, message, details))
end

return M
