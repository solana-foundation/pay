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
    ctx = {
      shared = state.shared,
    },
  }
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

local challenge = mpp.ParseWWWAuthenticate(state.response_headers["www-authenticate"])
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

plugin:response()
assert_true(state.response_headers["payment-receipt"] ~= nil, "expected downstream payment receipt header")

print("ok - native handler issues challenge and verifies transaction credentials")
