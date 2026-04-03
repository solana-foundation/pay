# Konnect

Konnect custom plugins require two separate pieces:

1. the plugin schema uploaded to the Konnect control plane
2. the plugin code installed on every Kong data plane node

For the Lua plugin, upload:

- `../../kong/plugins/solana-kong-402/schema.lua`

Then install the packaged Lua plugin on every data plane node, either by:

- baking it into a Kong image from `gateway/kong/Dockerfile`
- or installing the LuaRocks package from `pay-0.1.0-1.rockspec`

Relevant Kong docs:

- Konnect custom plugin schema upload: https://docs.konghq.com/konnect/gateway-manager/plugins/add-custom-plugin/
- Custom plugin installation and distribution: https://developer.konghq.com/custom-plugins/installation-and-distribution/
