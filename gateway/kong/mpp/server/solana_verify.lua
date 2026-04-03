local uint = require('mpp.util.uint')
local protocol = require('mpp.protocol.solana')

local M = {}

local TOKEN_PROGRAM = 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA'
local TOKEN_2022_PROGRAM = 'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb'
local verify_sol_transfers
local verify_spl_transfers

local function is_native_sol(currency)
  return string.lower(currency or '') == 'sol'
end

local function sum_split_amounts(splits)
  local total = '0'
  for _, split in ipairs(splits or {}) do
    total = uint.add(total, split.amount)
  end
  return total
end

local function primary_amount(amount, splits)
  local total_splits = sum_split_amounts(splits)
  if uint.compare(amount, total_splits) <= 0 then
    error('splits consume the entire amount')
  end
  return uint.sub(amount, total_splits)
end

local function build_expected_transfers(request)
  local splits = (request.methodDetails and request.methodDetails.splits) or {}
  local primary = primary_amount(request.amount, splits)
  local expected = {
    { recipient = request.recipient, amount = primary },
  }
  for _, split in ipairs(splits) do
    expected[#expected + 1] = {
      recipient = split.recipient,
      amount = split.amount,
    }
  end
  return expected
end

local function remove_at(list, index)
  table.remove(list, index)
end

local function normalize_program_id(ix)
  return ix.programId or ix.program_id or ''
end

local function normalize_program(ix)
  return ix.program or ''
end

local function instruction_info(ix)
  return ix.parsed and ix.parsed.info or nil
end

local function verify_confirmed_transaction(reference, tx, request, method_details, hooks)
  if not tx then
    error('transaction not found or not yet confirmed')
  end
  if tx.meta and tx.meta.err ~= nil then
    error('transaction failed on-chain')
  end

  local instructions = tx.transaction and tx.transaction.message and tx.transaction.message.instructions or {}
  if is_native_sol(request.currency) then
    verify_sol_transfers(instructions, request)
  else
    if not hooks.fetch_token_account then
      error('fetch_token_account callback is required for token verification')
    end
    verify_spl_transfers(instructions, request, method_details, hooks)
  end

  return {
    reference = reference,
  }
end

function verify_sol_transfers(instructions, request)
  local expected = build_expected_transfers(request)
  local transfers = {}
  for _, ix in ipairs(instructions or {}) do
    if normalize_program(ix) == 'system' and ix.parsed and ix.parsed.type == 'transfer' then
      transfers[#transfers + 1] = ix
    end
  end
  for _, want in ipairs(expected) do
    local found = false
    for idx, ix in ipairs(transfers) do
      local info = instruction_info(ix)
      if info and info.destination == want.recipient and uint.compare(info.lamports, want.amount) == 0 then
        remove_at(transfers, idx)
        found = true
        break
      end
    end
    if not found then
      error('no matching SOL transfer for ' .. want.recipient)
    end
  end
end

function verify_spl_transfers(instructions, request, method_details, hooks)
  local expected = build_expected_transfers(request)
  local program_id = method_details.tokenProgram or TOKEN_PROGRAM
  local mint = protocol.resolve_mint(request.currency, method_details.network)
  if program_id ~= TOKEN_PROGRAM and program_id ~= TOKEN_2022_PROGRAM then
    error('unsupported token program: ' .. tostring(program_id))
  end
  local transfers = {}
  for _, ix in ipairs(instructions or {}) do
    if ix.parsed and ix.parsed.type == 'transferChecked' and normalize_program_id(ix) == program_id then
      transfers[#transfers + 1] = ix
    end
  end
  for _, want in ipairs(expected) do
    local found = false
    for idx, ix in ipairs(transfers) do
      local info = instruction_info(ix)
      if info and info.mint == mint and uint.compare(info.tokenAmount.amount, want.amount) == 0 then
        local account = hooks.fetch_token_account(info.destination)
        if account and account.owner == want.recipient and account.mint == mint then
          remove_at(transfers, idx)
          found = true
          break
        end
      end
    end
    if not found then
      error('no matching token transfer for ' .. want.recipient)
    end
  end
end

function M.verify_signature(context, hooks)
  local payload = context.payload or {}
  local request = context.request or {}
  local method_details = context.method_details or request.methodDetails or {}

  if payload.signature == nil or payload.signature == '' then
    error('missing signature in credential payload')
  end

  if not hooks or type(hooks.fetch_transaction) ~= 'function' then
    error('fetch_transaction callback is required')
  end

  local tx = hooks.fetch_transaction(payload.signature)
  return verify_confirmed_transaction(payload.signature, tx, request, method_details, hooks)
end

function M.verify_transaction(context, hooks)
  local payload = context.payload or {}
  local request = context.request or {}
  local method_details = context.method_details or request.methodDetails or {}

  if payload.transaction == nil or payload.transaction == '' then
    error('missing transaction in credential payload')
  end
  if not hooks or type(hooks.send_transaction) ~= 'function' then
    error('send_transaction callback is required')
  end
  if type(hooks.await_transaction) ~= 'function' then
    error('await_transaction callback is required')
  end

  local signature = hooks.send_transaction(payload.transaction)
  if signature == nil or signature == '' then
    error('send_transaction returned an empty signature')
  end
  local tx = hooks.await_transaction(signature)
  return verify_confirmed_transaction(signature, tx, request, method_details, hooks)
end

function M.new_signature_verifier(hooks)
  return function(context)
    if context.payload.type == 'transaction' then
      return M.verify_transaction(context, hooks)
    end
    return M.verify_signature(context, hooks)
  end
end

return M
