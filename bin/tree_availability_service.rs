use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use axum::http;

use clap::Parser;
use ethers::prelude::{JsonRpcError, RetryPolicy};
use common::shutdown_tracer_provider;
use ethers::providers::{Http, Provider, RetryClientBuilder};
use ethers_throttle::ThrottledProvider;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use governor::Jitter;
use serde::Deserialize;
use world_tree::tree::config::ServiceConfig;
use world_tree::tree::service::TreeAvailabilityService;
use ethers::providers::HttpClientError;
/// This service syncs the state of the World Tree and spawns a server that can deliver inclusion proofs for a given identity.
#[derive(Parser, Debug)]
#[clap(name = "Tree Availability Service")]
#[clap(version)]
struct Opts {
    /// Path to the configuration file
    #[clap(short, long)]
    config: Option<PathBuf>,

    /// Enable datadog backend for instrumentation
    #[clap(long, env)]
    datadog: bool,
}

#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let config = ServiceConfig::load(Some(Path::new("/home/atris/world-tree/default_config.json")))?;

    // construct a subscriber that prints formatted traces to stdout
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    let http_provider = Http::new(config.provider.rpc_endpoint);

    let throttled_http_provider = ThrottledProvider::new(
        http_provider,
        config.provider.throttle.unwrap_or(u32::MAX),
        Some(Jitter::new(
            Duration::from_millis(50),
            Duration::from_millis(5_000),
        )),
    );
    let retry_provider = RetryClientBuilder::default()
        .rate_limit_retries(10)
        .timeout_retries(3)
        .initial_backoff(Duration::from_millis(500))
        .build(throttled_http_provider, Box::from(CustomRetryPolicy));

    let middleware = Arc::new(Provider::new(retry_provider));

    let handles = TreeAvailabilityService::new(
        config.world_tree.tree_depth,
        config.world_tree.dense_prefix_depth,
        config.world_tree.tree_history_size,
        config.world_tree.world_id_contract_address,
        config.world_tree.creation_block,
        config.world_tree.window_size,
        middleware,
    )
        .serve(config.world_tree.socket_address);

    let mut handles = handles.into_iter().collect::<FuturesUnordered<_>>();
    while let Some(result) = handles.next().await {
        tracing::error!("TreeAvailabilityError: {:?}", result);
        result??;
    }

    shutdown_tracer_provider();

    Ok(())
}


/// Implements [RetryPolicy] that will retry requests that errored with
/// status code 429 i.e. TOO_MANY_REQUESTS
///
/// Infura often fails with a `"header not found"` rpc error which is apparently linked to load
/// balancing, which are retried as well.
#[derive(Debug, Default)]
pub struct CustomRetryPolicy;

impl RetryPolicy<HttpClientError> for CustomRetryPolicy {
    fn should_retry(&self, error: &HttpClientError) -> bool {
        fn should_retry_json_rpc_error(err: &JsonRpcError) -> bool {
            let JsonRpcError { code, message, .. } = err;
            // alchemy throws it this way
            if *code == 429 {
                return true
            }

            if *code == -32603 {
                return true
            }

            // This is an infura error code for `exceeded project rate limit`
            if *code == -32005 {
                return true
            }

            // alternative alchemy error for specific IPs
            if *code == -32016 && message.contains("rate limit") {
                return true
            }

            match message.as_str() {
                // this is commonly thrown by infura and is apparently a load balancer issue, see also <https://github.com/MetaMask/metamask-extension/issues/7234>
                "header not found" => true,
                // also thrown by infura if out of budget for the day and ratelimited
                "daily request count exceeded, request rate limited" => true,
                _ => false,
            }
        }

        match error {
            HttpClientError::ReqwestError(err) => {
                err.status() == Some(http::StatusCode::TOO_MANY_REQUESTS)
            }
            HttpClientError::JsonRpcError(err) => should_retry_json_rpc_error(err),
            HttpClientError::SerdeJson { text, .. } => {
                // some providers send invalid JSON RPC in the error case (no `id:u64`), but the
                // text should be a `JsonRpcError`
                #[derive(Deserialize)]
                struct Resp {
                    error: JsonRpcError,
                }

                if let Ok(resp) = serde_json::from_str::<Resp>(text) {
                    return should_retry_json_rpc_error(&resp.error)
                }
                false
            }
        }
    }

    fn backoff_hint(&self, error: &HttpClientError) -> Option<Duration> {
        if let HttpClientError::JsonRpcError(JsonRpcError { data, .. }) = error {
            let data = data.as_ref()?;

            // if daily rate limit exceeded, infura returns the requested backoff in the error
            // response
            let backoff_seconds = &data["rate"]["backoff_seconds"];
            // infura rate limit error
            if let Some(seconds) = backoff_seconds.as_u64() {
                return Some(Duration::from_secs(seconds))
            }
            if let Some(seconds) = backoff_seconds.as_f64() {
                return Some(Duration::from_secs(seconds as u64 + 1))
            }
        }

        None
    }
}
