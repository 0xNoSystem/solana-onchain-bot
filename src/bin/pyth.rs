use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const DEFAULT_HERMES_URL: &str = "https://hermes.pyth.network";
const DEFAULT_SOL_USD_FEED_ID: &str =
    "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
const DEFAULT_MAX_UPDATES: usize = 0; // 0 means unlimited
const DEFAULT_MAX_RETRIES: usize = 5;
const RETRY_DELAY_SECS: u64 = 2;

#[derive(Debug, Clone)]
struct Config {
    hermes_url: String,
    feed_id: String,
    max_updates: usize,
    max_retries: usize,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(default)]
    parsed: Vec<ParsedPriceFeed>,
}

#[derive(Debug, Deserialize)]
struct ParsedPriceFeed {
    id: String,
    price: PriceData,
}

#[derive(Debug, Deserialize)]
struct PriceData {
    price: String,
    conf: String,
    expo: i32,
    publish_time: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = load_config();
    log::info!(
        "starting pyth stream: hermes={} feed_id={} max_updates={} max_retries={}",
        cfg.hermes_url,
        cfg.feed_id,
        cfg.max_updates,
        cfg.max_retries
    );

    stream_loop(cfg).await
}

fn load_config() -> Config {
    let hermes_url =
        std::env::var("PYTH_HERMES_URL").unwrap_or_else(|_| DEFAULT_HERMES_URL.to_string());
    let feed_id =
        std::env::var("PYTH_FEED_ID").unwrap_or_else(|_| DEFAULT_SOL_USD_FEED_ID.to_string());
    let max_updates = std::env::var("PYTH_MAX_UPDATES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_UPDATES);
    let max_retries = std::env::var("PYTH_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_RETRIES);

    Config {
        hermes_url: hermes_url.trim_end_matches('/').to_string(),
        feed_id: normalize_feed_id(feed_id),
        max_updates,
        max_retries,
    }
}

fn normalize_feed_id(feed_id: String) -> String {
    let trimmed = feed_id.trim().to_string();
    if trimmed.starts_with("0x") {
        trimmed
    } else {
        format!("0x{trimmed}")
    }
}

async fn stream_loop(cfg: Config) -> Result<()> {
    let client = reqwest::Client::builder().build()?;

    let stream_url = format!(
        "{}/v2/updates/price/stream?ids[]={}&parsed=true",
        cfg.hermes_url, cfg.feed_id
    );

    let mut retries = 0usize;
    let mut updates = 0usize;

    loop {
        let response = match client.get(&stream_url).send().await {
            Ok(resp) => match resp.error_for_status() {
                Ok(resp) => resp,
                Err(err) => {
                    retries += 1;
                    if retries > cfg.max_retries {
                        return Err(anyhow!("stream HTTP error after retries: {}", err));
                    }
                    log::warn!(
                        "stream HTTP error (attempt {}/{}): {}",
                        retries,
                        cfg.max_retries,
                        err
                    );
                    tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                    continue;
                }
            },
            Err(err) => {
                retries += 1;
                if retries > cfg.max_retries {
                    return Err(anyhow!("stream connect failed after retries: {}", err));
                }
                log::warn!(
                    "stream connect failed (attempt {}/{}): {}",
                    retries,
                    cfg.max_retries,
                    err
                );
                tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
                continue;
            }
        };

        retries = 0;
        log::info!("connected to {}", stream_url);

        if read_stream(response, &mut updates, cfg.max_updates)
            .await?
            .is_some()
        {
            log::info!("max updates reached, exiting");
            return Ok(());
        }

        log::warn!("stream disconnected, reconnecting");
        tokio::time::sleep(Duration::from_secs(RETRY_DELAY_SECS)).await;
    }
}

async fn read_stream(
    mut response: reqwest::Response,
    updates: &mut usize,
    max_updates: usize,
) -> Result<Option<usize>> {
    let mut buffer = String::new();

    while let Some(chunk) = response.chunk().await? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        buffer = buffer.replace('\r', "");

        while let Some(split_idx) = buffer.find("\n\n") {
            let raw_event = buffer[..split_idx].to_string();
            buffer = buffer[split_idx + 2..].to_string();

            if let Some(event) = parse_sse_event(&raw_event)? {
                for parsed in event.parsed {
                    print_parsed_update(&parsed);
                    *updates += 1;
                    if max_updates > 0 && *updates >= max_updates {
                        return Ok(Some(*updates));
                    }
                }
            }
        }
    }

    Ok(None)
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

    let event = serde_json::from_str::<StreamEvent>(&payload).map_err(|err| {
        anyhow!(
            "failed to parse SSE data payload: {}; payload={}",
            err,
            payload
        )
    })?;
    Ok(Some(event))
}

fn print_parsed_update(parsed: &ParsedPriceFeed) {
    let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let publish = format_publish_time(parsed.price.publish_time);
    let price = scale_i64_string(&parsed.price.price, parsed.price.expo)
        .map(|v| format!("{v:.8}"))
        .unwrap_or_else(|| "--".to_string());
    let conf = scale_i64_string(&parsed.price.conf, parsed.price.expo)
        .map(|v| format!("{v:.8}"))
        .unwrap_or_else(|| "--".to_string());

    println!(
        "[{}] feed={} price={} conf={} expo={} publish={}",
        now, parsed.id, price, conf, parsed.price.expo, publish
    );
}

fn scale_i64_string(raw: &str, expo: i32) -> Option<f64> {
    let base = raw.parse::<f64>().ok()?;
    Some(base * 10f64.powi(expo))
}

fn format_publish_time(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "--".to_string())
}
