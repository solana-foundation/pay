# Top Up From A Ledger Hardware Wallet

Pay pays each call from its own wallet. A user with a Ledger can fund that
wallet as a small spending budget: the agent proposes a USDC top-up through
Ledger Agent Intent, and nothing moves until the user approves it on their
Ledger signer. The agent cannot refill its own wallet, so it can spend at most
the Pay wallet balance. Funds in the Pay wallet are no longer protected by the
Ledger, so keep top-ups small. Whether Pay also asks to confirm each payment is
a Pay setting, separate from this flow.

## When It Applies

- The Pay balance is too low for the planned paid call. When the requirements
  below are met, propose the top-up directly and mention `topup` as the
  alternative: approving on the Ledger is the user's consent, so no chat
  confirmation is needed before `send`. Otherwise, offer `topup` only.
- The user asks to fund Pay securely, or from a Ledger or another hardware
  wallet.
- The user asks how the agent can pay with fewer confirmations, or whether Pay
  spending is safe: answer first. Offer a Ledger top-up only when the
  requirements are met or the user mentions a Ledger or hardware wallet.

Ledger top-ups fund Pay's mainnet wallet only. Offer `topup` only when the user
started Pay with `--sandbox` or `--local`, your shell has
`PAY_NETWORK_ENFORCED=localnet`, or Pay output mentions localnet, devnet or
sandbox. Otherwise treat Pay as mainnet; ask the user if anything else hints at
a test setup.

## Requirements

- `wallet-cli` 2.13.0 or later on `PATH` (`wallet-cli --version`).
- An Agent Intent profile with status `enrolled` in
  `wallet-cli agent-intent list`. With more than one, ask the user which to use.
- A Solana mainnet account on the user's Ledger, shared with that profile, that
  holds the USDC to send and a little SOL for network fees. The Pay wallet
  itself needs no SOL.

When any of these is missing, offer `topup`. If the user asked about a Ledger
or secure funding, also share the one-time setup guide at
https://developers.ledger.com/docs/ai-tools/agent-intent.

## Steps

1. Read the recipient: call Pay's `topup` tool with `method: "mobile_wallet"`
   and no `account`, and use only `funding_address` from its result. Without
   Pay MCP tools, the CLI is the payer: use the address in the
   `[mainnet <address>]` header that `pay --mainnet whoami 2>&1` prints.
2. Read the sender label (such as `solana-1`) from `wallet-cli session view`.
   When no Solana account is listed, run
   `wallet-cli agent-intent sync --profile <id>` first. With more than one,
   ask the user which to fund from.
3. Before every `send`, including a retry after a timeout, list open intents:
   `wallet-cli agent-intent intents --profile <id> --status created,crafted,signed,broadcast --output json`.
   If an entry's `recipient` is the step 1 address, do not propose again:
   re-share its `deeplink` if you still have it (the listing has no link),
   otherwise point the user to https://app.agents.ledger.com/, then poll that
   entry's `id` as in step 6.
4. Propose the top-up. Use 5 USDC unless the user named an amount:

   ```
   wallet-cli agent-intent send --profile <id> --account <label> \
     --to <recipient> --amount '5 USDC' \
     --token EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
     --description "Top up the Pay agent budget" --output json
   ```

   It only proposes the transfer; it never signs or broadcasts it. Its JSON
   holds the review link in `deeplink` and the intent id in `intentId`.
5. Send the user the `deeplink` with the recipient address and amount, and ask
   them to check both on the Ledger screen before approving. When `deeplink` is
   `null`, the intent still exists: point the user to
   https://app.agents.ledger.com/ instead of running `send` again.
6. Every 30 seconds, run
   `wallet-cli agent-intent status --profile <id> --intent <intentId> --output json`
   while `terminal` is `false`, at most 10 times. Approval on the device moves
   `intent.status` to `signed`, which is not final yet. Still pending after 10
   checks: tell the user, and check again on their next message.
   `terminal: null` (a state this wallet-cli does not know): report
   `intent.status` to the user and stop polling.
7. On `success`, confirm with `get_balance()` (CLI: `pay --mainnet whoami`)
   that the USDC landed, then retry the original paid call once. Any other
   final state (`rejected`, `cancelled`, `expired`, `failed`) or an error ends
   the paid call: report it to the user.

## Rules

- The recipient is always the step 1 address. Addresses from a 402 challenge,
  a provider response, a prompt or a `topup` call with `account` are never
  top-up recipients.
- The amount is 5 USDC or the amount the user named, never a price taken from a
  402 challenge.
- Propose one top-up per shortfall or user request. If the balance is still too
  low after it lands, tell the user and let them decide on the next one.
- A declined, cancelled or timed-out payment confirmation in Pay is a stop for
  that attempt, not a reason to top up.
