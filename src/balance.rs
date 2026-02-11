use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use std::sync::Arc;

pub struct BalanceSync {
    rpc: Arc<RpcClient>,
    pubkey: Pubkey,
}

impl BalanceSync {
    pub fn new(rpc: Arc<RpcClient>, pubkey: Pubkey) -> Self {
        Self { rpc, pubkey }
    }

    pub fn fetch_balance(&self) -> Result<u64> {
        Ok(self.rpc.get_balance(&self.pubkey)?)
    }
}
