local M = {
  TOKEN_PROGRAM = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA',
  TOKEN_2022_PROGRAM = 'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb',
  ASSOCIATED_TOKEN_PROGRAM = 'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL',
  SYSTEM_PROGRAM = '11111111111111111111111111111111',
}

local KNOWN_MINTS = {
  USDC = {
    devnet = '4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU',
    ['mainnet-beta'] = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v',
  },
  PYUSD = {
    devnet = 'CXk2AMBfi3TwaEL2468s6zP8xq9NxTXjp9gjMgzeUynM',
    ['mainnet-beta'] = '2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo',
  },
}

function M.default_rpc_url(network)
  if network == 'devnet' then
    return 'https://api.devnet.solana.com'
  elseif network == 'localnet' then
    return 'http://localhost:8899'
  end
  return 'https://api.mainnet-beta.solana.com'
end

function M.resolve_mint(currency, network)
  local normalized = string.upper(currency or '')
  if normalized == 'SOL' then
    return nil
  end

  local known = KNOWN_MINTS[normalized]
  if known then
    if network == 'devnet' then
      return known.devnet
    end
    return known['mainnet-beta']
  end

  return currency
end

return M
