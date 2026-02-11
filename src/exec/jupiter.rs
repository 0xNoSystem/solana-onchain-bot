use std::sync::Arc;

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};

#[derive(Clone, Debug)]
pub struct SwapSubmission {
    pub signature: Signature,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub input_amount: u64,
    pub expected_out_amount: Option<u64>,
}

pub struct JupiterClient {
    rpc: Arc<RpcClient>,
    http: Client,
    wallet: Arc<Keypair>,
    slippage_bps: u16,
}

impl JupiterClient {
    pub fn new(rpc: Arc<RpcClient>, wallet: Arc<Keypair>, slippage_bps: u16) -> Self {
        Self {
            rpc,
            http: Client::new(),
            wallet,
            slippage_bps,
        }
    }

    pub fn rpc(&self) -> &Arc<RpcClient> {
        &self.rpc
    }

    pub fn wallet_pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    pub async fn swap(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
        amount: u64,
    ) -> Result<SwapSubmission> {
        let quote_url = format!(
            "https://lite-api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            input_mint, output_mint, amount, self.slippage_bps,
        );

        let quote_resp = self.http.get(&quote_url).send().await?;
        let quote_status = quote_resp.status();
        let quote_body = quote_resp.text().await?;
        if !quote_status.is_success() {
            return Err(anyhow!("Jupiter quote HTTP {quote_status}: {quote_body}"));
        }

        let quote: serde_json::Value = serde_json::from_str(&quote_body)
            .map_err(|err| anyhow!("Jupiter quote parse error: {err}; body: {quote_body}"))?;

        let expected_out_amount = quote
            .get("outAmount")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok());

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

        let swap_resp = self
            .http
            .post("https://lite-api.jup.ag/swap/v1/swap")
            .json(&SwapRequest {
                quote_response: quote,
                user_public_key: self.wallet.pubkey().to_string(),
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

        let tx_bytes = general_purpose::STANDARD.decode(swap_res.swap_transaction)?;
        let tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
        let tx = VersionedTransaction::try_new(tx.message, std::slice::from_ref(&*self.wallet))?;

        let sig = self.rpc.send_transaction(&tx)?;

        Ok(SwapSubmission {
            signature: sig,
            input_mint,
            output_mint,
            input_amount: amount,
            expected_out_amount,
        })
    }
}
