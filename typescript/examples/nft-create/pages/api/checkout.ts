import { NextApiRequest, NextApiResponse } from "next"
import { clusterApiUrl, Connection, Keypair, PublicKey } from "@solana/web3.js"
import { getOrCreateAssociatedTokenAccount, createTransferCheckedInstruction, getMint } from "@solana/spl-token"
import { GuestIdentityDriver, keypairIdentity, Metaplex } from "@metaplex-foundation/js"
import base58 from 'bs58'

// Update these variables!
// This is returned by nft-upload/upload.js
const METADATA_URI = "https://arweave.net/1am2-5vjzk639JPAL_FMkswJPfbxe38Ejrmh8CkaAu8"

// Devnet 'fake' USDC, you can get these tokens from https://spl-token-faucet.com/
const USDC_ADDRESS = new PublicKey("Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr")

// Mainnet USDC, uncomment if using mainnet
// const USDC_ADDRESS = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")

// Connection endpoint, switch to a mainnet RPC if using mainnet
const ENDPOINT = clusterApiUrl('devnet')

// This is the name your created NFT will have. Other metadata comes from METADATA_URI
const NFT_NAME = "Golden Ticket"

// The amount to charge in USDC
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

async function postImpl(account: PublicKey): Promise<PostResponse> {
  const connection = new Connection(ENDPOINT)

  // Get the shop keypair from the environment variable
  const shopPrivateKey = process.env.SHOP_PRIVATE_KEY
  if (!shopPrivateKey) throw new Error('SHOP_PRIVATE_KEY not found')
  const shopKeypair = Keypair.fromSecretKey(base58.decode(shopPrivateKey))

  // Initialise Metaplex with our shop keypair
  const metaplex = Metaplex
    .make(connection)
    .use(keypairIdentity(shopKeypair))

  const nfts = metaplex.nfts()

  // The mint needs to sign the transaction, so we generate a new keypair for it
  const mintKeypair = Keypair.generate()

  // Create a transaction builder to create the NFT
  const transactionBuilder = await nfts.builders().create({
    uri: METADATA_URI, // use our metadata
    name: NFT_NAME,
    tokenOwner: account, // NFT is minted to the wallet submitting the transaction (buyer)
    updateAuthority: shopKeypair, // we retain update authority
    sellerFeeBasisPoints: 100, // 1% royalty
    useNewMint: mintKeypair, // we pass our mint in as the new mint to use
  })

  // Next we create an instruction to transfer USDC from the buyer to the shop
  // This will be added to the create NFT transaction

  // Get the buyer's USDC address
  const fromUsdcAddress = await getOrCreateAssociatedTokenAccount(
    connection,
    shopKeypair,
    USDC_ADDRESS,
    account,
  )

  // Get the shop's USDC address
  const toUsdcAddress = await getOrCreateAssociatedTokenAccount(
    connection,
    shopKeypair,
    USDC_ADDRESS,
    shopKeypair.publicKey,
  )

  const usdcMint = await getMint(connection, USDC_ADDRESS)
  const decimals = usdcMint.decimals

  const usdcTransferInstruction = createTransferCheckedInstruction(
    fromUsdcAddress.address, // from USDC address
    USDC_ADDRESS, // USDC mint address
    toUsdcAddress.address, // to USDC address
    account, // owner of the from USDC address (the buyer)
    PRICE_USDC * (10 ** decimals), // multiply by 10^decimals
    decimals
  )

  // Create a guest identity for buyer, so they will be a required signer for the transaction
  const identitySigner = new GuestIdentityDriver(account)

  // Add the USDC payment to the NFT transaction
  transactionBuilder.prepend({
    instruction: usdcTransferInstruction,
    signers: [identitySigner]
  })

  // transactionBuilder.setFeePayer(payerKeypair)

  // Convert to transaction
  const latestBlockhash = await connection.getLatestBlockhash()
  const transaction = await transactionBuilder.toTransaction(latestBlockhash)

  // Partially sign the transaction, as the shop and the mint
  // The account is also a required signer, but they'll sign it with their wallet after we return it
  transaction.sign(shopKeypair, mintKeypair)

  // Serialize the transaction and convert to base64 to return it
  const serializedTransaction = transaction.serialize({
    requireAllSignatures: false // account is a missing signature
  })
  const base64 = serializedTransaction.toString('base64')

  const message = "Please approve the transaction to mint your golden ticket!"

  // Return the serialized transaction
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
    const mintOutputData = await postImpl(new PublicKey(account));
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
