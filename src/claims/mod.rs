/* Module to handle indexing all WLD airdrop claim events */

use std::collections::BTreeMap;
use std::ops::DerefMut;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use ethers::abi::AbiDecode;
use ethers::middleware::Middleware;
use ethers::prelude::{Filter, H160, Selector, Transaction, U64, ValueOrArray};
use ethers::core::types::U256;
use futures::StreamExt;
use sea_orm::ActiveValue::Set;
use sea_orm::{Database, DatabaseConnection, EntityTrait};
use sea_orm::prelude::DateTime;
use futures::stream::{FuturesUnordered, iter};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::instrument;
use crate::abi::{ClaimCall, DeleteIdentitiesCall, DeleteIdentitiesWithDeletionProofAndBatchSizeAndPackedDeletionIndicesAndPreRootCall, GrantClaimedFilter, RegisterIdentitiesCall, TreeChangedFilter, TransferFilter};
use crate::entities::batches;
use crate::entities::prelude::{Batches, Deletions, Insertions};
use crate::tree::block_scanner::BlockScanner;
use crate::tree::error::{GrantClaimedError, TreeAvailabilityError};
use crate::tree::{Hash, SYNC_TO_HEAD_SLEEP_SECONDS};
use crate::tree::service::synced;
use crate::tree::tree_data::TreeData;
use crate::tree::tree_updater::{TreeUpdater, unpack_indices};

/// Manages the synchronization of the World Tree with it's onchain representation.
pub struct ClaimUpdater<M: Middleware> {
    /// Contract address of the `RecurringGrantDrop`.
    pub address: H160,
    /// Latest block that has been synced.
    pub latest_synced_block: AtomicU64,
    /// Scanner responsible for fetching logs and parsing calldata to decode tree updates.
    block_scanner: BlockScanner<Arc<M>>,
    /// Provider to interact with Ethereum.
    pub middleware: Arc<M>,
}

impl<M: Middleware> ClaimUpdater<M> {
    pub fn new(
        address: H160,
        creation_block: u64,
        window_size: u64,
        middleware: Arc<M>,
    ) -> Self {
        let filter = Filter::new()
            .address(address)
            .topic0(ValueOrArray::Value(TransferFilter::signature()));

        Self {
            address,
            latest_synced_block: AtomicU64::new(creation_block),
            block_scanner: BlockScanner::new(
                middleware.clone(),
                window_size,
                creation_block,
                filter,
            ),
            middleware,
        }
    }

    /// Steps through all the unsynced blocks and writes changed to database
    #[instrument(skip(self))]
    pub async fn sync_to_head(
        &self,
        db: &DatabaseConnection,
    ) -> Result<(), GrantClaimedError<M>> {
        tracing::info!("Syncing claims to chain head");

        let logs = self
            .block_scanner
            .next()
            .await
            .map_err(GrantClaimedError::MiddlewareError)?;

        if logs.is_empty() {
            tracing::info!("No `TreeChanged` events found within block range");
            return Ok(());
        }

        for log in logs {
            println!("Claimed {} WLD to {}", U256::decode(log.data).unwrap(), log.topics[1])
        }

        Ok(())
    }
}

pub struct ClaimStorage<M: Middleware> {
    pub claim_updater: Arc<ClaimUpdater<M>>,
}

impl<M: Middleware> ClaimStorage<M> {

    /// Spawns a task that continually syncs the `TreeData` to the state at the chain head.
    #[instrument(skip(self))]
    pub async fn spawn(&self) -> JoinHandle<Result<(), GrantClaimedError<M>>> {
        let claim_updater = self.claim_updater.clone();
        const DATABASE_URL: &str = env!("DATABASE_URL");
        let db = Database::connect(DATABASE_URL).await.unwrap();

        tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            claim_updater.sync_to_head(&db).await?;
            let sync_time = start.elapsed();

            tracing::info!(?sync_time, "ClaimUpdater synced to chain head");

            loop {
                claim_updater.sync_to_head(&db).await?;

                tokio::time::sleep(Duration::from_secs(
                    SYNC_TO_HEAD_SLEEP_SECONDS,
                ))
                    .await;
            }
        })
    }
}
