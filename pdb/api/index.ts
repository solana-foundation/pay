import express from "express";
import cors from "cors";
import {
  generateKeyPairSigner,
  createKeyPairSignerFromBytes,
  getBase58Codec,
} from "@solana/kit";
import { Mppx, solana } from "@solana/mpp/server";
import { paymentMiddleware } from "x402-express";
import { FlowCorrelation, type LogEntry } from "./correlation.js";
import type { SSEMessage } from "./types.js";

const RPC_URL = process.env.RPC_URL || "https://402.surfnet.dev:8899";
const NETWORK = process.env.NETWORK || "localnet";
const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SECRET_KEY = process.env.SECRET_KEY || "demo-secret-key";

// ── Correlation engine ──
const correlation = new FlowCorrelation();

// Run stale-flow cleanup every 10s
setInterval(() => correlation.cleanup(), 10_000);

// ── Request log (in-memory ring buffer — kept for raw log consumers) ──
let logId = 0;
const MAX_LOGS = 200;
const logs: LogEntry[] = [];

function pushLog(entry: LogEntry) {
  logs.push(entry);
  if (logs.length > MAX_LOGS) logs.shift();
  // Feed into correlation engine
  correlation.ingest(entry);
}

function logToConsole(e: LogEntry) {
  const grey = "\x1b[90m";
  const reset = "\x1b[0m";
  const bold = "\x1b[1m";
  const green = "\x1b[32m";
  const yellow = "\x1b[33m";
  const red = "\x1b[31m";
  const cyan = "\x1b[36m";

  const statusColor =
    e.status === 402
      ? `${bold}${yellow}`
      : e.status < 300
        ? green
        : e.status < 500
          ? yellow
          : red;

  const ts = fmtTs(e.ts);
  const method = e.method.padEnd(6);
  const ms = `${e.ms}ms`.padStart(6);

  console.log(
    `${statusColor}${e.status}${reset}  ${cyan}${method}${reset} ${e.path}  ${grey}${ts}${reset}  ${grey}${ms}${reset}`,
  );
}

function fmtTs(iso: string): string {
  const d = new Date(iso);
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  const ss = String(d.getSeconds()).padStart(2, "0");
  const ms = String(d.getMilliseconds()).padStart(3, "0");
  return `${hh}:${mm}:${ss}.${ms}`;
}

// ── SSE clients ──
const sseClients = new Set<{
  res: import("express").Response;
  viewerIp: string;
}>();

function broadcastSSE(msg: SSEMessage) {
  const data = `data: ${JSON.stringify(msg)}\n\n`;
  for (const client of sseClients) client.res.write(data);
}

// Forward correlation events to SSE clients
correlation.subscribe(broadcastSSE);

// ── Cache app across warm invocations ──
let cachedApp: express.Express | null = null;

async function createApp() {
  if (cachedApp) return cachedApp;

  // ── Keypair setup ──
  let feePayerSigner;
  if (process.env.FEE_PAYER_KEY) {
    const bytes = getBase58Codec().encode(process.env.FEE_PAYER_KEY);
    feePayerSigner = await createKeyPairSignerFromBytes(bytes);
  } else {
    feePayerSigner = await generateKeyPairSigner();
  }

  const recipient = process.env.RECIPIENT || feePayerSigner.address;

  // Bootstrap fee payer on surfnet via cheatcodes (best-effort)
  await bootstrap(feePayerSigner.address).catch(() => {});

  // ── Express app ──
  const app = express();
  app.use(express.json());
  app.use(
    cors({
      exposedHeaders: [
        "www-authenticate",
        "payment-receipt", // MPP
        "x-payment-required",
        "x-payment-response", // x402
      ],
    }),
  );

  // ── Request logging middleware ──
  app.use((req, res, next) => {
    if (req.path === "/" || req.path.startsWith("/__debugger"))
      return next();
    const start = Date.now();

    const chunks: Buffer[] = [];
    let writeHeadHeaders: Record<string, string> = {};
    const origWriteHead = res.writeHead.bind(res) as any;
    res.writeHead = function (statusCode: number, headers?: any) {
      if (headers && typeof headers === "object") {
        for (const [k, v] of Object.entries(headers)) {
          if (v != null) writeHeadHeaders[String(k).toLowerCase()] = String(v);
        }
      }
      return origWriteHead(statusCode, headers);
    };
    const origWrite = res.write.bind(res) as any;
    const origEnd = res.end.bind(res) as any;
    res.write = function (chunk: any, ...args: any[]) {
      if (chunk)
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      return origWrite(chunk, ...args);
    };
    res.end = function (chunk: any, ...args: any[]) {
      if (chunk)
        chunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
      return origEnd(chunk, ...args);
    };

    res.on("finish", () => {
      const reqHeaders: Record<string, string> = {};
      for (const [k, v] of Object.entries(req.headers)) {
        if (typeof v === "string") reqHeaders[k] = v;
      }
      const resHeaders: Record<string, string> = { ...writeHeadHeaders };
      for (const [k, v] of Object.entries(res.getHeaders())) {
        if (v != null) resHeaders[k] = String(v);
      }
      let resBody: string | null = null;
      if (chunks.length > 0) {
        const raw = Buffer.concat(chunks).toString("utf-8");
        resBody = raw.length > 4096 ? raw.slice(0, 4096) + "…" : raw;
      }
      const clientIp =
        (req.headers["x-forwarded-for"] as string)?.split(",")[0]?.trim() ||
        req.socket.remoteAddress ||
        "unknown";

      const entry: LogEntry = {
        id: ++logId,
        ts: new Date().toISOString(),
        method: req.method,
        path: req.path,
        status: res.statusCode,
        ms: Date.now() - start,
        reqHeaders,
        resHeaders,
        resBody,
        clientIp,
      };
      pushLog(entry);
      logToConsole(entry);
    });
    next();
  });

  // ── SSE stream (flow events) ──
  app.get("/__debugger/logs/stream", (req, res) => {
    const viewerIp =
      (req.headers["x-forwarded-for"] as string)?.split(",")[0]?.trim() ||
      req.socket.remoteAddress ||
      "unknown";
    res.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    });
    // Send viewer IP so client can filter own flows
    const initMsg: SSEMessage = { type: "init", viewerIp };
    res.write(`data: ${JSON.stringify(initMsg)}\n\n`);
    // Send snapshot of current flows
    const snapshotMsg: SSEMessage = {
      type: "snapshot",
      flows: correlation.snapshot(),
    };
    res.write(`data: ${JSON.stringify(snapshotMsg)}\n\n`);

    const client = { res, viewerIp };
    sseClients.add(client);
    req.on("close", () => sseClients.delete(client));
  });

  app.get("/__debugger/logs", (_req, res) => res.json(correlation.snapshot()));

  // ── Config endpoint (for React frontend) ──
  app.get("/__debugger/api/config", (_req, res) => {
    res.json({
      recipient,
      network: NETWORK,
      rpcUrl: RPC_URL,
      endpoints: {
        mpp: [
          {
            method: "GET",
            path: "/mpp/quote/:symbol",
            price: "0.01 USDC",
            description: "Stock quote",
          },
          {
            method: "GET",
            path: "/mpp/weather/:city",
            price: "0.005 USDC",
            description: "Weather data",
          },
        ],
        x402: [
          {
            method: "GET",
            path: "/x402/joke",
            price: "$0.001",
            description: "Random joke",
          },
          {
            method: "GET",
            path: "/x402/fact",
            price: "$0.001",
            description: "Random fact",
          },
        ],
        oauth: [
          {
            method: "POST",
            path: "/v1/generate",
            price: "$0.001",
            description: "Generate content",
          },
          {
            method: "POST",
            path: "/v1/models/:id:predict",
            price: "$0.01",
            description: "Model prediction",
          },
          {
            method: "GET",
            path: "/v1/models",
            price: "free",
            description: "List models",
          },
          {
            method: "GET",
            path: "/v1/health",
            price: "free",
            description: "Health check",
          },
        ],
      },
    });
  });

  // ── MPP setup ──
  const mppx = Mppx.create({
    secretKey: SECRET_KEY,
    methods: [
      solana.charge({
        recipient,
        network: NETWORK,
        rpcUrl: RPC_URL,
        signer: feePayerSigner,
        currency: USDC_MINT,
        decimals: 6,
      }),
    ],
  });

  // ── MPP endpoints ──

  app.get("/mpp/quote/:symbol", async (req, res) => {
    const result = await mppx.charge({
      amount: "10000",
      currency: USDC_MINT,
      description: `Stock quote: ${req.params.symbol}`,
    })(toWebRequest(req));

    if (result.status === 402) {
      const challenge = result.challenge as Response;
      const body = await challenge.text();
      res.writeHead(challenge.status, Object.fromEntries(challenge.headers));
      res.end(body);
      return;
    }

    const response = result.withReceipt(
      Response.json({
        symbol: req.params.symbol.toUpperCase(),
        price: (Math.random() * 500).toFixed(2),
        currency: "USD",
        source: "mpp-demo",
      }),
    ) as Response;
    res.writeHead(response.status, Object.fromEntries(response.headers));
    res.end(await response.text());
  });

  app.get("/mpp/weather/:city", async (req, res) => {
    const result = await mppx.charge({
      amount: "5000",
      currency: USDC_MINT,
      description: `Weather: ${req.params.city}`,
    })(toWebRequest(req));

    if (result.status === 402) {
      const challenge = result.challenge as Response;
      const body = await challenge.text();
      res.writeHead(challenge.status, Object.fromEntries(challenge.headers));
      res.end(body);
      return;
    }

    const response = result.withReceipt(
      Response.json({
        city: req.params.city,
        temperature: Math.floor(Math.random() * 35) + 5,
        conditions: ["Sunny", "Cloudy", "Rainy", "Windy"][
          Math.floor(Math.random() * 4)
        ],
        source: "mpp-demo",
      }),
    ) as Response;
    res.writeHead(response.status, Object.fromEntries(response.headers));
    res.end(await response.text());
  });

  // ── Embedded facilitator ──

  app.get("/facilitator/supported", (_req, res) => {
    res.json({
      kinds: [
        {
          scheme: "exact",
          network: "solana-devnet",
          extra: { feePayer: feePayerSigner.address },
        },
      ],
    });
  });

  app.post("/facilitator/verify", (req, res) => {
    const { paymentPayload } = req.body;
    if (!paymentPayload?.payload) {
      return res.json({ isValid: false, invalidReason: "Missing payload" });
    }
    res.json({
      isValid: true,
      payer: paymentPayload.payload.authorization?.from || "unknown",
    });
  });

  app.post("/facilitator/settle", async (req, res) => {
    const { paymentPayload } = req.body;
    try {
      const payload = paymentPayload?.payload;
      if (!payload) {
        return res.json({ success: false, errorReason: "Missing payload" });
      }

      if (payload.transaction) {
        const result = await fetch(RPC_URL, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "sendTransaction",
            params: [
              payload.transaction,
              { encoding: "base64", skipPreflight: true },
            ],
          }),
        });
        const data = (await result.json()) as any;
        if (data.error) {
          return res.json({ success: false, errorReason: data.error.message });
        }
        return res.json({ success: true, transaction: data.result });
      }

      return res.json({
        success: true,
        transaction: "local-facilitator-settled",
      });
    } catch (err: any) {
      return res.json({ success: false, errorReason: err.message });
    }
  });

  // ── x402 endpoints ──
  const facilitatorUrl =
    process.env.FACILITATOR_URL || productionUrl() + "/facilitator";

  const x402App = express.Router();

  x402App.use(
    paymentMiddleware(
      recipient,
      {
        "/x402/joke": {
          price: "$0.001",
          network: "solana-devnet" as any,
          config: { description: "A random joke" },
        },
        "/x402/fact": {
          price: "$0.001",
          network: "solana-devnet" as any,
          config: { description: "A random fact" },
        },
      },
      { url: facilitatorUrl },
    ),
  );

  x402App.get("/x402/joke", (_req, res) => {
    const jokes = [
      "Why do programmers prefer dark mode? Because light attracts bugs.",
      "There are 10 types of people: those who understand binary and those who don't.",
      "A SQL query walks into a bar, sees two tables, and asks: 'Can I JOIN you?'",
    ];
    res.json({
      joke: jokes[Math.floor(Math.random() * jokes.length)],
      source: "x402-demo",
    });
  });

  x402App.get("/x402/fact", (_req, res) => {
    const facts = [
      "Honey never spoils. Archaeologists found 3000-year-old honey in Egyptian tombs.",
      "Octopuses have three hearts and blue blood.",
      "A group of flamingos is called a 'flamboyance'.",
    ];
    res.json({
      fact: facts[Math.floor(Math.random() * facts.length)],
      source: "x402-demo",
    });
  });

  app.use(x402App);

  // ── Health ──
  app.get("/health", (_req, res) => {
    res.json({ status: "ok", network: NETWORK, recipient });
  });

  // ── Root: redirect to Vite in dev, noop on Vercel (static serves SPA) ──
  app.get("/", (_req, res) => {
    if (isVercel) return res.json({ status: "ok", ui: "/" });
    res.redirect("http://localhost:5173");
  });

  // ── Catch-all: auth-gated echo ──
  app.all("*", (req, res) => {
    const authHeader = req.headers["authorization"];
    const queryKey = req.query["key"] as string | undefined;

    if (authHeader) {
      const parts = authHeader.split(" ");
      if (parts[0]?.toLowerCase() !== "bearer" || !parts[1]?.length) {
        return res.status(401).json({
          error: {
            code: 401,
            message: "Invalid credentials",
            status: "UNAUTHENTICATED",
          },
        });
      }
    } else if (!queryKey?.length) {
      return res.status(403).json({
        error: {
          code: 403,
          message: "Missing API key or access token",
          status: "PERMISSION_DENIED",
        },
      });
    }

    res.json({
      ok: true,
      method: req.method,
      path: req.path,
      query: Object.fromEntries(
        Object.entries(req.query).filter(([k]) => k !== "key"),
      ),
      timestamp: new Date().toISOString(),
    });
  });

  cachedApp = app;
  return app;
}

// ── Surfnet bootstrap ──

async function bootstrap(address: string) {
  const rpc = (method: string, params: any[]) =>
    fetch(RPC_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
      signal: AbortSignal.timeout(5000),
    }).then((r) => r.json() as Promise<any>);

  await rpc("surfnet_setAccount", [
    address,
    {
      lamports: 100_000_000_000,
      data: "",
      executable: false,
      owner: "11111111111111111111111111111111",
    },
  ]);

  await rpc("surfnet_setTokenAccount", [
    address,
    USDC_MINT,
    { amount: 1_000_000_000 },
  ]);
}

// ── Helpers ──

function productionUrl(): string {
  if (process.env.VERCEL_PROJECT_PRODUCTION_URL)
    return `https://${process.env.VERCEL_PROJECT_PRODUCTION_URL}`;
  if (process.env.VERCEL_URL) return `https://${process.env.VERCEL_URL}`;
  const port = process.env.PORT || "3000";
  return `http://localhost:${port}`;
}

function toWebRequest(req: express.Request): globalThis.Request {
  const protocol = req.headers["x-forwarded-proto"] || req.protocol || "https";
  const host = req.headers.host || "localhost";
  const url = `${protocol}://${host}${req.originalUrl}`;
  return new globalThis.Request(url, {
    method: req.method,
    headers: new Headers(req.headers as Record<string, string>),
    body: ["GET", "HEAD"].includes(req.method)
      ? undefined
      : JSON.stringify(req.body),
  });
}

// ── Local dev server ──
const isVercel = !!process.env.VERCEL;
if (!isVercel) {
  const port = parseInt(process.env.PORT || "3000", 10);
  createApp().then((app) => {
    app.listen(port, () => {
      const g = "\x1b[32m";
      const c = "\x1b[36m";
      const d = "\x1b[90m";
      const r = "\x1b[0m";
      const b = "\x1b[1m";
      console.log();
      console.log(`  ${b}${g}payment-debugger${r}  ${d}v0.2.0${r}`);
      console.log();
      console.log(`  ${d}Local:${r}     ${c}http://localhost:${port}${r}`);
      console.log(`  ${d}Network:${r}   ${NETWORK}`);
      console.log(`  ${d}RPC:${r}       ${RPC_URL}`);
      console.log();
      console.log(`  ${d}MPP${r}   GET /mpp/quote/:symbol  ${d}0.01 USDC${r}`);
      console.log(
        `  ${d}MPP${r}   GET /mpp/weather/:city   ${d}0.005 USDC${r}`,
      );
      console.log(
        `  ${d}x402${r}  GET /x402/joke           ${d}$0.001${r}`,
      );
      console.log(
        `  ${d}x402${r}  GET /x402/fact            ${d}$0.001${r}`,
      );
      console.log();
    });
  });
}

// ── Vercel handler ──
export default async function handler(req: any, res: any) {
  const app = await createApp();
  return app(req, res);
}
