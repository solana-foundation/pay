local bit = require('mpp.util.bit')
local base64url = require('mpp.util.base64url')

local M = {}
local UINT32 = 4294967296

local K = {
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
  0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
  0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
  0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
  0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
  0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
  0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
  0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
  0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
  0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
  0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
}

local function to_bytes_from_words(words)
  local out = {}
  for i = 1, #words do
    local value = words[i]
    out[#out + 1] = string.char(bit.rshift(value, 24) % 256)
    out[#out + 1] = string.char(bit.rshift(value, 16) % 256)
    out[#out + 1] = string.char(bit.rshift(value, 8) % 256)
    out[#out + 1] = string.char(value % 256)
  end
  return table.concat(out)
end

local function to_word(bytes, start)
  return bit.add(
    bit.lshift(bytes[start], 24),
    bit.lshift(bytes[start + 1], 16),
    bit.lshift(bytes[start + 2], 8),
    bytes[start + 3]
  )
end

local function ch(x, y, z)
  return bit.bxor(bit.band(x, y), bit.band(bit.bnot(x), z))
end

local function maj(x, y, z)
  return bit.bxor(bit.bxor(bit.band(x, y), bit.band(x, z)), bit.band(y, z))
end

local function big_sigma0(x)
  return bit.bxor(bit.bxor(bit.rrotate(x, 2), bit.rrotate(x, 13)), bit.rrotate(x, 22))
end

local function big_sigma1(x)
  return bit.bxor(bit.bxor(bit.rrotate(x, 6), bit.rrotate(x, 11)), bit.rrotate(x, 25))
end

local function small_sigma0(x)
  return bit.bxor(bit.bxor(bit.rrotate(x, 7), bit.rrotate(x, 18)), bit.rshift(x, 3))
end

local function small_sigma1(x)
  return bit.bxor(bit.bxor(bit.rrotate(x, 17), bit.rrotate(x, 19)), bit.rshift(x, 10))
end

function M.sha256(input)
  local bytes = { input:byte(1, #input) }
  local bit_len = #bytes * 8
  bytes[#bytes + 1] = 0x80
  while (#bytes % 64) ~= 56 do
    bytes[#bytes + 1] = 0
  end

  local high = math.floor(bit_len / UINT32)
  local low = bit_len % UINT32
  local length_words = {
    0, 0, 0, 0,
    bit.rshift(high, 24) % 256,
    bit.rshift(high, 16) % 256,
    bit.rshift(high, 8) % 256,
    high % 256,
    bit.rshift(low, 24) % 256,
    bit.rshift(low, 16) % 256,
    bit.rshift(low, 8) % 256,
    low % 256,
  }
  for i = 5, 12 do
    bytes[#bytes + 1] = length_words[i]
  end

  local h0 = 0x6a09e667
  local h1 = 0xbb67ae85
  local h2 = 0x3c6ef372
  local h3 = 0xa54ff53a
  local h4 = 0x510e527f
  local h5 = 0x9b05688c
  local h6 = 0x1f83d9ab
  local h7 = 0x5be0cd19

  for chunk_start = 1, #bytes, 64 do
    local w = {}
    for i = 0, 15 do
      local start = chunk_start + (i * 4)
      w[i] = to_word(bytes, start)
    end
    for i = 16, 63 do
      w[i] = bit.add(small_sigma1(w[i - 2]), w[i - 7], small_sigma0(w[i - 15]), w[i - 16])
    end

    local a, b, c, d = h0, h1, h2, h3
    local e, f, g, h = h4, h5, h6, h7

    for i = 0, 63 do
      local t1 = bit.add(h, big_sigma1(e), ch(e, f, g), K[i + 1], w[i])
      local t2 = bit.add(big_sigma0(a), maj(a, b, c))
      h = g
      g = f
      f = e
      e = bit.add(d, t1)
      d = c
      c = b
      b = a
      a = bit.add(t1, t2)
    end

    h0 = bit.add(h0, a)
    h1 = bit.add(h1, b)
    h2 = bit.add(h2, c)
    h3 = bit.add(h3, d)
    h4 = bit.add(h4, e)
    h5 = bit.add(h5, f)
    h6 = bit.add(h6, g)
    h7 = bit.add(h7, h)
  end

  return to_bytes_from_words({ h0, h1, h2, h3, h4, h5, h6, h7 })
end

function M.hmac_sha256(key, message)
  local block_size = 64
  if #key > block_size then
    key = M.sha256(key)
  end
  if #key < block_size then
    key = key .. string.rep('\0', block_size - #key)
  end

  local o_key = {}
  local i_key = {}
  for i = 1, block_size do
    local byte = key:byte(i)
    o_key[i] = string.char(bit.bxor(byte, 0x5c))
    i_key[i] = string.char(bit.bxor(byte, 0x36))
  end

  local inner = M.sha256(table.concat(i_key) .. message)
  return M.sha256(table.concat(o_key) .. inner)
end

function M.hmac_sha256_base64url(key, message)
  return base64url.encode(M.hmac_sha256(key, message))
end

return M
