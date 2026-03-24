fn main() {
    let rpc = pay_core::balance::mainnet_rpc_url();
    let pk = "Fd5DXRtiJPnc3wDJ8kL1n1BHVnP92XzKr84cdcTyNnx7";

    println!("RPC: {rpc}");
    match pay_core::balance::get_balances(&rpc, pk) {
        Ok(b) => {
            println!("SOL: {} lamports", b.sol_lamports);
            println!("Tokens: {}", b.tokens.len());
            for t in &b.tokens {
                println!("  {} = {} ({:?})", t.mint, t.ui_amount, t.symbol);
            }
        }
        Err(e) => println!("ERROR: {e:?}"),
    }
}
