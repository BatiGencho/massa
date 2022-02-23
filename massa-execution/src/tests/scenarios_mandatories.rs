// Copyright (c) 2021 MASSA LABS <info@massa.net>

use crate::{
    settings::ExecutionConfigs, start_controller, ExecutionError, ExecutionSettings, SCELedger,
    SCELedgerEntry,
};
use massa_hash::hash::Hash;
use massa_models::{
    prehash::Map, Block, BlockHeader, BlockHeaderContent, BlockId, Operation, OperationContent,
    OperationType, SerializeCompact,
};
use massa_models::{Address, Amount, Slot};
use massa_signature::{
    derive_public_key, generate_random_private_key, sign, PrivateKey, PublicKey,
};
use massa_time::MassaTime;
use serial_test::serial;
use std::str::FromStr;
use tempfile::NamedTempFile;

/// generate a named temporary initial ledger file
pub fn generate_ledger_initial_file(values: &Map<Address, Amount>) -> NamedTempFile {
    use std::io::prelude::*;
    let file_named = NamedTempFile::new().expect("cannot create temp file");
    serde_json::to_writer_pretty(file_named.as_file(), &values)
        .expect("unable to write initial ledger file");
    file_named
        .as_file()
        .seek(std::io::SeekFrom::Start(0))
        .expect("could not seek file");
    file_named
}

/// Return a randomized address
pub fn get_random_address() -> Address {
    get_random_address_full().0
}

/// Same as `get_random_address()` and return priv_key and pub_key associated
/// to the address.
pub fn get_random_address_full() -> (Address, PrivateKey, PublicKey) {
    let priv_key = generate_random_private_key();
    let pub_key = derive_public_key(&priv_key);
    (Address::from_public_key(&pub_key), priv_key, pub_key)
}

fn get_sample_settings() -> (NamedTempFile, ExecutionConfigs) {
    let initial_file = generate_ledger_initial_file(
        &vec![
            (get_random_address(), Amount::from_str("14785.22").unwrap()),
            (get_random_address(), Amount::from_str("4778.1").unwrap()),
        ]
        .into_iter()
        .collect(),
    );
    let res = ExecutionConfigs {
        settings: ExecutionSettings {
            initial_sce_ledger_path: initial_file.path().into(),
            max_final_events: 200,
        },
        thread_count: 2,
        genesis_timestamp: MassaTime::now().unwrap(),
        t0: 16000.into(),
        clock_compensation: 0,
    };
    (initial_file, res)
}

fn get_sample_ledger() -> SCELedger {
    SCELedger(
        vec![
            (
                get_random_address(),
                SCELedgerEntry {
                    balance: Amount::from_str("129").unwrap(),
                    opt_module: None,
                    data: vec![
                        (
                            massa_hash::hash::Hash::compute_from("key_testA".as_bytes()),
                            "test1_data".into(),
                        ),
                        (
                            massa_hash::hash::Hash::compute_from("key_testB".as_bytes()),
                            "test2_data".into(),
                        ),
                        (
                            massa_hash::hash::Hash::compute_from("key_testC".as_bytes()),
                            "test3_data".into(),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            ),
            (
                get_random_address(),
                SCELedgerEntry {
                    balance: Amount::from_str("878").unwrap(),
                    opt_module: Some("bytecodebytecode".into()),
                    data: vec![
                        (
                            massa_hash::hash::Hash::compute_from("key_testD".as_bytes()),
                            "test4_data".into(),
                        ),
                        (
                            massa_hash::hash::Hash::compute_from("key_testE".as_bytes()),
                            "test5_data".into(),
                        ),
                    ]
                    .into_iter()
                    .collect(),
                },
            ),
        ]
        .into_iter()
        .collect(),
    )
}

#[tokio::test]
#[serial]
async fn test_execution_basic() {
    let (_config_file_keepalive, settings) = get_sample_settings();
    assert!(start_controller(settings, None).await.is_ok());
}

#[tokio::test]
#[serial]
async fn test_execution_shutdown() {
    let (_config_file_keepalive, settings) = get_sample_settings();
    let (_command_sender, _event_receiver, manager) = start_controller(settings, None)
        .await
        .expect("Failed to start execution.");
    manager.stop().await.expect("Failed to stop execution.");
}

#[tokio::test]
#[serial]
async fn test_sending_command() {
    let (_config_file_keepalive, settings) = get_sample_settings();
    let (command_sender, _event_receiver, manager) = start_controller(settings, None)
        .await
        .expect("Failed to start execution.");
    command_sender
        .update_blockclique(Default::default(), Default::default())
        .await
        .expect("Failed to send command");
    manager.stop().await.expect("Failed to stop execution.");
}

#[tokio::test]
#[serial]
async fn test_sending_read_only_execution_command() {
    let (_config_file_keepalive, settings) = get_sample_settings();
    let (command_sender, _event_receiver, manager) = start_controller(settings, None)
        .await
        .expect("Failed to start execution.");
    command_sender
        .execute_read_only_request(
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        )
        .await
        .expect("Failed to send command");
    manager.stop().await.expect("Failed to stop execution.");
}

#[tokio::test]
#[serial]
async fn test_execution_with_bootstrap() {
    let bootstrap_state = crate::BootstrapExecutionState {
        final_slot: Slot::new(12, 5),
        final_ledger: get_sample_ledger(),
    };
    let (_config_file_keepalive, settings) = get_sample_settings();
    let (command_sender, _event_receiver, manager) =
        start_controller(settings, Some(bootstrap_state))
            .await
            .expect("Failed to start execution.");
    command_sender
        .update_blockclique(Default::default(), Default::default())
        .await
        .expect("Failed to send command");
    manager.stop().await.expect("Failed to stop execution.");
}

#[tokio::test]
#[serial]
async fn generate_events() {
    // Compile the `./wasm_tests` and generate a block with `event_test.wasm`
    // as data. Then we check if we get an event as expected.

    let bootstrap_state = crate::BootstrapExecutionState {
        final_slot: Slot::new(0, 0),
        final_ledger: get_sample_ledger(),
    };
    let (_config_file_keepalive, settings) = get_sample_settings();
    let (command_sender, _event_receiver, manager) =
        start_controller(settings, Some(bootstrap_state))
            .await
            .expect("Failed to start execution.");

    let (sender_address, sender_private_key, sender_public_key) = get_random_address_full();
    let event_test_data = include_bytes!("./wasm_tests/build/event_test.wasm");
    let (block_id, block) = create_block(vec![create_execute_sc_operation(
        sender_private_key,
        sender_public_key,
        event_test_data,
    )
    .unwrap()])
    .unwrap();

    let mut finalized_blocks: Map<BlockId, Block> = Default::default();
    let blockclique: Map<BlockId, Block> = Default::default();
    let slot = block.header.content.slot;
    finalized_blocks.insert(block_id, block);
    command_sender
        .update_blockclique(finalized_blocks, blockclique)
        .await
        .expect("Failed to send command");

    let events = command_sender
        .get_filtered_sc_output_event(Some(slot), Some(slot), Some(sender_address), None, None)
        .await
        .unwrap();
    assert!(!events.is_empty(), "At least one event was expected");
    manager.stop().await.expect("Failed to stop execution.");
}

/// Create an operation for the given sender with `data` as bytecode.
/// Return a result that should be unwraped in the root `#[test]` routine.
fn create_execute_sc_operation(
    sender_private_key: PrivateKey,
    sender_public_key: PublicKey,
    data: &[u8],
) -> Result<Operation, ExecutionError> {
    let signature = sign(&Hash::compute_from("dummy".as_bytes()), &sender_private_key)?;
    let op = OperationType::ExecuteSC {
        data: data.to_vec(),
        max_gas: u64::MAX,
        coins: Amount::from_raw(u64::MAX),
        gas_price: Amount::from_str("1")?,
    };
    Ok(Operation {
        content: OperationContent {
            sender_public_key,
            fee: Amount::from_raw(0),
            expire_period: 10,
            op,
        },
        signature,
    })
}

/// Create an almost empty block with a vector `operations` and a random
/// creator.
///
/// Return a result that should be unwraped in the root `#[test]` routine.
fn create_block(operations: Vec<Operation>) -> Result<(BlockId, Block), ExecutionError> {
    let creator = generate_random_private_key();
    let public_key = derive_public_key(&creator);

    let operation_merkle_root = Hash::compute_from(
        &operations.iter().fold(Vec::new(), |acc, v| {
            [acc, v.to_bytes_compact().unwrap()].concat()
        })[..],
    );

    let (hash, header) = BlockHeader::new_signed(
        &creator,
        BlockHeaderContent {
            creator: public_key,
            slot: Slot {
                period: 0,
                thread: 1,
            },
            parents: vec![],
            operation_merkle_root,
            endorsements: vec![],
        },
    )?;

    let block = Block { header, operations };

    Ok((hash, block))
}
