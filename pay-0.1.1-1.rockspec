package = "pay"
version = "0.1.1-1"
source = {
  url = "git+https://github.com/solana-foundation/pay",
}
description = {
  summary = "Native Lua Kong plugin for Solana-backed 402/MPP payment verification",
  detailed = [[
Self-contained Kong plugin distribution for Solana Pay.
It installs the solana-kong-402 plugin plus the bundled Lua MPP runtime used
for challenge generation and on-chain payment verification.
]],
  homepage = "https://github.com/solana-foundation/pay",
  license = "Apache-2.0",
}
build = {
  type = "builtin",
  modules = {
    ["kong.plugins.solana-kong-402.handler"] = "gateway/kong/kong/plugins/solana-kong-402/handler.lua",
    ["kong.plugins.solana-kong-402.schema"] = "gateway/kong/kong/plugins/solana-kong-402/schema.lua",
    ["mpp"] = "gateway/kong/mpp/init.lua",
    ["mpp.error"] = "gateway/kong/mpp/error.lua",
    ["mpp.expires"] = "gateway/kong/mpp/expires.lua",
    ["mpp.store"] = "gateway/kong/mpp/store.lua",
    ["mpp.protocol.solana"] = "gateway/kong/mpp/protocol/solana.lua",
    ["mpp.protocol.core.challenge"] = "gateway/kong/mpp/protocol/core/challenge.lua",
    ["mpp.protocol.core.headers"] = "gateway/kong/mpp/protocol/core/headers.lua",
    ["mpp.protocol.core.types"] = "gateway/kong/mpp/protocol/core/types.lua",
    ["mpp.protocol.intents.charge"] = "gateway/kong/mpp/protocol/intents/charge.lua",
    ["mpp.server"] = "gateway/kong/mpp/server/init.lua",
    ["mpp.server.html"] = "gateway/kong/mpp/server/html.lua",
    ["mpp.server.html_assets"] = "gateway/kong/mpp/server/html_assets/gen.lua",
    ["mpp.server.solana_verify"] = "gateway/kong/mpp/server/solana_verify.lua",
    ["mpp.util.base64url"] = "gateway/kong/mpp/util/base64url.lua",
    ["mpp.util.bit"] = "gateway/kong/mpp/util/bit.lua",
    ["mpp.util.crypto"] = "gateway/kong/mpp/util/crypto.lua",
    ["mpp.util.json"] = "gateway/kong/mpp/util/json.lua",
    ["mpp.util.uint"] = "gateway/kong/mpp/util/uint.lua",
  },
}
