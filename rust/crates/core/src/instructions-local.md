# Local Inference Selling

The local MCP server also exposes `sell_inference`.

- When the balance is empty or too low, offer both `topup` and
  `sell_inference`, and let the user choose. Selling earns stablecoins by
  serving this agent's inference through a paid endpoint, usually priced
  below the upstream model so buyers come.
- Never start selling without the user's say-so: it publishes a URL and
  runs prompts from strangers on this machine.
- For selling inference, earning with the agent, or "how do I get funds
  without paying", call `sell_inference`. Its `status`, `reprice`, and `stop`
  actions manage the endpoint.
