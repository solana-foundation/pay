local script_dir = debug.getinfo(1, "S").source:sub(2):match("^(.*)/")
local repo_root = script_dir and script_dir:match("^(.*)/kong/plugins/solana%-kong%-402$")
if not repo_root then
  error("failed to resolve kong plugin test root")
end

package.path = "./?.lua;./?/init.lua;" .. repo_root .. "/?.lua;" .. repo_root .. "/?/init.lua;" .. package.path

local mpp = require("mpp")

local rpc_calls = {}

package.preload["resty.http"] = function()
  return {
    new = function()
      return {
        set_timeout = function() end,
        request_uri = function(_, _, opts)
          local body = require("mpp.util.json").decode(opts.body)
          rpc_calls[#rpc_calls + 1] = body.method
          if body.method == "sendTransaction" then
            return {
              status = 200,
              body = [[{"jsonrpc":"2.0","id":1,"result":"sig-123"}]],
            }
          end
          if body.method == "getTransaction" then
            return {
              status = 200,
              body = [[{"jsonrpc":"2.0","id":1,"result":{"meta":{"err":null},"transaction":{"message":{"instructions":[{"program":"system","parsed":{"type":"transfer","info":{"destination":"recipient-1","lamports":"1000"}}}]}}}}]],
            }
          end
          if body.method == "getAccountInfo" then
            return {
              status = 200,
              body = [[{"jsonrpc":"2.0","id":1,"result":{"value":{"data":{"parsed":{"info":{"owner":"recipient-1","mint":"mint-1"}}}}}}]],
            }
          end
          return {
            status = 500,
            body = [[{"jsonrpc":"2.0","id":1,"error":{"message":"unexpected method"}}]],
          }
        end,
      }
    end,
  }
end

local state = {}

local function reset_state()
  state = {
    method = "GET",
    path = "/paid",
    headers = {},
    response_headers = {},
    service_headers = {},
    shared = {},
    logs = {},
    exit = nil,
  }
end

local function install_kong()
  _G.kong = {
    request = {
      get_method = function()
        return state.method
      end,
      get_path = function()
        return state.path
      end,
      get_header = function(name)
        return state.headers[string.lower(name)]
      end,
    },
    response = {
      set_header = function(name, value)
        state.response_headers[string.lower(name)] = value
      end,
      exit = function(status, body)
        state.exit = {
          status = status,
          body = body,
          headers = state.response_headers,
        }
        return state.exit
      end,
    },
    service = {
      request = {
        set_header = function(name, value)
          state.service_headers[name] = value
        end,
      },
    },
    router = {
      get_route = function()
        return { id = "route-123" }
      end,
      get_service = function()
        return { id = "service-123" }
      end,
    },
    log = {
      notice = function(message)
        state.logs[#state.logs + 1] = message
      end,
    },
    ctx = {
      shared = state.shared,
    },
  }
  _G.kong.request.get_id = function()
    return "request-123"
  end
end

local function assert_true(value, message)
  if not value then
    error(message or "assertion failed")
  end
end

local function assert_equal(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected) .. ", got " .. tostring(actual))
  end
end

local config = {
  recipient = "recipient-1",
  currency = "sol",
  decimals = 9,
  network = "localnet",
  rpc_url = "http://rpc.local",
  secret_key = "test-secret",
  inject_upstream_headers = true,
  observability = {
    mode = "log",
  },
  endpoints = {
    {
      method = "GET",
      path = "/paid",
      description = "Protected endpoint",
      metering = {
        dimensions = {
          {
            direction = "usage",
            unit = "requests",
            scale = 1,
            tiers = {
              { price_usd = 0.000001 },
            },
          },
        },
      },
    },
  },
}

reset_state()
install_kong()

local plugin = dofile(repo_root .. "/kong/plugins/solana-kong-402/handler.lua")

local challenge_exit = plugin:access(config)
assert_true(challenge_exit ~= nil, "expected unauthenticated request to exit")
assert_equal(state.exit.status, 402, "expected payment challenge")
assert_true(state.response_headers["www-authenticate"] ~= nil, "expected WWW-Authenticate header")
assert_equal(#state.logs, 1, "expected one challenge observability log")

local challenge = mpp.ParseWWWAuthenticate(state.response_headers["www-authenticate"])
local challenge_event = require("mpp.util.json").decode(state.logs[1])
assert_equal(challenge_event.event_name, "solana_pay.payment_requested", "expected payment requested event")
assert_equal(challenge_event.challenge_id, challenge.id, "expected challenge id in challenge event")
assert_equal(challenge_event.receipt_status, "pending", "expected pending status in challenge event")
assert_equal(challenge_event.payment_amount_atomic, "1000", "expected atomic amount in challenge event")

local authorization = mpp.FormatAuthorization(mpp.NewPaymentCredential(challenge:to_echo(), {
  type = "transaction",
  transaction = "base64-tx",
}))

reset_state()
install_kong()
state.headers["authorization"] = authorization

local result = plugin:access(config)
assert_equal(result, nil, "expected authorized request to continue")
assert_true(state.service_headers["X-MPP-Receipt"] ~= nil, "expected upstream receipt header")
assert_true(state.shared.solana_mpp_receipt ~= nil, "expected shared receipt context")
assert_true(#rpc_calls >= 1, "expected RPC verification to run")
assert_equal(rpc_calls[1], "sendTransaction", "expected transaction broadcast first")
assert_equal(#state.logs, 1, "expected one observability log")

local log_event = require("mpp.util.json").decode(state.logs[1])
assert_equal(log_event.event_name, "solana_pay.payment_verified", "expected payment verified event")
assert_equal(log_event.plugin, "solana-kong-402", "expected plugin name in event")
assert_equal(log_event.challenge_id, challenge.id, "expected challenge id in event")
assert_equal(log_event.external_id, nil, "expected absent external id in event")
assert_equal(log_event.route_id, "route-123", "expected route id in event")
assert_equal(log_event.service_id, "service-123", "expected service id in event")
assert_equal(log_event.request_id, "request-123", "expected request id in event")
assert_equal(log_event.payment_currency, "sol", "expected payment currency in event")
assert_equal(log_event.payment_amount_atomic, "1000", "expected atomic amount in event")
assert_equal(log_event.payment_amount_display, "0.000001", "expected display amount in event")

plugin:response()
assert_true(state.response_headers["payment-receipt"] ~= nil, "expected downstream payment receipt header")

reset_state()
install_kong()
state.headers["authorization"] = "Payment invalid"

local failed_challenge = plugin:access(config)
assert_true(failed_challenge ~= nil, "expected invalid authorization to be challenged")
assert_equal(state.exit.status, 402, "expected failed authorization to be re-challenged")
assert_equal(#state.logs, 2, "expected failure and challenge observability logs")

local failed_event = require("mpp.util.json").decode(state.logs[1])
assert_equal(failed_event.event_name, "solana_pay.payment_failed", "expected payment failed event")
assert_equal(failed_event.failure_reason, "invalid_authorization", "expected invalid authorization reason")
assert_true(failed_event.failure_details ~= nil, "expected failure details in event")
assert_equal(failed_event.route_id, "route-123", "expected route id in failed event")
assert_equal(failed_event.service_id, "service-123", "expected service id in failed event")
assert_equal(failed_event.payment_amount_atomic, "1000", "expected atomic amount in failed event")

local rechallenge_event = require("mpp.util.json").decode(state.logs[2])
assert_equal(rechallenge_event.event_name, "solana_pay.payment_requested", "expected re-challenge event")
assert_equal(rechallenge_event.receipt_status, "pending", "expected pending re-challenge status")

print("ok - native handler emits observability logs for payment requested, success, and failure flows")
