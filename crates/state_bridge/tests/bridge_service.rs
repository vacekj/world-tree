pub use std::time::Duration;

pub use ethers::abi::{AbiEncode, Address};
pub use ethers::core::abi::Abi;
pub use ethers::core::k256::ecdsa::SigningKey;
pub use ethers::core::rand;
pub use ethers::prelude::{
    ContractFactory, Http, LocalWallet, NonceManagerMiddleware, Provider,
    Signer, SignerMiddleware, Wallet,
};
pub use ethers::providers::Middleware;
pub use ethers::types::{Bytes, H256, U256};
pub use ethers::utils::{Anvil, AnvilInstance};
pub use ethers_solc::artifacts::Bytecode;
use rand::rngs::mock;
pub use semaphore::identity::Identity;
pub use semaphore::merkle_tree::{self, Branch};
pub use semaphore::poseidon_tree::{PoseidonHash, PoseidonTree};
pub use semaphore::protocol::{self, generate_nullifier_hash, generate_proof};
pub use semaphore::{hash_to_field, Field};
pub use serde::{Deserialize, Serialize};
pub use serde_json::json;
pub use tokio::spawn;
pub use tokio::task::JoinHandle;
pub use tracing::{error, info, instrument};

use state_bridge::bridge::{
    bridged_world_id, BridgedWorldID, IStateBridge, StateBridge,
};
use state_bridge::root::IWorldIdIdentityManager;
use state_bridge::StateBridgeService;
use std::str::FromStr;

use test_common::chain_mock::{spawn_mock_chain, MockChain};

#[derive(Deserialize, Serialize, Debug)]
struct CompiledContract {
    abi: Abi,
    bytecode: Bytecode,
}

#[tokio::test]
pub async fn test_relay_root() -> eyre::Result<()> {
    let MockChain {
        state_bridge,
        mock_bridged_world_id,
        mock_world_id,
        middleware,
        ..
    } = spawn_mock_chain().await?;

    let relaying_period = std::time::Duration::from_secs(5);

    let world_id = IWorldIdIdentityManager::new(
        mock_world_id.address(),
        middleware.clone(),
    );

    state_bridge.propagate_root().send().await?.await?;

    let state_bridge_address = state_bridge.address();

    let bridged_world_id_address = mock_bridged_world_id.address();

    let mut state_bridge_service = StateBridgeService::new(world_id)
        .await
        .expect("couldn't create StateBridgeService");

    let state_bridge =
        IStateBridge::new(state_bridge_address, middleware.clone());

    let bridged_world_id =
        BridgedWorldID::new(bridged_world_id_address, middleware.clone());

    let state_bridge =
        StateBridge::new(state_bridge, bridged_world_id, relaying_period)
            .unwrap();

    state_bridge_service.add_state_bridge(state_bridge);

    state_bridge_service
        .spawn()
        .await
        .expect("failed to spawn a state bridge service");

    let latest_root =
        U256::from_str("0x12312321321").expect("couldn't parse hexstring");

    mock_world_id.insert_root(latest_root).send().await?.await?;

    let mut bridged_world_id_root =
        mock_bridged_world_id.latest_root().call().await?;
    // in a loop:
    for _ in 0..20 {
        if latest_root != bridged_world_id_root {
            tokio::time::sleep(relaying_period / 10).await;
            bridged_world_id_root =
                mock_bridged_world_id.latest_root().call().await?;
        } else {
            break;
        }
    }

    assert_eq!(latest_root, bridged_world_id_root);

    Ok(())
}
