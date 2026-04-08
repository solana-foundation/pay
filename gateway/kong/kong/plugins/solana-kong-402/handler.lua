local mpp = require "mpp"
local json = require "mpp.util.json"

local plugin_state = {
  servers = {},
}

local plugin = {
  PRIORITY = 1000,
  VERSION = "0.1.0",
}

local upstream_receipt_header = "X-MPP-Receipt"
local upstream_status_header = "X-MPP-Receipt-Status"
local upstream_reference_header = "X-MPP-Receipt-Reference"
local upstream_challenge_id_header = "X-MPP-Challenge-ID"
local upstream_external_id_header = "X-MPP-External-ID"
local plugin_name = "solana-kong-402"

local function trim(value)
  return (tostring(value or ""):gsub("^%s+", ""):gsub("%s+$", ""))
end

local function denull(value)
  if value == json.null then
    return nil
  end
  if type(value) ~= "table" then
    return value
  end
  local out = {}
  for key, item in pairs(value) do
    out[key] = denull(item)
  end
  return out
end

local function require_http()
  local ok, http = pcall(require, "resty.http")
  if not ok then
    error('resty.http is required for native payment verification')
  end
  return http
end

local function rpc_request(rpc_url, method, params)
  local http = require_http()
  local client = http.new()
  client:set_timeout(5000)

  local response, err = client:request_uri(rpc_url, {
    method = "POST",
    body = json.encode({
      jsonrpc = "2.0",
      id = 1,
      method = method,
      params = params,
    }),
    headers = {
      ["content-type"] = "application/json",
    },
    ssl_verify = false,
  })
  if not response then
    error("rpc request failed: " .. tostring(err))
  end
  if response.status < 200 or response.status >= 300 then
    error("rpc request failed with status " .. tostring(response.status))
  end

  local ok, payload = pcall(json.decode, response.body)
  if not ok then
    error("rpc response was not valid JSON")
  end
  payload = denull(payload)
  if payload.error then
    local message = payload.error.message or json.encode(payload.error)
    error("rpc error: " .. tostring(message))
  end
  return payload.result
end

local function fetch_transaction(rpc_url, signature)
  return rpc_request(rpc_url, "getTransaction", {
    signature,
    {
      encoding = "jsonParsed",
      commitment = "confirmed",
      maxSupportedTransactionVersion = 0,
    },
  })
end

local function send_transaction(rpc_url, transaction)
  return rpc_request(rpc_url, "sendTransaction", {
    transaction,
    {
      encoding = "base64",
      preflightCommitment = "confirmed",
    },
  })
end

local function await_transaction(rpc_url, signature)
  for _ = 1, 30 do
    local tx = fetch_transaction(rpc_url, signature)
    if tx then
      return tx
    end
    if ngx and ngx.sleep then
      ngx.sleep(0.2)
    end
  end
  return nil
end

local function fetch_token_account(rpc_url, address)
  local result = rpc_request(rpc_url, "getAccountInfo", {
    address,
    {
      encoding = "jsonParsed",
      commitment = "confirmed",
    },
  })
  local value = result and result.value or nil
  local parsed = value
    and value.data
    and value.data.parsed
    and value.data.parsed.info
    or nil
  if not parsed then
    return nil
  end
  return {
    owner = parsed.owner,
    mint = parsed.mint,
  }
end

local function verifier_hooks(config)
  local rpc_url = trim(config.rpc_url)
  if rpc_url == "" then
    error("rpc_url is required for native payment verification")
  end
  return {
    fetch_transaction = function(signature)
      return fetch_transaction(rpc_url, signature)
    end,
    send_transaction = function(transaction)
      return send_transaction(rpc_url, transaction)
    end,
    await_transaction = function(signature)
      return await_transaction(rpc_url, signature)
    end,
    fetch_token_account = function(address)
      return fetch_token_account(rpc_url, address)
    end,
  }
end

local function server_cache_key(config)
  return json.encode({
    recipient = config.recipient,
    currency = config.currency,
    decimals = config.decimals,
    network = config.network,
    rpc_url = config.rpc_url,
    secret_key = config.secret_key,
    realm = config.realm,
    fee_payer = config.fee_payer,
  })
end

local function path_matches(pattern, path)
  local pattern_parts = {}
  local path_parts = {}

  for part in string.gmatch((pattern:gsub("^/+", ""):gsub("/+$", "")), "[^/]+") do
    pattern_parts[#pattern_parts + 1] = part
  end
  for part in string.gmatch((path:gsub("^/+", ""):gsub("/+$", "")), "[^/]+") do
    path_parts[#path_parts + 1] = part
  end
  if #pattern_parts ~= #path_parts then
    return false
  end
  for i = 1, #pattern_parts do
    local pat = pattern_parts[i]
    local actual = path_parts[i]
    local matches = false

    if pat:match("^%b{}$") then
      matches = true
    elseif pat:find("{", 1, true) then
      local close = pat:find("}", 1, true)
      if not close then
        return false
      end
      local suffix = pat:sub(close + 1)
      matches = suffix == "" or actual:sub(-#suffix) == suffix
    else
      matches = pat == actual
    end

    if not matches then
      return false
    end
  end
  return true
end

local function find_endpoint(endpoints, method, path)
  for _, endpoint in ipairs(endpoints or {}) do
    if string.upper(endpoint.method) == method and endpoint.path == path then
      return endpoint
    end
  end
  for _, endpoint in ipairs(endpoints or {}) do
    if string.upper(endpoint.method) == method and path_matches(endpoint.path, path) then
      return endpoint
    end
  end
  return nil
end

local function extract_variant_hint(path)
  local trimmed = path:gsub("^/+", ""):gsub("/+$", "")
  for segment in string.gmatch(trimmed, "[^/]+") do
    if segment:find(":", 1, true) then
      return segment
    end
  end
  return trimmed
end

local function request_properties()
  local header = kong.request.get_header("content-length")
  local size = tonumber(header)
  if not size then
    return {}
  end
  return { body_size = size }
end

local function evaluate_condition(condition, props)
  if not condition then
    return true
  end
  local actual = props[condition.field]
  if actual == nil then
    return true
  end
  if condition.op == "<=" then
    return actual <= condition.value
  elseif condition.op == "<" then
    return actual < condition.value
  elseif condition.op == ">=" then
    return actual >= condition.value
  elseif condition.op == ">" then
    return actual > condition.value
  elseif condition.op == "==" then
    return actual == condition.value
  end
  return true
end

local function resolve_tier_price(tiers, props)
  if not tiers or #tiers == 0 then
    return 0
  end
  for _, tier in ipairs(tiers) do
    if evaluate_condition(tier.condition, props) and tier.price_usd and tier.price_usd > 0 then
      return tier.price_usd
    end
  end
  return tiers[#tiers].price_usd
end

local function resolve_dimensions(dimensions, props)
  if not dimensions or #dimensions == 0 then
    return nil
  end
  local resolved = { dimensions = {} }
  for _, dimension in ipairs(dimensions) do
    resolved.dimensions[#resolved.dimensions + 1] = {
      direction = string.lower(dimension.direction),
      unit = string.lower(dimension.unit),
      scale = dimension.scale or 1,
      price_usd = resolve_tier_price(dimension.tiers, props),
    }
  end
  return resolved
end

local function resolve_price(metering, props, variant_hint)
  if not metering then
    return nil
  end
  if metering.variants and #metering.variants > 0 then
    for _, variant in ipairs(metering.variants) do
      if variant_hint ~= "" and string.find(variant_hint, variant.value, 1, true) then
        return resolve_dimensions(variant.dimensions, props)
      end
    end
    return resolve_dimensions(metering.variants[1].dimensions, props)
  end
  return resolve_dimensions(metering.dimensions, props)
end

local function amount_from_price(price, decimals)
  if not price or not price.dimensions or #price.dimensions == 0 then
    return nil, "resolved price is empty"
  end
  local precision = tonumber(decimals) or 6
  local amount = string.format("%." .. tostring(precision) .. "f", price.dimensions[1].price_usd)
  amount = amount:gsub("0+$", ""):gsub("%.$", "")
  if amount == "" then
    amount = "0"
  end
  return amount
end

local function atomic_amount_from_display(amount, decimals)
  local precision = tonumber(decimals) or 0
  local numeric = tonumber(amount)
  if not numeric then
    return amount
  end
  local scaled = math.floor((numeric * (10 ^ precision)) + 0.5)
  return tostring(scaled)
end

local function exit_json(status, body, headers)
  for name, value in pairs(headers or {}) do
    kong.response.set_header(name, value)
  end
  kong.response.set_header("content-type", "application/json")
  return kong.response.exit(status, body)
end

local function build_server(config)
  local cache_key = server_cache_key(config)
  if plugin_state.servers[cache_key] then
    return plugin_state.servers[cache_key]
  end

  local server = mpp.server.new({
    recipient = config.recipient,
    currency = config.currency,
    decimals = config.decimals,
    network = config.network,
    rpc_url = config.rpc_url,
    secret_key = config.secret_key,
    realm = config.realm,
    fee_payer = config.fee_payer,
    verifier_hooks = verifier_hooks(config),
  })
  plugin_state.servers[cache_key] = server
  return server
end

local function apply_receipt(config, receipt)
  local header = mpp.FormatReceipt(receipt)
  kong.ctx.shared.solana_mpp_receipt = header

  if not config.inject_upstream_headers then
    return
  end

  kong.service.request.set_header(upstream_receipt_header, header)
  kong.service.request.set_header(upstream_status_header, tostring(receipt.status or ""))
  if receipt.reference and receipt.reference ~= "" then
    kong.service.request.set_header(upstream_reference_header, receipt.reference)
  end
  if receipt.challengeId and receipt.challengeId ~= "" then
    kong.service.request.set_header(upstream_challenge_id_header, receipt.challengeId)
  end
  if receipt.externalId and receipt.externalId ~= "" then
    kong.service.request.set_header(upstream_external_id_header, receipt.externalId)
  end
end

local function safe_call(fn)
  local ok, value = pcall(fn)
  if not ok then
    return nil
  end
  return value
end

local function get_route_id()
  if not kong.router or not kong.router.get_route then
    return nil
  end
  local route = safe_call(kong.router.get_route)
  return route and route.id or nil
end

local function get_service_id()
  if not kong.router or not kong.router.get_service then
    return nil
  end
  local service = safe_call(kong.router.get_service)
  return service and service.id or nil
end

local function get_request_id()
  if not kong.request or not kong.request.get_id then
    return nil
  end
  return safe_call(kong.request.get_id)
end

local function build_observability_event(config, endpoint, path)
  local price = resolve_price(endpoint.metering, request_properties(), extract_variant_hint(path))
  local amount_display = amount_from_price(price, config.decimals) or ""
  return {
    plugin = plugin_name,
    payment_network = config.network,
    payment_currency = config.currency,
    payment_decimals = config.decimals,
    payment_amount_atomic = atomic_amount_from_display(amount_display, config.decimals),
    payment_amount_display = amount_display,
    recipient = config.recipient,
    endpoint_method = endpoint.method,
    endpoint_path = endpoint.path,
    route_id = get_route_id(),
    service_id = get_service_id(),
    request_id = get_request_id(),
  }
end

local function emit_event(config, event)
  local observability = config.observability or {}
  if observability.mode ~= "log" then
    return
  end
  if not kong.log or not kong.log.notice then
    return
  end
  kong.log.notice(json.encode(event))
end

local function emit_payment_verified(config, endpoint, receipt, path)
  local event = build_observability_event(config, endpoint, path)
  event.event_name = "solana_pay.payment_verified"
  event.challenge_id = receipt.challengeId
  event.external_id = receipt.externalId
  event.receipt_reference = receipt.reference
  event.receipt_status = receipt.status
  event.payment_method = receipt.method

  emit_event(config, event)
end

local function emit_payment_failed(config, endpoint, path, reason, details)
  local event = build_observability_event(config, endpoint, path)
  event.event_name = "solana_pay.payment_failed"
  event.failure_reason = reason
  event.failure_details = details
  event.receipt_status = "failed"

  emit_event(config, event)
end

local function emit_payment_requested(config, endpoint, challenge, path)
  local event = build_observability_event(config, endpoint, path)
  event.event_name = "solana_pay.payment_requested"
  event.challenge_id = challenge.id
  event.external_id = challenge.externalId
  event.payment_method = challenge.method
  event.receipt_status = "pending"

  emit_event(config, event)
end

local function payment_required(config, endpoint, method, path, reason, details)
  local price = resolve_price(endpoint.metering, request_properties(), extract_variant_hint(path))
  local amount, err = amount_from_price(price, config.decimals)
  if not amount then
    return exit_json(500, {
      error = "pricing_resolution_failed",
      message = err,
    })
  end

  local description = endpoint.description
  if not description or description == "" then
    description = config.description
  end

  local ok, server_or_err = pcall(build_server, config)
  if not ok then
    return exit_json(500, {
      error = "plugin_misconfigured",
      message = server_or_err,
    })
  end

  local challenge = server_or_err:charge_with_options(amount, {
    description = description,
    external_id = config.external_id,
    fee_payer = config.fee_payer,
  })
  emit_payment_requested(config, endpoint, challenge, path)
  local header = mpp.FormatWWWAuthenticate(challenge)

  local body = {
    error = "payment_required",
    message = "payment required",
    endpoint = {
      method = method,
      path = endpoint.path,
    },
    pricing = price,
    implementation = "lua-native",
  }
  if reason and reason ~= "" then
    body.reason = reason
  end
  if details and details ~= "" then
    body.details = details
  end

  return exit_json(402, body, {
    [mpp.WWWAuthenticateHeader] = header,
  })
end

local function verify_authorization(config, authorization)
  local ok, credential_or_err = pcall(mpp.ParseAuthorization, authorization)
  if not ok then
    return nil, "invalid_authorization", credential_or_err
  end

  local ok_server, server_or_err = pcall(build_server, config)
  if not ok_server then
    return nil, "plugin_misconfigured", server_or_err
  end

  local ok_verify, receipt_or_err = pcall(function()
    return server_or_err:verify_credential(credential_or_err)
  end)
  if not ok_verify then
    return nil, "payment_verification_failed", receipt_or_err
  end
  return receipt_or_err
end

function plugin:access(config)
  local method = kong.request.get_method()
  local path = kong.request.get_path()
  local endpoint = find_endpoint(config.endpoints, method, path)
  if not endpoint or not endpoint.metering then
    return
  end

  local auth_header = trim(kong.request.get_header(mpp.AuthorizationHeader))
  if auth_header == "" then
    return payment_required(config, endpoint, method, path)
  end

  local receipt, reason, details = verify_authorization(config, auth_header)
  if not receipt then
    if reason == "plugin_misconfigured" then
      return exit_json(500, {
        error = reason,
        message = details,
      })
    end
    emit_payment_failed(config, endpoint, path, reason, details)
    return payment_required(config, endpoint, method, path, reason, details)
  end

  apply_receipt(config, receipt)
  emit_payment_verified(config, endpoint, receipt, path)
end

function plugin:response()
  local receipt = kong.ctx.shared.solana_mpp_receipt
  if receipt and receipt ~= "" then
    kong.response.set_header(mpp.PaymentReceiptHeader, receipt)
  end
end

return plugin
