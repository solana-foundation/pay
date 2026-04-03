local crypto = require('mpp.util.crypto')
local expires = require('mpp.expires')
local json = require('mpp.util.json')
local types = require('mpp.protocol.core.types')

local M = {}

local function opaque_raw(value)
  if value == nil then
    return ''
  end
  return value:raw()
end

local function challenge_mt(challenge)
  return setmetatable(challenge, {
    __index = {
      to_echo = function(self)
        return {
          id = self.id,
          realm = self.realm,
          method = self.method,
          intent = self.intent,
          request = self.request,
          expires = self.expires,
          digest = self.digest,
          opaque = self.opaque,
        }
      end,
      verify = function(self, secret_key)
        local expected = M.compute_challenge_id(
          secret_key,
          self.realm,
          self.method,
          self.intent,
          self.request:raw(),
          self.expires,
          self.digest,
          opaque_raw(self.opaque)
        )
        return expected == self.id
      end,
      is_expired = function(self, now_epoch)
        return expires.is_expired(self.expires, now_epoch)
      end,
    },
  })
end

function M.compute_challenge_id(secret_key, realm, method, intent, request, expires_value, digest, opaque)
  local message = table.concat({
    realm or '',
    method or '',
    intent or '',
    request or '',
    expires_value or '',
    digest or '',
    opaque or '',
  }, '|')
  return crypto.hmac_sha256_base64url(secret_key, message)
end

function M.new_challenge_with_secret(secret_key, realm, method, intent, request)
  return M.new_challenge_with_secret_full(secret_key, realm, method, intent, request, nil, nil, nil, nil)
end

function M.new_challenge_with_secret_full(secret_key, realm, method, intent, request, expires_value, digest, description, opaque)
  return challenge_mt({
    id = M.compute_challenge_id(secret_key, realm, method, intent, request:raw(), expires_value, digest, opaque_raw(opaque)),
    realm = realm,
    method = method,
    intent = intent,
    request = request,
    expires = expires_value,
    description = description,
    digest = digest,
    opaque = opaque,
  })
end

function M.new_payment_credential(challenge_echo, payload, source)
  return {
    challenge = challenge_echo,
    payload = payload == nil and nil or payload,
    source = source,
  }
end

function M.payload_as(credential)
  if credential.payload == nil then
    return nil
  end
  return credential.payload
end

function M.new_receipt(receipt)
  receipt.status = receipt.status or types.RECEIPT_STATUS_SUCCESS
  return receipt
end

function M.challenge_from_table(value)
  value.request = types.new_base64url_json_raw(value.request)
  if value.opaque ~= nil then
    value.opaque = types.new_base64url_json_raw(value.opaque)
  end
  return challenge_mt(value)
end

function M.challenge_to_plain(value)
  return {
    id = value.id,
    realm = value.realm,
    method = value.method,
    intent = value.intent,
    request = value.request:raw(),
    expires = value.expires,
    description = value.description,
    digest = value.digest,
    opaque = value.opaque and value.opaque:raw() or nil,
  }
end

function M.credential_to_plain(value)
  local challenge = value.challenge
  return {
    challenge = {
      id = challenge.id,
      realm = challenge.realm,
      method = challenge.method,
      intent = challenge.intent,
      request = challenge.request:raw(),
      expires = challenge.expires,
      digest = challenge.digest,
      opaque = challenge.opaque and challenge.opaque:raw() or nil,
    },
    payload = value.payload,
    source = value.source,
  }
end

function M.receipt_to_plain(value)
  return {
    status = value.status,
    method = value.method,
    timestamp = value.timestamp,
    reference = value.reference,
    challengeId = value.challengeId,
    externalId = value.externalId,
  }
end

M.json = json

return M
