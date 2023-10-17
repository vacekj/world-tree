use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use ethers::middleware::Middleware;
use ethers::{
    contract::Contract,
    providers::{MiddlewareError, StreamExt},
    types::{Filter, H160, U256},
};
use ruint::Uint;
use semaphore::{
    merkle_tree::Hasher,
    poseidon_tree::{PoseidonHash, Proof},
};

pub type Hash = <PoseidonHash as Hasher>::Hash;
use crate::error::StateBridgeError;
use ethers::prelude::abigen;
use tokio::{task::JoinHandle, time::Duration};

abigen!(
    IWorldIdIdentityManager,
    r#"[
        function latestRoot() external returns (uint256)
        event TreeChanged(uint256 indexed preRoot, uint8 indexed kind, uint256 indexed postRoot)
    ]"#;
);

pub struct WorldTreeRoot<M: Middleware + 'static> {
    pub world_id_identity_manager: IWorldIdIdentityManager<M>,
    pub root_tx: tokio::sync::broadcast::Sender<Hash>,
}

impl<M> WorldTreeRoot<M>
where
    M: Middleware,
{
    pub async fn new(
        world_id_identity_manager: IWorldIdIdentityManager<M>,
    ) -> Result<Self, StateBridgeError<M>> {
        let (root_tx, _) = tokio::sync::broadcast::channel::<Hash>(1000);

        Ok(Self {
            world_id_identity_manager,
            root_tx,
        })
    }

    pub async fn new_from_parts(
        world_tree_address: H160,
        middleware: Arc<M>,
    ) -> Result<Self, StateBridgeError<M>> {
        let (root_tx, _) = tokio::sync::broadcast::channel::<Hash>(1000);

        let world_id_identity_manager = IWorldIdIdentityManager::new(
            world_tree_address,
            middleware.clone(),
        );

        Ok(Self {
            world_id_identity_manager,
            root_tx,
        })
    }

    pub async fn spawn(&self) -> JoinHandle<Result<(), StateBridgeError<M>>> {
        let root_tx = self.root_tx.clone();
        let world_id_identity_manager = self.world_id_identity_manager.clone();

        tokio::spawn(async move {
            dbg!("Spawning root service");

            let filter = world_id_identity_manager.event::<TreeChangedFilter>();

            let mut event_stream = filter.stream().await?.with_meta();

            // Listen to a stream of events, when a new event is received, update the root and block number
            while let Some(Ok((event, _))) = event_stream.next().await {
                // Send it through the tx, you can convert ethers U256 to ruint with Uint::from_limbs()
                let _ = root_tx.send(Uint::from_limbs(event.post_root.0));
            }

            Ok(())
        })
    }
}

#[cfg(test)]

mod tests {
    use std::str::FromStr;

    use super::*;
    use test_common::chain_mock::{spawn_mock_chain, MockChain};

    #[tokio::test]
    async fn listen_and_propagate_root() -> eyre::Result<()> {
        let MockChain {
            mock_world_id,
            middleware,
            anvil,
            ..
        } = spawn_mock_chain().await?;

        let world_id = IWorldIdIdentityManager::new(
            mock_world_id.address(),
            middleware.clone(),
        );

        let tree_root = WorldTreeRoot::new(world_id).await?;

        tree_root.spawn().await;

        let test_root = U256::from_str("0x222").unwrap();

        mock_world_id.insert_root(test_root).send().await?.await?;

        let mut root_rx = tree_root.root_tx.subscribe();

        let relaying_period = Duration::from_secs(5);

        tokio::spawn(async move {
            loop {
                // Process all of the updates and get the latest root
                while let Ok(root) = root_rx.recv().await {
                    if root == Uint::from_limbs(test_root.0) {
                        break;
                    }
                    // Check if the latest root is different than on L2 and if so, update the root
                    tokio::time::sleep(relaying_period).await;
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        Ok(())
    }
}
