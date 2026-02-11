use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::native_token::LAMPORTS_PER_SOL;

#[derive(Clone, Copy, Debug)]
pub enum MarginAllocation {
    Alloc(f64), // percentage of free margin, from 0.0 to 100.0
    AmountSol(f64),
    AmountLamports(u64),
}

pub type AssetMargin = (Pubkey, u64);

pub struct MarginBook {
    rpc: Arc<RpcClient>,
    wallet_pubkey: Pubkey,
    map: HashMap<Pubkey, u64>,
    pub total_on_chain_lamports: u64,
}

impl MarginBook {
    pub fn new(rpc: Arc<RpcClient>, wallet_pubkey: Pubkey) -> Self {
        Self {
            rpc,
            wallet_pubkey,
            map: HashMap::new(),
            total_on_chain_lamports: 0,
        }
    }

    pub fn sync(&mut self) -> Result<u64> {
        let total = self.rpc.get_balance(&self.wallet_pubkey)?;
        self.total_on_chain_lamports = total;
        Ok(total)
    }

    pub fn allocate(&mut self, asset: Pubkey, alloc: MarginAllocation) -> Result<u64> {
        self.sync()?;
        let free = self.free();

        let requested = match alloc {
            MarginAllocation::Alloc(percent) => {
                if !(0.0 < percent && percent <= 100.0) {
                    return Err(anyhow!(
                        "invalid allocation percent: {} (expected 0.0 < x <= 100.0)",
                        percent
                    ));
                }
                ((free as f64) * (percent / 100.0)).floor() as u64
            }
            MarginAllocation::AmountSol(sol) => {
                if sol <= 0.0 {
                    return Err(anyhow!("invalid SOL amount: {}", sol));
                }
                (sol * LAMPORTS_PER_SOL as f64).floor() as u64
            }
            MarginAllocation::AmountLamports(lamports) => lamports,
        };

        if requested == 0 {
            return Err(anyhow!("requested allocation rounded to zero"));
        }
        if requested > free {
            return Err(anyhow!(
                "insufficient free margin: requested={} free={}",
                requested,
                free
            ));
        }

        self.map.insert(asset, requested);
        Ok(requested)
    }

    pub fn update_asset(&mut self, update: AssetMargin) -> Result<u64> {
        let (asset, requested_margin) = update;

        let current = *self
            .map
            .get(&asset)
            .ok_or_else(|| anyhow!("market {} not found in margin book", asset))?;

        self.sync()?;
        let free_with_current = self.free().saturating_add(current);
        if requested_margin > free_with_current {
            return Err(anyhow!(
                "insufficient free margin: requested={} free_with_current={}",
                requested_margin,
                free_with_current
            ));
        }

        self.map.insert(asset, requested_margin);
        Ok(requested_margin)
    }

    pub fn set_asset(&mut self, asset: Pubkey, new_margin: u64) -> Result<u64> {
        let slot = self
            .map
            .get_mut(&asset)
            .ok_or_else(|| anyhow!("market {} not found in margin book", asset))?;
        *slot = new_margin;
        Ok(new_margin)
    }

    pub fn remove(&mut self, asset: &Pubkey) {
        self.map.remove(asset);
    }

    pub fn used(&self) -> u64 {
        self.map.values().copied().sum()
    }

    pub fn free(&self) -> u64 {
        self.total_on_chain_lamports.saturating_sub(self.used())
    }

    pub fn reset(&mut self) {
        self.map.clear();
    }
}
