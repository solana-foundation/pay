import { bundlrStorage, keypairIdentity, Metaplex, toMetaplexFile } from "@metaplex-foundation/js"
import { clusterApiUrl, Connection, Keypair } from "@solana/web3.js"
import base58 from "bs58"
import * as dotenv from "dotenv"
import * as fs from "fs"
dotenv.config()

// update these variables!
// Connection endpoint, switch to a mainnet RPC if using mainnet
const ENDPOINT = clusterApiUrl('devnet')

// Devnet Bundlr address
const BUNDLR_ADDRESS = "https://devnet.bundlr.network"

// Mainnet Bundlr address, uncomment if using mainnet
// const BUNDLR_ADDRESS = "https://node1.bundlr.network"

// NFT metadata
const NFT_NAME = "Golden Ticket"
const NFT_SYMBOL = "GOLD"
const NFT_DESCRIPTION = "A golden ticket that grants access to loyalty rewards"
// Set this relative to the root directory
const NFT_IMAGE_PATH = "nft-upload/pay-logo.svg"
const NFT_FILE_NAME = "pay-logo.svg"


async function main() {
  // Get the shop keypair from the environment variable
  const shopPrivateKey = process.env.SHOP_PRIVATE_KEY
  if (!shopPrivateKey) throw new Error('SHOP_PRIVATE_KEY not found')
  const shopKeypair = Keypair.fromSecretKey(base58.decode(shopPrivateKey))

  const connection = new Connection(ENDPOINT)

  const nfts = Metaplex
    .make(connection, { cluster: 'devnet' })
    .use(keypairIdentity(shopKeypair))
    .use(bundlrStorage({
      address: BUNDLR_ADDRESS,
      providerUrl: ENDPOINT,
      timeout: 60000
    }))
    .nfts();

  const imageBuffer = fs.readFileSync(NFT_IMAGE_PATH)
  const file = toMetaplexFile(imageBuffer, NFT_FILE_NAME)

  const uploadedMetadata = await nfts.uploadMetadata({
    name: NFT_NAME,
    symbol: NFT_SYMBOL,
    description: NFT_DESCRIPTION,
    image: file,
  })

  console.log(`Uploaded metadata: ${uploadedMetadata.uri}`)
}

main()
  .then(() => {
    console.log("Done!")
  })
  .catch((err) => {
    console.error(err)
    process.exit(1)
  })
