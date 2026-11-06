import { createServer, type Server } from "node:http";
import { once } from "node:events";
import {
  afterAll,
  beforeAll,
  beforeEach,
  describe,
  expect,
  test,
  vi,
} from "vitest";
import {
  generateKeyPairSigner,
  getBase64Codec,
  getTransactionDecoder,
  getCompiledTransactionMessageDecoder,
} from "@solana/kit";
import { createPayKitClient } from "@solana/pay-kit/client";

const RPC = "http://rpc.test";
const realFetch = globalThis.fetch;
const methods: string[] = [];
let failSimulation = false;
let broadcast = "";
let server: Server;
let base: string;
let client: Awaited<ReturnType<typeof createPayKitClient>>;
let credential: string | undefined;

beforeAll(async () => {
  vi.stubEnv("VERCEL", "1");
  vi.stubEnv("RPC_URL", RPC);
  vi.stubGlobal(
    "fetch",
    async (input: Parameters<typeof fetch>[0], init?: RequestInit) => {
      if (String(input) !== RPC) {
        return realFetch(input, init);
      }
      const { method, params, id } = JSON.parse(String(init?.body));
      methods.push(method);
      let result: unknown;
      switch (method) {
        case "surfnet_setAccount":
        case "surfnet_setTokenAccount":
          result = null;
          break;
        case "getLatestBlockhash":
          result = {
            context: { slot: 314 },
            value: {
              blockhash: "EkSnNWid2cvwEVnVx9aBqawnmiCNiDgp3gUdkDPTKN1N",
              lastValidBlockHeight: 1000,
            },
          };
          break;
        case "simulateTransaction":
          if (failSimulation) {
            failSimulation = false;
            return Response.json({
              jsonrpc: "2.0",
              id,
              error: { code: -32000, message: "transient simulation failure" },
            });
          }
          result = { value: { err: null, logs: [] } };
          break;
        case "sendTransaction":
          broadcast = params[0];
          result = "mock-signature";
          break;
        case "getSignatureStatuses":
          result = { value: [{ err: null, confirmationStatus: "confirmed" }] };
          break;
        case "getTransaction": {
          const tx = getTransactionDecoder().decode(
            getBase64Codec().encode(broadcast),
          );
          const message = getCompiledTransactionMessageDecoder().decode(
            tx.messageBytes,
          );
          if (message.version === 1)
            throw new Error("Expected a v0 transaction");
          const instructions = message.instructions
            .filter((ix) => ix.data?.[0] === 12)
            .map((ix) => ({
              programId: message.staticAccounts[ix.programAddressIndex],
              parsed: {
                type: "transferChecked",
                info: {
                  destination: message.staticAccounts[ix.accountIndices![2]!],
                  mint: message.staticAccounts[ix.accountIndices![1]!],
                  tokenAmount: {
                    amount: new DataView(Uint8Array.from(ix.data!).buffer)
                      .getBigUint64(1, true)
                      .toString(),
                  },
                },
              },
            }));
          result = {
            version: 0,
            meta: { err: null },
            transaction: { message: { instructions } },
          };
          break;
        }
        default:
          throw new Error(`Unexpected RPC: ${method}`);
      }
      return Response.json({ jsonrpc: "2.0", id, result });
    },
  );
  const { default: handler } = await import("./index.js");
  server = createServer((req, res) => {
    if (req.headers.authorization) credential = req.headers.authorization;
    void handler(req, res);
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("Missing server address");
  base = `http://127.0.0.1:${address.port}`;
});
beforeEach(async () => {
  methods.length = 0;
  credential = undefined;
  failSimulation = false;
  client = await createPayKitClient({
    signer: await generateKeyPairSigner(),
    rpcUrl: RPC,
    network: "localnet",
    accept: ["mpp"],
    permissions: false,
  });
});
afterAll(async () => {
  if (server)
    await new Promise<void>((resolve) => server.close(() => resolve()));
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

describe.each([
  ["/mpp/quote/AAPL", "10000", "symbol", "AAPL"],
  ["/mpp/weather/paris", "5000", "city", "paris"],
])("MPP route %s", (path, amount, key, value) => {
  test("unpaid request challenges for the configured amount", async () => {
    const response = await fetch(base + path);
    expect(response.status).toBe(402);
    const header = response.headers.get("www-authenticate")!;
    expect(header).toContain("Payment");
    const encoded = /request="([^"]+)"/.exec(header)![1]!;
    expect(
      JSON.parse(Buffer.from(encoded, "base64url").toString()).amount,
    ).toBe(amount);
    expect(methods).not.toContain("sendTransaction");
  });
  test("paid request returns route data and a receipt", async () => {
    const response = await client.fetch(base + path);
    expect(response.status).toBe(200);
    expect(response.headers.get("payment-receipt")).toBeTruthy();
    expect(await response.json()).toMatchObject({
      [key]: value,
      source: "mpp-demo",
    });
    expect(methods.filter((m) => m === "sendTransaction")).toHaveLength(1);
  });
  test.each(["application/json", "text/html"])(
    "simulation failure does not settle while rendering %s",
    async (accept) => {
      failSimulation = true;
      const response = await client.fetch(base + path, { headers: { accept } });
      expect(response.status).toBe(402);
      expect(response.headers.get("payment-receipt")).toBeNull();
      expect(methods.filter((m) => m === "simulateTransaction")).toHaveLength(
        1,
      );
      expect(methods).not.toContain("sendTransaction");
      expect(credential).toBeTruthy();
      const retry = await fetch(base + path, {
        headers: { authorization: credential! },
      });
      expect(retry.status).toBe(200);
      expect(retry.headers.get("payment-receipt")).toBeTruthy();
      expect(await retry.json()).toMatchObject({ [key]: value });
    },
  );
});
