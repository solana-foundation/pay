local challenge = require('mpp.protocol.core.challenge')
local intents = require('mpp.protocol.intents.charge')
local protocol = require('mpp.protocol.solana')
local solana_verify = require('mpp.server.solana_verify')
local store = require('mpp.store')
local types = require('mpp.protocol.core.types')

local M = {}

local DEFAULT_REALM = 'MPP Payment'
local CONSUMED_PREFIX = 'solana-charge:consumed:'

local Server = {}
Server.__index = Server

local function is_native_sol(currency)
  return string.lower(currency or '') == 'sol'
end

local function bool_or_nil(value)
  if value == nil then
    return nil
  end
  return value and true or false
end

function M.new(config)
  if type(config) ~= 'table' then
    error('config table is required')
  end
  if type(config.recipient) ~= 'string' or config.recipient == '' then
    error('recipient is required')
  end
  local secret_key = config.secret_key or os.getenv('MPP_SECRET_KEY')
  if secret_key == nil or secret_key == '' then
    error('missing secret key')
  end
  local instance = {
    secret_key = secret_key,
    realm = config.realm or DEFAULT_REALM,
    recipient = config.recipient,
    currency = config.currency or 'USDC',
    decimals = config.decimals or 6,
    network = config.network or 'mainnet-beta',
    rpc_url = config.rpc_url or protocol.default_rpc_url(config.network or 'mainnet-beta'),
    fee_payer = bool_or_nil(config.fee_payer),
    fee_payer_key = config.fee_payer_key,
    store = config.store or store.memory(),
    verify_payment = config.verify_payment,
    recent_blockhash = config.recent_blockhash,
  }
  if instance.verify_payment == nil and config.verifier_hooks ~= nil then
    instance.verify_payment = solana_verify.new_signature_verifier(config.verifier_hooks)
  end
  return setmetatable(instance, Server)
end

function Server:charge(amount)
  return self:charge_with_options(amount, {})
end

function Server:charge_with_options(amount, options)
  options = options or {}
  local base_units = intents.parse_units(amount, self.decimals)
  local method_details = {
    network = self.network,
  }
  if not is_native_sol(self.currency) then
    method_details.decimals = self.decimals
    if options.token_program then
      method_details.tokenProgram = options.token_program
    end
  end
  if options.fee_payer or self.fee_payer then
    method_details.feePayer = true
    if options.fee_payer_key or self.fee_payer_key then
      method_details.feePayerKey = options.fee_payer_key or self.fee_payer_key
    end
  end
  if options.splits then
    method_details.splits = options.splits
  end
  if options.recent_blockhash or self.recent_blockhash then
    method_details.recentBlockhash = options.recent_blockhash or self.recent_blockhash
  end
  local request = types.new_base64url_json_value({
    amount = base_units,
    currency = self.currency,
    recipient = self.recipient,
    description = options.description,
    externalId = options.external_id,
    methodDetails = method_details,
  })
  return challenge.new_challenge_with_secret_full(
    self.secret_key,
    self.realm,
    types.new_method_name('solana'),
    types.new_intent_name('charge'),
    request,
    options.expires,
    nil,
    options.description,
    nil
  )
end

function Server:verify_credential(credential_value, now_epoch)
  local echoed = credential_value.challenge
  local challenge_value = challenge.challenge_from_table({
    id = echoed.id,
    realm = echoed.realm,
    method = echoed.method,
    intent = echoed.intent,
    request = echoed.request:raw(),
    expires = echoed.expires,
    digest = echoed.digest,
    opaque = echoed.opaque and echoed.opaque:raw() or nil,
  })

  if not challenge_value:verify(self.secret_key) then
    error('challenge ID mismatch')
  end
  if challenge_value:is_expired(now_epoch or os.time()) then
    error('challenge expired at ' .. tostring(challenge_value.expires))
  end

  local request, decode_err = challenge_value.request:decode()
  if not request then
    error(decode_err)
  end
  local method_details = request.methodDetails or {}
  local payload = challenge.payload_as(credential_value) or {}
  local payload_type = payload.type
  if payload_type ~= 'transaction' and payload_type ~= 'signature' then
    error('missing or invalid payload type')
  end
  if payload_type == 'signature' and method_details.feePayer then
    error('type="signature" credentials cannot be used with fee sponsorship')
  end
  if type(self.verify_payment) ~= 'function' then
    error('verify_payment callback is required')
  end

  local result = self.verify_payment({
    payload = payload,
    request = request,
    method_details = method_details,
    credential = credential_value,
    store = self.store,
    server = self,
  }) or {}

  local reference = result.reference or payload.signature or payload.transaction
  if reference == nil or reference == '' then
    error('verification result must include a reference')
  end

  local replay_key = result.replay_key or (CONSUMED_PREFIX .. reference)
  local inserted = self.store:put_if_absent(replay_key, true)
  if not inserted then
    error('payment already consumed')
  end

  return challenge.new_receipt({
    method = 'solana',
    timestamp = result.timestamp or os.date('!%Y-%m-%dT%H:%M:%SZ'),
    reference = reference,
    challengeId = echoed.id,
    externalId = request.externalId,
    status = result.status or types.RECEIPT_STATUS_SUCCESS,
  })
end

M.Server = Server

return M
