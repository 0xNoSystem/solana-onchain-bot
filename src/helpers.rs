use crate::TimeFrame;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use solana_sdk::signature::{Keypair, read_keypair_file};
use std::path::Path;

#[inline]
pub fn get_time_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub fn get_time_now_and_candles_ago(candle_count: u64, tf: TimeFrame) -> (u64, u64) {
    let end = get_time_now();

    let interval = candle_count
        .checked_mul(tf.to_secs())
        .and_then(|s| s.checked_mul(1_000))
        .expect("interval overflowed");

    let start = end.saturating_sub(interval);

    (start, end)
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct TimeDelta(u64);

#[macro_export]
macro_rules! timedelta {
    ($tf:path, $count:expr) => {
        $crate::helpers::TimeDelta::from_tf($tf, $count)
    };
}

impl TimeDelta {
    pub(crate) const fn from_tf(tf: TimeFrame, count: u64) -> TimeDelta {
        let ms = tf
            .to_millis()
            .checked_mul(count)
            .expect("time delta overflow");
        TimeDelta(ms)
    }

    pub fn as_ms(&self) -> u64 {
        self.0
    }

    pub fn as_secs(&self) -> u64 {
        self.0 / 1000
    }
}

pub fn read_keypair_from_file(file_path: &str) -> Result<Keypair, Box<dyn std::error::Error>> {
    let keypair = read_keypair_file(Path::new(file_path))?;
    Ok(keypair)
}

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_account_decoder_client_types::UiAccountData;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::{pubkey::Pubkey, signature::Signer, transaction::VersionedTransaction};

pub enum Side {
    Buy,
    Sell,
}

const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

pub fn associated_token_address(owner: &Pubkey, mint: &Pubkey) -> Result<Pubkey> {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;
    associated_token_address_with_program(owner, mint, &token_program)
}

pub fn associated_token_address_with_program(
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<Pubkey> {
    let ata_program = Pubkey::from_str(ASSOCIATED_TOKEN_PROGRAM_ID)?;
    let (ata, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );
    Ok(ata)
}

pub fn token_balance(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64> {
    let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID)?;
    if let Some(amount) = token_balance_for_program(rpc, owner, mint, &token_program)? {
        return Ok(amount);
    }

    let token_2022_program = Pubkey::from_str(TOKEN_2022_PROGRAM_ID)?;
    if let Some(amount) = token_balance_for_program(rpc, owner, mint, &token_2022_program)? {
        return Ok(amount);
    }

    let amount = token_balance_from_owner(rpc, owner, mint)?;
    Ok(amount)
}

fn token_balance_for_program(
    rpc: &RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<Option<u64>> {
    let ata = associated_token_address_with_program(owner, mint, token_program)?;
    match rpc.get_token_account_balance(&ata) {
        Ok(balance) => {
            let amount: u64 = balance.amount.parse()?;
            Ok(Some(amount))
        }
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("could not find account") {
                Ok(None)
            } else {
                Err(anyhow!(err))
            }
        }
    }
}

fn token_balance_from_owner(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64> {
    let accounts = rpc.get_token_accounts_by_owner(owner, TokenAccountsFilter::Mint(*mint))?;
    let mut total: u64 = 0;

    for keyed in accounts {
        if let UiAccountData::Json(parsed) = keyed.account.data {
            if let Some(info) = parsed.parsed.get("info").and_then(|v| v.as_object()) {
                if let Some(amount) = info
                    .get("tokenAmount")
                    .and_then(|v| v.as_object())
                    .and_then(|obj| obj.get("amount"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    total = total.saturating_add(amount);
                }
            }
        }
    }

    if total == 0 {
        return Err(anyhow!("token account not found or zero balance"));
    }

    Ok(total)
}

pub async fn buy_token(
    rpc: &RpcClient,
    wallet: &Keypair,
    base_mint: Pubkey,
    asset_mint: Pubkey,
    amount: u64,
    slippage_bps: u16,
) -> Result<(String, bool)> {
    swap_and_send_jupiter(
        rpc,
        wallet,
        base_mint,
        asset_mint,
        amount,
        slippage_bps,
        Side::Buy,
    )
    .await
}

pub async fn sell_token(
    rpc: &RpcClient,
    wallet: &Keypair,
    base_mint: Pubkey,
    asset_mint: Pubkey,
    amount: u64,
    slippage_bps: u16,
) -> Result<(String, bool)> {
    swap_and_send_jupiter(
        rpc,
        wallet,
        base_mint,
        asset_mint,
        amount,
        slippage_bps,
        Side::Sell,
    )
    .await
}

pub async fn sell_token_all(
    rpc: &RpcClient,
    wallet: &Keypair,
    base_mint: Pubkey,
    asset_mint: Pubkey,
    slippage_bps: u16,
) -> Result<(String, bool)> {
    let amount = token_balance(rpc, &wallet.pubkey(), &asset_mint)?;
    if amount == 0 {
        return Err(anyhow!("token balance is zero"));
    }
    sell_token(rpc, wallet, base_mint, asset_mint, amount, slippage_bps).await
}

pub async fn swap_and_send_jupiter(
    rpc: &RpcClient,
    wallet: &Keypair,
    base_mint: Pubkey,
    asset_mint: Pubkey,
    amount: u64,
    slippage_bps: u16,
    side: Side,
) -> Result<(String, bool)> {
    let http = Client::new();

    // Choose direction
    let (input_mint, output_mint) = match side {
        Side::Buy => (base_mint, asset_mint),
        Side::Sell => (asset_mint, base_mint),
    };

    // 🚀 Quote request
    let quote_url = format!(
        "https://lite-api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
        input_mint, output_mint, amount, slippage_bps
    );

    let quote_resp = http.get(&quote_url).send().await?;
    let quote_status = quote_resp.status();
    let quote_body = quote_resp.text().await?;

    if !quote_status.is_success() {
        return Err(anyhow!("Jupiter quote HTTP {quote_status}: {quote_body}"));
    }

    let quote: serde_json::Value = serde_json::from_str(&quote_body)
        .map_err(|err| anyhow!("Jupiter quote parse error: {err}; body: {quote_body}"))?;

    // 🧱 Build swap tx via Jupiter
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct SwapRequest {
        quote_response: serde_json::Value,
        user_public_key: String,
        wrap_unwrap_sol: bool,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct SwapResponse {
        swap_transaction: String,
    }

    let swap_resp = http
        .post("https://lite-api.jup.ag/swap/v1/swap")
        .json(&SwapRequest {
            quote_response: quote,
            user_public_key: wallet.pubkey().to_string(),
            wrap_unwrap_sol: true,
        })
        .send()
        .await?;

    let swap_status = swap_resp.status();
    let swap_body = swap_resp.text().await?;

    if !swap_status.is_success() {
        return Err(anyhow!("Jupiter swap HTTP {swap_status}: {swap_body}"));
    }

    let swap_res: SwapResponse = serde_json::from_str(&swap_body)
        .map_err(|err| anyhow!("Jupiter swap parse error: {err}; body: {swap_body}"))?;

    // 📦 Decode the returned base64 VersionedTransaction
    let tx_bytes = general_purpose::STANDARD.decode(swap_res.swap_transaction)?;
    let tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;

    // ✍️ Sign using wallet
    let tx = VersionedTransaction::try_new(tx.message, std::slice::from_ref(wallet))?;

    // 📤 Submit
    let sig = rpc.send_transaction(&tx)?;

    // Wait for confirmation
    rpc.confirm_transaction(&sig)?;

    // 🧾 Final status check
    let statuses = rpc.get_signature_statuses(&[sig])?.value;
    let success = statuses
        .get(0)
        .and_then(|status| status.as_ref())
        .map(|status| status.err.is_none())
        .unwrap_or(false);

    Ok((sig.to_string(), success))
}
