#!/usr/bin/env python3
"""Drive `pay mcp` over stdio: sell_inference create, then status, then stop.

Usage: e2e-sell-mcp.py <pay-bin> <action> [endpoint_id]
  create  -> prints the new endpoint id on stdout
  status  -> prints the tool text
  stop    -> prints the tool text
The environment must carry PAY_CONNECT_URL, PAY_CONNECT_TOKEN, PAY_SELL_DIR
and SELL_RECIPIENT; `pay --local mcp` pins localnet like the buyer does.
"""
import json
import os
import re
import subprocess
import sys


def main() -> int:
    pay, action = sys.argv[1], sys.argv[2]
    endpoint_id = sys.argv[3] if len(sys.argv) > 3 else None
    proc = subprocess.Popen(
        [pay, "--local", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=open(os.environ.get("MCP_LOG", os.devnull), "ab"),
        text=True,
        bufsize=1,
    )

    def send(msg):
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def recv(expect_id):
        while True:
            line = proc.stdout.readline()
            if not line:
                raise SystemExit("pay mcp closed its stdout")
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == expect_id:
                return msg

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "e2e", "version": "0"}}})
    init = recv(1)
    tools_listed = init["result"]["serverInfo"]["name"]
    assert tools_listed == "pay", init
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    args = {"action": action}
    if action == "create":
        args.update({
            "price_per_request_usd": 0.02,
            "model": "mcp-agent",
            "harness": "echo",
            "recipient": os.environ["SELL_RECIPIENT"],
            "network": "localnet",
            "session_idle_close_secs": 5,
            # Two $0.02 answers cross this; the third buyer gets 410.
            "earn_cap_usd": 0.03,
        })
    elif endpoint_id:
        args["endpoint_id"] = endpoint_id
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
          "params": {"name": "sell_inference", "arguments": args}})
    result = recv(2)
    proc.stdin.close()
    proc.wait(timeout=10)
    if "error" in result:
        print(result["error"], file=sys.stderr)
        return 1
    text = "\n".join(c.get("text", "") for c in result["result"].get("content", []))
    if result["result"].get("isError"):
        print(text, file=sys.stderr)
        return 1
    if action == "create":
        match = re.search(r"endpoint_id: (\S+)", text)
        if not match:
            print(text, file=sys.stderr)
            return 1
        print(match.group(1))
        print(text, file=sys.stderr)
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
