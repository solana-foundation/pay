import { NextApiRequest, NextApiResponse } from "next"
import { address, createSolanaRpc, getBase58Encoder } from "@solana/kit"
import type { Address } from "@solana/kit"
// NOTE: @metaplex-foundation/js still depends on @solana/web3.js v1 internally.
// We import it for NFT creation but keep our own code free of web3.js imports.
// The web3.js types are used only within the Metaplex interaction boundary.
import { Connection, Keypair, PublicKey, Transaction } from "@solana/web3.js"
import { getOrCreateAssociatedTokenAccount, createTransferCheckedInstruction, getMint } from "@solana/spl-token"
import { GuestIdentityDriver, keypairIdentity, Metaplex } from "@metaplex-foundation/js"

// Update these variables!
const METADATA_URI = "https://arweave.net/1am2-5vjzk639JPAL_FMkswJPfbxe38Ejrmh8CkaAu8"

const USDC_ADDRESS = new PublicKey("Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr")
const ENDPOINT = 'https://api.devnet.solana.com'
const NFT_NAME = "Golden Ticket"
const PRICE_USDC = 0.1

type InputData = {
  account: string,
}

type GetResponse = {
  label: string,
  icon: string,
}

export type PostResponse = {
  transaction: string,
  message: string,
}

export type PostError = {
  error: string
}

function get(res: NextApiResponse<GetResponse>) {
  res.status(200).json({
    label: "My Store",
    icon: "https://solana.com/src/img/branding/solanaLogoMark.svg",
  })
}

async function postImpl(accountAddress: string): Promise<PostResponse> {
  // Metaplex requires web3.js Connection and Keypair — use them within this boundary
  const connection = new Connection(ENDPOINT)
  const account = new PublicKey(accountAddress)

  const shopPrivateKey = process.env.SHOP_PRIVATE_KEY
  if (!shopPrivateKey) throw new Error('SHOP_PRIVATE_KEY not found')
  const shopKeypair = Keypair.fromSecretKey(new Uint8Array(getBase58Encoder().encode(shopPrivateKey)))

  const metaplex = Metaplex
    .make(connection)
    .use(keypairIdentity(shopKeypair))

  const nfts = metaplex.nfts()
  const mintKeypair = Keypair.generate()

  const transactionBuilder = await nfts.builders().create({
    uri: METADATA_URI,
    name: NFT_NAME,
    tokenOwner: account,
    updateAuthority: shopKeypair,
    sellerFeeBasisPoints: 100,
    useNewMint: mintKeypair,
  })

  const fromUsdcAddress = await getOrCreateAssociatedTokenAccount(
    connection,
    shopKeypair,
    USDC_ADDRESS,
    account,
  )

  const toUsdcAddress = await getOrCreateAssociatedTokenAccount(
    connection,
    shopKeypair,
    USDC_ADDRESS,
    shopKeypair.publicKey,
  )

  const usdcMint = await getMint(connection, USDC_ADDRESS)
  const decimals = usdcMint.decimals

  const usdcTransferInstruction = createTransferCheckedInstruction(
    fromUsdcAddress.address,
    USDC_ADDRESS,
    toUsdcAddress.address,
    account,
    PRICE_USDC * (10 ** decimals),
    decimals
  )

  const identitySigner = new GuestIdentityDriver(account)

  transactionBuilder.prepend({
    instruction: usdcTransferInstruction,
    signers: [identitySigner]
  })

  const latestBlockhash = await connection.getLatestBlockhash()
  const transaction = await transactionBuilder.toTransaction(latestBlockhash)

  transaction.sign(shopKeypair, mintKeypair)

  const serializedTransaction = transaction.serialize({
    requireAllSignatures: false
  })
  const base64 = serializedTransaction.toString('base64')

  const message = "Please approve the transaction to mint your golden ticket!"

  return {
    transaction: base64,
    message,
  }
}

async function post(
  req: NextApiRequest,
  res: NextApiResponse<PostResponse | PostError>
) {
  const { account } = req.body as InputData
  console.log(req.body)
  if (!account) {
    res.status(400).json({ error: "No account provided" })
    return
  }

  try {
    const mintOutputData = await postImpl(account);
    res.status(200).json(mintOutputData)
    return
  } catch (error) {
    console.error(error);
    res.status(500).json({ error: 'error creating transaction' })
    return
  }
}

export default async function handler(
  req: NextApiRequest,
  res: NextApiResponse<GetResponse | PostResponse | PostError>
) {
  if (req.method === "GET") {
    return get(res)
  } else if (req.method === "POST") {
    return await post(req, res)
  } else {
    return res.status(405).json({ error: "Method not allowed" })
  }
}
