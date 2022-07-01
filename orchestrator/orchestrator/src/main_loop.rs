//! This file contains the main loops for two distinct functions that just happen to reside int his same binary for ease of use. The Ethereum Signer and the Ethereum Oracle are both roles in Gravity
//! that can only be run by a validator. This single binary the 'Orchestrator' runs not only these two rules but also the untrusted role of a relayer, that does not need any permissions and has it's
//! own crate and binary so that anyone may run it.

use std::{cmp::min, time::Duration};

use cosmos_gravity::{
    query::{
        get_gravity_params, get_oldest_unsigned_logic_calls,
        get_oldest_unsigned_transaction_batches, get_oldest_unsigned_valsets,
    },
    send::{send_batch_confirm, send_logic_call_confirm, send_valset_confirms},
};
use futures::future::{try_join, try_join3};
use gravity_proto::{
    cosmos_sdk_proto::cosmos::base::abci::v1beta1::TxResponse,
    gravity::query_client::QueryClient as GravityQueryClient,
};
use gravity_utils::{
    clarity::{address::Address as EthAddress, u256, PrivateKey as EthPrivateKey, Uint256},
    deep_space::{
        address::Address as CosmosAddress, client::ChainStatus, coin::Coin, error::CosmosGrpcError,
        private_key::PrivateKey as CosmosPrivateKey, utils::FeeInfo, Contact,
    },
    error::GravityError,
    types::GravityBridgeToolsConfig,
    u64_array_bigints,
    web30::client::Web3,
};
use metrics_exporter::{metrics_errors_counter, metrics_latest, metrics_warnings_counter};
use relayer::main_loop::relayer_main_loop;
use tokio::time::sleep;
use tonic::transport::Channel;

use crate::{ethereum_event_watcher::check_for_events, oracle_resync::get_last_checked_block};

/// The execution speed governing all loops in this file
/// which is to say all loops started by Orchestrator main
/// loop except the relayer loop
pub const ETH_SIGNER_LOOP_SPEED: Duration = Duration::from_secs(11);
pub const ETH_ORACLE_LOOP_SPEED: Duration = Duration::from_secs(13);

/// This loop combines the three major roles required to make
/// up the 'Orchestrator', all three of these are async loops
/// meaning they will occupy the same thread, but since they do
/// very little actual cpu bound work and spend the vast majority
/// of all execution time sleeping this shouldn't be an issue at all.
#[allow(clippy::too_many_arguments)]
pub async fn orchestrator_main_loop(
    cosmos_key: CosmosPrivateKey,
    ethereum_key: EthPrivateKey,
    web3: Web3,
    contact: Contact,
    grpc_client: GravityQueryClient<Channel>,
    gravity_contract_address: EthAddress,
    gravity_id: String,
    user_fee_amount: Coin,
    config: GravityBridgeToolsConfig,
) -> Result<(), GravityError> {
    let fee = user_fee_amount;

    let a = eth_oracle_main_loop(
        cosmos_key,
        web3.clone(),
        contact.clone(),
        grpc_client.clone(),
        gravity_contract_address,
        fee.clone(),
    );

    let b = eth_signer_main_loop(
        cosmos_key,
        ethereum_key,
        contact.clone(),
        grpc_client.clone(),
        fee.clone(),
    );

    // TODO add additional arg param to override it
    // by default the address will be taken from the cosmos_key
    let reward_recipient: CosmosAddress = cosmos_key.to_address(&contact.get_prefix()).unwrap();

    let c = relayer_main_loop(
        ethereum_key,
        Some(cosmos_key),
        Some(fee),
        web3,
        contact,
        grpc_client.clone(),
        gravity_contract_address,
        gravity_id,
        &config.relayer,
        reward_recipient,
    );

    // if the relayer is not enabled we just don't start the future
    if config.orchestrator.relayer_enabled {
        if let Err(e) = try_join3(a, b, c).await {
            return Err(e);
        }
    } else if let Err(e) = try_join(a, b).await {
        return Err(e);
    }

    Ok(())
}

const DELAY: Duration = Duration::from_secs(5);

/// This function is responsible for making sure that Ethereum events are retrieved from the Ethereum blockchain
/// and ferried over to Cosmos where they will be used to issue tokens or process batches.
pub async fn eth_oracle_main_loop(
    cosmos_key: CosmosPrivateKey,
    web3: Web3,
    contact: Contact,
    grpc_client: GravityQueryClient<Channel>,
    gravity_contract_address: EthAddress,
    fee: Coin,
) -> Result<(), GravityError> {
    let our_cosmos_address = cosmos_key.to_address(&contact.get_prefix()).unwrap();
    let long_timeout_web30 = Web3::new(&web3.get_url(), Duration::from_secs(120));

    let mut last_checked_block: Uint256 = get_last_checked_block(
        grpc_client.clone(),
        our_cosmos_address,
        contact.get_prefix(),
        gravity_contract_address,
        &long_timeout_web30,
    )
    .await;

    // In case of governance vote to unhalt bridge, need to replay old events. Keep track of the
    // last checked event nonce to detect when this happens
    let mut last_checked_event = u256!(0);
    info!("Oracle resync complete, Oracle now operational");
    let mut grpc_client = grpc_client;

    loop {
        let _ = tokio::join!(
            async {
                let latest_eth_block = web3.eth_block_number().await;
                let latest_cosmos_block = contact.get_chain_status().await;

                match (latest_eth_block, latest_cosmos_block) {
                    (Ok(latest_eth_block), Ok(ChainStatus::Moving { block_height })) => {
                        trace!(
                            "Latest Eth block {} Latest Cosmos block {}",
                            latest_eth_block,
                            block_height,
                        );

                        metrics_latest(block_height, "latest_cosmos_block");
                        // Converting into u64
                        metrics_latest(latest_eth_block.resize_to_u64(), "latest_eth_block");
                    }
                    (Ok(_latest_eth_block), Ok(ChainStatus::Syncing)) => {
                        warn!("Cosmos node syncing, Eth oracle paused");
                        metrics_warnings_counter(2, "Cosmos node syncing");
                        sleep(DELAY).await;
                        return None;
                    }
                    (Ok(_latest_eth_block), Ok(ChainStatus::WaitingToStart)) => {
                        warn!("Cosmos node syncing waiting for chain start, Eth oracle paused");
                        metrics_warnings_counter(2, "Cosmos node syncing waiting for chain start");
                        sleep(DELAY).await;
                        return None;
                    }
                    (Ok(_), Err(_)) => {
                        warn!("Could not contact Cosmos grpc, trying again");
                        metrics_warnings_counter(2, "Could not contact Cosmos grpc");
                        sleep(DELAY).await;
                        return None;
                    }
                    (Err(_), Ok(_)) => {
                        warn!("Could not contact Eth node, trying again");
                        metrics_warnings_counter(1, "Could not contact Eth node");
                        sleep(DELAY).await;
                        return None;
                    }
                    (Err(_), Err(_)) => {
                        error!("Could not reach Ethereum or Cosmos rpc!");
                        metrics_errors_counter(0, "Could not reach Ethereum or Cosmos rpc");
                        sleep(DELAY).await;
                        return None;
                    }
                }

                // Relays events from Ethereum -> Cosmos
                match check_for_events(
                    &web3,
                    &contact,
                    &mut grpc_client,
                    gravity_contract_address,
                    cosmos_key,
                    fee.clone(),
                    last_checked_block,
                )
                .await
                {
                    Ok(nonces) => {
                        // this output CheckedNonces is accurate unless a governance vote happens
                        last_checked_block = nonces.block_number;
                        if last_checked_event > nonces.event_nonce {
                            // validator went back in history
                            info!(
                                "Governance unhalt vote must have happened, resetting the block to check!"
                            );
                            last_checked_block = get_last_checked_block(
                                grpc_client.clone(),
                                our_cosmos_address,
                                contact.get_prefix(),
                                gravity_contract_address,
                                &web3,
                            )
                            .await;
                        }
                        last_checked_event = nonces.event_nonce;
                        metrics_latest(last_checked_event.resize_to_u64(), "last_checked_event");
                    }
                    Err(e) => {
                        error!("Failed to get events for block range, Check your Eth node and Cosmos gRPC {:?}", e);
                        metrics_errors_counter(0, "Failed to get events for block range");
                    }
                }

                Some(())
            },
            tokio::time::sleep(ETH_SIGNER_LOOP_SPEED)
        );
    }
}

/// The eth_signer simply signs off on any batches or validator sets provided by the validator
/// since these are provided directly by a trusted Cosmsos node they can simply be assumed to be
/// valid and signed off on.
#[allow(clippy::too_many_arguments)]
pub async fn eth_signer_main_loop(
    cosmos_key: CosmosPrivateKey,
    ethereum_key: EthPrivateKey,
    contact: Contact,
    grpc_client: GravityQueryClient<Channel>,
    fee: Coin,
) -> Result<(), GravityError> {
    let our_cosmos_address = cosmos_key.to_address(&contact.get_prefix()).unwrap();
    let mut grpc_client = grpc_client;

    loop {
        let (async_result, _) = tokio::join!(
            async {
                // repeatedly refreshing the parameters here maintains loop correctness
                // if the gravity_id is changed or slashing windows are changed. Neither of these
                // is very probable
                let params = match get_gravity_params(&mut grpc_client).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to get Gravity parameters with {} correct your Cosmos gRPC connection immediately, you are risking slashing",e);
                        metrics_errors_counter(2, "Failed to get Gravity parameters correct your Cosmos gRPC connection immediately, you are risking slashing");
                        return Ok(());
                    }
                };
                let blocks_until_slashing = min(
                    min(params.signed_valsets_window, params.signed_batches_window),
                    params.signed_logic_calls_window,
                );
                let gravity_id = params.gravity_id;

                let latest_cosmos_block = contact.get_chain_status().await;
                match latest_cosmos_block {
                    Ok(ChainStatus::Moving { block_height }) => {
                        trace!("Latest Cosmos block {}", block_height,);
                    }
                    Ok(ChainStatus::Syncing) => {
                        warn!("Cosmos node syncing, Eth signer paused");
                        warn!("If this operation will take more than {} blocks of time you must find another node to submit signatures or risk slashing", blocks_until_slashing);
                        metrics_warnings_counter(2, "Cosmos node syncing, Eth signer paused");
                        metrics_latest(blocks_until_slashing, "blocks_until_slashing");
                        sleep(DELAY).await;
                        return Ok(());
                    }
                    Ok(ChainStatus::WaitingToStart) => {
                        warn!("Cosmos node syncing waiting for chain start, Eth signer paused");
                        metrics_warnings_counter(
                            2,
                            "Cosmos node syncing waiting for chain start, Eth signer paused",
                        );
                        sleep(DELAY).await;
                        return Ok(());
                    }
                    Err(_) => {
                        metrics_latest(blocks_until_slashing, "blocks_until_slashing");
                        metrics_errors_counter(
                            2,
                            "Could not reach Cosmos rpc! You must correct this or you risk being slashed",
                        );
                        return Ok(());
                    }
                }

                // sign the last unsigned valsets
                match get_oldest_unsigned_valsets(
                    &mut grpc_client,
                    our_cosmos_address,
                    contact.get_prefix(),
                )
                .await
                {
                    Ok(valsets) => {
                        if valsets.is_empty() {
                            trace!("No validator sets to sign, node is caught up!")
                        } else {
                            info!(
                                "Sending {} valset confirms starting with {}",
                                valsets.len(),
                                valsets[0].nonce
                            );
                            let res = send_valset_confirms(
                                &contact,
                                ethereum_key,
                                fee.clone(),
                                valsets,
                                cosmos_key,
                                gravity_id.clone(),
                            )
                            .await;
                            trace!("Valset confirm result is {:?}", res);
                            return check_for_fee_error(res, &fee);
                        }
                    }
                    Err(e) => trace!(
                        "Failed to get unsigned valsets, check your Cosmos gRPC {:?}",
                        e
                    ),
                }

                // sign the last unsigned batch, TODO check if we already have signed this
                match get_oldest_unsigned_transaction_batches(
                    &mut grpc_client,
                    our_cosmos_address,
                    contact.get_prefix(),
                )
                .await
                {
                    Ok(last_unsigned_batches) => {
                        if last_unsigned_batches.is_empty() {
                            trace!("No unsigned batch sets to sign, node is caught up!")
                        } else {
                            info!(
                                "Sending {} valset confirms starting with {}",
                                last_unsigned_batches.len(),
                                last_unsigned_batches[0].nonce
                            );

                            let res = send_batch_confirm(
                                &contact,
                                ethereum_key,
                                fee.clone(),
                                last_unsigned_batches,
                                cosmos_key,
                                gravity_id.clone(),
                            )
                            .await;
                            trace!("Batch confirm result is {:?}", res);
                            return check_for_fee_error(res, &fee);
                        }
                    }
                    Err(e) => trace!(
                        "Failed to get unsigned Batches, check your Cosmos gRPC {:?}",
                        e
                    ),
                }

                match get_oldest_unsigned_logic_calls(
                    &mut grpc_client,
                    our_cosmos_address,
                    contact.get_prefix(),
                )
                .await
                {
                    Ok(last_unsigned_calls) => {
                        if last_unsigned_calls.is_empty() {
                            trace!("No unsigned call sets to sign, node is caught up!")
                        } else {
                            info!(
                                "Sending {} valset confirms starting with {}",
                                last_unsigned_calls.len(),
                                last_unsigned_calls[0].invalidation_nonce
                            );
                            let res = send_logic_call_confirm(
                                &contact,
                                ethereum_key,
                                fee.clone(),
                                last_unsigned_calls,
                                cosmos_key,
                                gravity_id.clone(),
                            )
                            .await;
                            trace!("call confirm result is {:?}", res);
                            return check_for_fee_error(res, &fee);
                        }
                    }
                    Err(e) => info!(
                        "Failed to get unsigned Logic Calls, check your Cosmos gRPC {:?}",
                        e
                    ),
                }

                Ok(())
            },
            sleep(ETH_SIGNER_LOOP_SPEED)
        );

        if let Err(e) = async_result {
            return Err(e);
        }
    }
}

/// Checks for fee errors on our confirm submission transactions, a failure here
/// can be fatal and cause slashing so we want to warn the user and exit. There is
/// no point in running if we can't perform our most important function
fn check_for_fee_error(
    res: Result<TxResponse, CosmosGrpcError>,
    fee: &Coin,
) -> Result<(), GravityError> {
    if let Err(CosmosGrpcError::InsufficientFees { fee_info }) = res {
        match fee_info {
            FeeInfo::InsufficientFees { min_fees } => {
                return Err(GravityError::UnrecoverableError(
                    format!( "Your specified fee value {} is too small please use at least {} \n\
                    Correct fee argument immediately! You will be slashed within a few hours if you fail to do so",  fee, Coin::display_list(&min_fees)),
                ));
            }
            FeeInfo::InsufficientGas { .. } => {
                return Err(GravityError::UnrecoverableError(
                    "Hardcoded gas amounts insufficient!".into(),
                ));
            }
        }
    } else if res.is_err() {
        let error = res.err();
        error!("{:?}", error);
    }

    Ok(())
}
