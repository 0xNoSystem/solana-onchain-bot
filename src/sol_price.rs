use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

const DEFAULT_HERMES_URL: &str = "https://hermes.pyth.network";
const DEFAULT_SOL_USD_FEED_ID: &str =
    "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
const DEFAULT_RETRY_DELAY_SECS: u64 = 2;

#[derive(Clone, Debug)]
pub struct SolPriceStreamConfig {
    pub hermes_url: String,
    pub feed_id: String,
    pub retry_delay: Duration,
}

impl SolPriceStreamConfig {
    pub fn from_env() -> Self {
        let hermes_url =
            std::env::var("PYTH_HERMES_URL").unwrap_or_else(|_| DEFAULT_HERMES_URL.to_string());
        let feed_id =
            std::env::var("PYTH_FEED_ID").unwrap_or_else(|_| DEFAULT_SOL_USD_FEED_ID.to_string());
        let retry_delay_secs = std::env::var("PYTH_RETRY_DELAY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_RETRY_DELAY_SECS);

        Self {
            hermes_url: hermes_url.trim_end_matches('/').to_string(),
            feed_id: normalize_feed_id(feed_id),
            retry_delay: Duration::from_secs(retry_delay_secs),
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(default)]
    parsed: Vec<ParsedPriceFeed>,
}

#[derive(Debug, Deserialize)]
struct ParsedPriceFeed {
    price: PriceData,
}

#[derive(Debug, Deserialize)]
struct PriceData {
    price: String,
    expo: i32,
}

pub fn spawn_sol_price_stream(
    cfg: SolPriceStreamConfig,
    tx: UnboundedSender<f64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        run_sol_price_stream(cfg, tx).await;
    })
}

fn normalize_feed_id(feed_id: String) -> String {
    let trimmed = feed_id.trim().to_string();
    if trimmed.starts_with("0x") {
        trimmed
    } else {
        format!("0x{trimmed}")
    }
}

async fn run_sol_price_stream(cfg: SolPriceStreamConfig, tx: UnboundedSender<f64>) {
    let client = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(err) => {
            log::warn!("failed to build sol price http client: {}", err);
            return;
        }
    };

    let stream_url = format!(
        "{}/v2/updates/price/stream?ids[]={}&parsed=true",
        cfg.hermes_url, cfg.feed_id
    );

    loop {
        let response = match client.get(&stream_url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => resp,
                Err(err) => {
                    log::warn!("sol price stream http error: {}", err);
                    tokio::time::sleep(cfg.retry_delay).await;
                    continue;
                }
            },
            Err(err) => {
                log::warn!("sol price stream connect failed: {}", err);
                tokio::time::sleep(cfg.retry_delay).await;
                continue;
            }
        };

        log::info!("sol price stream connected: {}", stream_url);

        if let Err(err) = read_stream(response, &tx).await {
            log::warn!("sol price stream read error: {}", err);
        }

        tokio::time::sleep(cfg.retry_delay).await;
    }
}

async fn read_stream(mut response: reqwest::Response, tx: &UnboundedSender<f64>) -> Result<()> {
    let mut buffer = String::new();

    while let Some(chunk) = response.chunk().await? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        buffer = buffer.replace('\r', "");

        while let Some(split_idx) = buffer.find("\n\n") {
            let raw_event = buffer[..split_idx].to_string();
            buffer = buffer[split_idx + 2..].to_string();

            let Some(event) = parse_sse_event(&raw_event)? else {
                continue;
            };

            for parsed in event.parsed {
                let Some(price) = scale_i64_string(&parsed.price.price, parsed.price.expo) else {
                    continue;
                };
                if !price.is_finite() || price <= 0.0 {
                    continue;
                }
                if tx.send(price).is_err() {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

fn parse_sse_event(raw_event: &str) -> Result<Option<StreamEvent>> {
    let mut payload = String::new();
    for line in raw_event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            payload.push_str(rest.trim_start());
        }
    }

    if payload.is_empty() {
        return Ok(None);
    }

    let event = serde_json::from_str::<StreamEvent>(&payload)?;
    Ok(Some(event))
}

fn scale_i64_string(raw: &str, expo: i32) -> Option<f64> {
    let base = raw.parse::<f64>().ok()?;
    Some(base * 10f64.powi(expo))
}
