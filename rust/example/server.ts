import express from 'express'
import cors from 'cors'
import { generateKeyPairSigner, createKeyPairSignerFromBytes, getBase58Codec } from '@solana/kit'
import { Mppx, solana } from '@solana/mpp/server'
import { paymentMiddleware } from 'x402-express'

const PORT = Number(process.env.PORT || 3402)
const NETWORK = process.env.NETWORK || 'localnet'
const RPC_URL = process.env.RPC_URL || 'http://localhost:8899'
const SECRET_KEY = process.env.SECRET_KEY || 'test-secret-key-for-dev'

// USDC mint (same address used on localnet via surfpool)
const USDC_MINT = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'

// ── Colors ──
const c = {
  reset:   '\x1b[0m',
  dim:     '\x1b[2m',
  bold:    '\x1b[1m',
  green:   '\x1b[32m',
  yellow:  '\x1b[33m',
  blue:    '\x1b[34m',
  magenta: '\x1b[35m',
  cyan:    '\x1b[36m',
  red:     '\x1b[31m',
  gray:    '\x1b[90m',
}

async function main() {
  // ── Keypair setup ──
  let feePayerSigner
  if (process.env.FEE_PAYER_KEY) {
    const bytes = getBase58Codec().encode(process.env.FEE_PAYER_KEY)
    feePayerSigner = await createKeyPairSignerFromBytes(bytes)
  } else {
    feePayerSigner = await generateKeyPairSigner()
  }

  const recipient = process.env.RECIPIENT || feePayerSigner.address
  console.log(`  ${c.dim}Recipient${c.reset}   ${c.cyan}${recipient}${c.reset}`)
  console.log(`  ${c.dim}Fee payer${c.reset}   ${c.cyan}${feePayerSigner.address}${c.reset}`)
  console.log(`  ${c.dim}Network${c.reset}     ${c.yellow}${NETWORK}${c.reset}`)

  // Bootstrap fee payer on localnet via surfpool cheatcodes
  if (NETWORK === 'localnet') {
    if (!await isSurfpoolRunning(RPC_URL)) {
      console.error()
      console.error(`  ${c.red}Could not connect to Surfpool at ${RPC_URL}${c.reset}`)
      console.error()
      console.error(`  ${c.dim}Install and start Surfpool:${c.reset}`)
      console.error()
      console.error(`  ${c.cyan}# Install Surfpool CLI${c.reset}`)
      console.error(`  ${c.bold}curl -sL https://run.surfpool.run/ | bash${c.reset}`)
      console.error()
      console.error(`  ${c.cyan}# Start local Solana network${c.reset}`)
      console.error(`  ${c.bold}surfpool start${c.reset}`)
      console.error()
      process.exit(1)
    }
    try {
      await bootstrap(feePayerSigner.address, RPC_URL)
    } catch (err) {
      console.error(`  ${c.red}Bootstrap failed: ${err}${c.reset}`)
      process.exit(1)
    }
  }

  const app = express()
  app.use(express.json())
  app.use(cors({
    exposedHeaders: [
      'www-authenticate', 'payment-receipt',  // MPP
      'x-payment-required', 'x-payment-response',  // x402
    ],
  }))

  // ── Request logger (skip paid retries — the response logger handles those) ──
  app.use((req, _res, next) => {
    const isPaidRetry = !!req.header('X-PAYMENT') || req.header('Authorization')?.startsWith('Payment ')
    if (!isPaidRetry) {
      const method = `${c.bold}${req.method}${c.reset}`
      const path = `${c.cyan}${req.path}${c.reset}`
      console.log(`  ${c.gray}${timestamp()}${c.reset} ${method} ${path}`)
    }
    next()
  })

  // ── MPP endpoints ──
  const mppx = Mppx.create({
    secretKey: SECRET_KEY,
    methods: [solana.charge({
      recipient,
      network: NETWORK,
      signer: feePayerSigner,
      currency: USDC_MINT,
      decimals: 6,
    })],
  })

  app.get('/mpp/quote/:symbol', async (req, res) => {
    const result = await mppx.charge({
      amount: '10000',
      currency: USDC_MINT,
      description: `Stock quote: ${req.params.symbol}`,
    })(toWebRequest(req))

    if (result.status === 402) {
      const challenge = result.challenge as Response
      const body = await challenge.text()
      logMpp402(req, challenge)
      res.writeHead(challenge.status, Object.fromEntries(challenge.headers))
      res.end(body)
      return
    }

    const response = result.withReceipt(Response.json({
      symbol: req.params.symbol.toUpperCase(),
      price: (Math.random() * 500).toFixed(2),
      currency: 'USD',
      source: 'mpp-demo',
    })) as Response
    logMpp200(response)
    res.writeHead(response.status, Object.fromEntries(response.headers))
    res.end(await response.text())
  })

  app.get('/mpp/weather/:city', async (req, res) => {
    const result = await mppx.charge({
      amount: '5000',
      currency: USDC_MINT,
      description: `Weather: ${req.params.city}`,
    })(toWebRequest(req))

    if (result.status === 402) {
      const challenge = result.challenge as Response
      const body = await challenge.text()
      logMpp402(req, challenge)
      res.writeHead(challenge.status, Object.fromEntries(challenge.headers))
      res.end(body)
      return
    }

    const response = result.withReceipt(Response.json({
      city: req.params.city,
      temperature: Math.floor(Math.random() * 35) + 5,
      conditions: ['Sunny', 'Cloudy', 'Rainy', 'Windy'][Math.floor(Math.random() * 4)],
      source: 'mpp-demo',
    })) as Response
    logMpp200(response)
    res.writeHead(response.status, Object.fromEntries(response.headers))
    res.end(await response.text())
  })

  // ── x402 endpoints ──
  const facilitatorPort = PORT + 1
  await startLocalFacilitator(facilitatorPort, feePayerSigner.address, RPC_URL)

  const x402App = express.Router()

  // x402 response logger
  x402App.use((req, res, next) => {
    const origEnd = res.end.bind(res)
    res.end = function (...args: any[]) {
      if (res.statusCode === 402) {
        const hasPayment = !!req.header('X-PAYMENT')
        if (hasPayment) {
          console.log(`  ${c.gray}${timestamp()}${c.reset} ${c.red}402${c.reset} payment rejected ${c.dim}(x402)${c.reset}`)
        } else {
          console.log(`  ${c.gray}${timestamp()}${c.reset} ${c.yellow}402${c.reset} challenge sent ${c.dim}(x402)${c.reset}`)
        }
      } else if (res.statusCode === 200) {
        const txHeader = res.getHeader('x-payment-response') as string | undefined
        const sig = txHeader ? extractX402TxSig(txHeader) : undefined
        const link = sig ? `${c.dim}${surfpoolLink(sig)}${c.reset}` : ''
        console.log(`  ${c.gray}${timestamp()}${c.reset} ${c.green}200${c.reset} payment accepted ${c.dim}(x402)${c.reset} ${link}`)
      }
      return origEnd(...args)
    } as any
    next()
  })

  x402App.use(paymentMiddleware(
    recipient,
    {
      '/x402/joke': {
        price: '$0.001',
        network: 'solana-devnet' as any,
        config: { description: 'A random joke' },
      },
      '/x402/fact': {
        price: '$0.001',
        network: 'solana-devnet' as any,
        config: { description: 'A random fact' },
      },
    },
    { url: `http://localhost:${facilitatorPort}` },
  ))

  x402App.get('/x402/joke', (_req, res) => {
    const jokes = [
      "Why do programmers prefer dark mode? Because light attracts bugs.",
      "There are 10 types of people: those who understand binary and those who don't.",
      "A SQL query walks into a bar, sees two tables, and asks: 'Can I JOIN you?'",
    ]
    res.json({ joke: jokes[Math.floor(Math.random() * jokes.length)], source: 'x402-demo' })
  })

  x402App.get('/x402/fact', (_req, res) => {
    const facts = [
      "Honey never spoils. Archaeologists found 3000-year-old honey in Egyptian tombs.",
      "Octopuses have three hearts and blue blood.",
      "A group of flamingos is called a 'flamboyance'.",
    ]
    res.json({ fact: facts[Math.floor(Math.random() * facts.length)], source: 'x402-demo' })
  })

  app.use(x402App)

  // ── Health ──
  app.get('/health', (_req, res) => {
    res.json({ status: 'ok', network: NETWORK, recipient })
  })

  app.listen(PORT, () => {
    console.log()
    console.log(`  ${c.bold}Server${c.reset} running on ${c.cyan}http://localhost:${PORT}${c.reset}`)
    console.log()
    console.log(`  ${c.magenta}MPP${c.reset} endpoints ${c.dim}(www-authenticate)${c.reset}`)
    console.log(`    ${c.bold}GET${c.reset} /mpp/quote/:symbol   ${c.yellow}0.01 USDC${c.reset}`)
    console.log(`    ${c.bold}GET${c.reset} /mpp/weather/:city   ${c.yellow}0.005 USDC${c.reset}`)
    console.log()
    console.log(`  ${c.blue}x402${c.reset} endpoints ${c.dim}(X-PAYMENT-REQUIRED)${c.reset}`)
    console.log(`    ${c.bold}GET${c.reset} /x402/joke           ${c.yellow}$0.001${c.reset}`)
    console.log(`    ${c.bold}GET${c.reset} /x402/fact           ${c.yellow}$0.001${c.reset}`)
    console.log()
    console.log(`  ${c.dim}Free endpoints${c.reset}`)
    console.log(`    ${c.bold}GET${c.reset} /health`)
    console.log()
  })
}

// ── Helpers ──

async function isSurfpoolRunning(rpcUrl: string): Promise<boolean> {
  try {
    const res = await fetch(rpcUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'getHealth' }),
    })
    return res.ok
  } catch {
    return false
  }
}

function timestamp() {
  return new Date().toLocaleTimeString('en-US', { hour12: false })
}

function surfpoolLink(sig: string) {
  const url = `http://localhost:18488/?t=${sig}`
  // OSC 8 hyperlink: \e]8;;URL\e\\LABEL\e]8;;\e\\
  return `\x1b]8;;${url}\x1b\\inspect\x1b]8;;\x1b\\`
}

/// Extract tx signature from MPP payment-receipt header (base64 JSON with `reference` field).
function extractMppTxSig(receiptHeader: string): string | undefined {
  try {
    const decoded = JSON.parse(Buffer.from(receiptHeader, 'base64').toString())
    return decoded.reference
  } catch {
    return undefined
  }
}

/// Extract tx signature from x402 X-PAYMENT-RESPONSE header (base64 JSON with `transaction` field).
function extractX402TxSig(header: string): string | undefined {
  try {
    const decoded = JSON.parse(Buffer.from(header, 'base64').toString())
    return decoded.transaction
  } catch {
    return undefined
  }
}

function logMpp402(req: express.Request, challenge: Response) {
  const hasCredential = req.header('Authorization')?.startsWith('Payment ')
  if (hasCredential) {
    console.log(`  ${c.gray}${timestamp()}${c.reset} ${c.red}402${c.reset} payment rejected ${c.dim}(MPP)${c.reset}`)
  } else {
    console.log(`  ${c.gray}${timestamp()}${c.reset} ${c.yellow}402${c.reset} challenge sent ${c.dim}(MPP)${c.reset}`)
  }
}

function logMpp200(response: Response) {
  const receipt = response.headers.get('payment-receipt') || ''
  const sig = extractMppTxSig(receipt)
  const link = sig ? `${c.dim}${surfpoolLink(sig)}${c.reset}` : ''
  console.log(`  ${c.gray}${timestamp()}${c.reset} ${c.green}200${c.reset} payment accepted ${c.dim}(MPP)${c.reset} ${link}`)
}

function toWebRequest(req: express.Request): globalThis.Request {
  const protocol = req.protocol || 'http'
  const host = req.headers.host || 'localhost'
  const url = `${protocol}://${host}${req.originalUrl}`
  return new globalThis.Request(url, {
    method: req.method,
    headers: new Headers(req.headers as Record<string, string>),
    body: ['GET', 'HEAD'].includes(req.method) ? undefined : JSON.stringify(req.body),
  })
}

async function bootstrap(address: string, rpcUrl: string) {
  const rpc = (method: string, params: any[]) =>
    fetch(rpcUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
    }).then(r => r.json() as Promise<any>)

  // Fund fee payer with 100 SOL
  await rpc('surfnet_setAccount', [address, {
    lamports: 100_000_000_000,
    data: '',
    executable: false,
    owner: '11111111111111111111111111111111',
  }])
  console.log(`  ${c.green}+${c.reset} Funded fee payer with ${c.bold}100 SOL${c.reset}`)

  // Fund fee payer's USDC token account with 1000 USDC
  await rpc('surfnet_setTokenAccount', [address, USDC_MINT, {
    amount: 1_000_000_000, // 1000 USDC (6 decimals)
  }])
  console.log(`  ${c.green}+${c.reset} Funded fee payer with ${c.bold}1000 USDC${c.reset}`)
}

// ── Embedded x402 facilitator for localnet ──

async function startLocalFacilitator(port: number, feePayer: string, rpcUrl: string) {
  const facilitator = express()
  facilitator.use(express.json())

  facilitator.get('/supported', (_req, res) => {
    res.json({
      kinds: [{
        scheme: 'exact',
        network: 'solana-devnet',
        extra: { feePayer },
      }],
    })
  })

  facilitator.post('/verify', (req, res) => {
    const { paymentPayload } = req.body
    if (!paymentPayload?.payload) {
      return res.json({ isValid: false, invalidReason: 'Missing payload' })
    }
    res.json({
      isValid: true,
      payer: paymentPayload.payload.authorization?.from || 'unknown',
    })
  })

  facilitator.post('/settle', async (req, res) => {
    const { paymentPayload } = req.body
    try {
      const payload = paymentPayload?.payload
      if (!payload) {
        return res.json({ success: false, errorReason: 'Missing payload' })
      }

      if (payload.transaction) {
        const result = await fetch(rpcUrl, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            jsonrpc: '2.0', id: 1,
            method: 'sendTransaction',
            params: [payload.transaction, { encoding: 'base64', skipPreflight: true }],
          }),
        })
        const data = await result.json() as any
        if (data.error) {
          return res.json({ success: false, errorReason: data.error.message })
        }
        return res.json({ success: true, transaction: data.result })
      }

      return res.json({ success: true, transaction: 'local-facilitator-settled' })
    } catch (err: any) {
      return res.json({ success: false, errorReason: err.message })
    }
  })

  return new Promise<void>((resolve) => {
    facilitator.listen(port, () => {
      console.log(`  ${c.dim}Facilitator${c.reset}  ${c.cyan}http://localhost:${port}${c.reset}`)
      resolve()
    })
  })
}

main().catch(console.error)
