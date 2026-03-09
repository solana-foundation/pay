import { NextApiRequest, NextApiResponse } from "next"
import {
  address,
  createKeyPairSignerFromBytes,
  createNoopSigner,
  createSolanaRpc,
  createTransactionMessage,
  generateKeyPairSigner,
  getBase58Encoder,
  getBase64EncodedWireTransaction,
  lamports,
  pipe,
  setTransactionMessageFeePayer,
  setTransactionMessageLifetimeUsingBlockhash,
  appendTransactionMessageInstructions,
  signTransaction,
  compileTransaction,
  type Address,
  type Instruction,
} from "@solana/kit"
import { getCreateAccountInstruction } from "@solana-program/system"
import {
  findAssociatedTokenPda,
  getTransferCheckedInstruction,
  fetchMint,
  TOKEN_PROGRAM_ADDRESS,
} from "@solana-program/token"
import {
  getMintSize,
  getInitializeMetadataPointerInstruction,
  getInitializeMint2Instruction,
  getInitializeTokenMetadataInstruction,
  getMintToInstruction,
  getCreateAssociatedTokenIdempotentInstructionAsync,
  TOKEN_2022_PROGRAM_ADDRESS,
  extension,
} from "@solana-program/token-2022"

// Update these variables!
const METADATA_URI = "https://arweave.net/1am2-5vjzk639JPAL_FMkswJPfbxe38Ejrmh8CkaAu8"

const USDC_ADDRESS = address("Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr")
const ENDPOINT = "https://api.devnet.solana.com"
const NFT_NAME = "Golden Ticket"
const NFT_SYMBOL = "TICKET"
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
  const rpc = createSolanaRpc(ENDPOINT)
  const buyerAddress = address(accountAddress)

  // Load the shop signer from env
  const shopPrivateKey = process.env.SHOP_PRIVATE_KEY
  if (!shopPrivateKey) throw new Error("SHOP_PRIVATE_KEY not found")
  const shopSigner = await createKeyPairSignerFromBytes(
    new Uint8Array(getBase58Encoder().encode(shopPrivateKey))
  )

  // Generate a fresh mint keypair for this NFT
  const mintSigner = await generateKeyPairSigner()

  const instructions: Instruction[] = []

  // 1. Create mint account with space for MetadataPointer + TokenMetadata extensions
  const mintSize = getMintSize([
    extension("MetadataPointer", { authority: shopSigner.address, metadataAddress: mintSigner.address }),
    extension("TokenMetadata", {
      updateAuthority: shopSigner.address,
      mint: mintSigner.address,
      name: NFT_NAME,
      symbol: NFT_SYMBOL,
      uri: METADATA_URI,
      additionalMetadata: new Map(),
    }),
  ])

  const rentLamports = await rpc.getMinimumBalanceForRentExemption(BigInt(mintSize)).send()

  instructions.push(
    getCreateAccountInstruction({
      payer: shopSigner,
      newAccount: mintSigner,
      lamports: lamports(rentLamports),
      space: mintSize,
      programAddress: TOKEN_2022_PROGRAM_ADDRESS,
    })
  )

  // 2. Initialize metadata pointer (self-referencing — metadata lives on the mint)
  instructions.push(
    getInitializeMetadataPointerInstruction({
      mint: mintSigner.address,
      authority: shopSigner.address,
      metadataAddress: mintSigner.address,
    })
  )

  // 3. Initialize mint (decimals: 0 for NFT)
  instructions.push(
    getInitializeMint2Instruction({
      mint: mintSigner.address,
      decimals: 0,
      mintAuthority: shopSigner.address,
    })
  )

  // 4. Initialize token metadata on the mint
  instructions.push(
    getInitializeTokenMetadataInstruction({
      metadata: mintSigner.address,
      updateAuthority: shopSigner.address,
      mint: mintSigner.address,
      mintAuthority: shopSigner,
      name: NFT_NAME,
      symbol: NFT_SYMBOL,
      uri: METADATA_URI,
    })
  )

  // 5. Create buyer's ATA for the NFT (Token-2022 program)
  const createAtaIx = await getCreateAssociatedTokenIdempotentInstructionAsync({
    payer: shopSigner,
    owner: buyerAddress,
    mint: mintSigner.address,
    tokenProgram: TOKEN_2022_PROGRAM_ADDRESS,
  })
  instructions.push(createAtaIx)

  // 6. Mint 1 NFT to the buyer's ATA
  const [buyerAta] = await findAssociatedTokenPda({
    owner: buyerAddress,
    mint: mintSigner.address,
    tokenProgram: TOKEN_2022_PROGRAM_ADDRESS,
  })
  instructions.push(
    getMintToInstruction({
      mint: mintSigner.address,
      token: buyerAta,
      mintAuthority: shopSigner,
      amount: 1n,
    }, { programAddress: TOKEN_2022_PROGRAM_ADDRESS })
  )

  // 7. USDC transfer: buyer → shop (buyer signs later)
  const buyerNoopSigner = createNoopSigner(buyerAddress)
  const usdcMint = await fetchMint(rpc, USDC_ADDRESS)
  const decimals = usdcMint.data.decimals
  const usdcAmount = BigInt(Math.round(PRICE_USDC * 10 ** decimals))

  const [buyerUsdcAta] = await findAssociatedTokenPda({
    owner: buyerAddress,
    tokenProgram: TOKEN_PROGRAM_ADDRESS,
    mint: USDC_ADDRESS,
  })
  const [shopUsdcAta] = await findAssociatedTokenPda({
    owner: shopSigner.address,
    tokenProgram: TOKEN_PROGRAM_ADDRESS,
    mint: USDC_ADDRESS,
  })

  instructions.push(
    getTransferCheckedInstruction({
      source: buyerUsdcAta,
      mint: USDC_ADDRESS,
      destination: shopUsdcAta,
      authority: buyerNoopSigner,
      amount: usdcAmount,
      decimals,
    })
  )

  // Build the transaction, partially sign with shop + mint
  const { value: latestBlockhash } = await rpc.getLatestBlockhash().send()

  const txMessage = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayer(shopSigner.address, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
    (m) => appendTransactionMessageInstructions(instructions, m),
  )

  const compiled = compileTransaction(txMessage)
  const signed = await signTransaction([shopSigner.keyPair, mintSigner.keyPair], compiled)
  const base64 = getBase64EncodedWireTransaction(signed)

  return {
    transaction: base64,
    message: "Please approve the transaction to mint your golden ticket!",
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
    res.status(500).json({ error: "error creating transaction" })
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
