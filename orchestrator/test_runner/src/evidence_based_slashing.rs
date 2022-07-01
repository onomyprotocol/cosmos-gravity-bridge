//! This is a test for Evidence based slashing, we simply create a bad signature and submit it as evidence
//! we don't launch the orchestrators here as they are not required.

use cosmos_gravity::{send::submit_bad_signature_evidence, utils::BadSignatureEvidence};
use ethereum_gravity::{
    message_signatures::{encode_valset_confirm, encode_valset_confirm_hashed},
    utils::get_gravity_id,
};
use gravity_proto::cosmos_sdk_proto::cosmos::staking::v1beta1::QueryValidatorsRequest;
use gravity_utils::{
    clarity::{u256, utils::bytes_to_hex_str, Address as EthAddress},
    deep_space::{Coin, Contact, PrivateKey},
    types::{Valset, ValsetMember},
    u64_array_bigints,
    web30::client::Web3,
};

use crate::{
    get_fee,
    utils::{get_operator_address, ValidatorKeys},
    STAKING_TOKEN, STARTING_STAKE_PER_VALIDATOR, TOTAL_TIMEOUT,
};

pub async fn evidence_based_slashing(
    web30: &Web3,
    contact: &Contact,
    keys: Vec<ValidatorKeys>,
    gravity_address: EthAddress,
) {
    // first step in this test is to ensure that our slashing vitim does not
    // have 33% and therefore will not halt the chain when jailed. Since our
    // default validator set is 3 validators with 33% each we need to change it some.
    delegate_to_validator(&keys, keys[1].validator_key, contact).await;
    delegate_to_validator(&keys, keys[2].validator_key, contact).await;

    // our slashing victim is just the first validator
    let cosmos_private_key = keys[0].validator_key;
    let eth_private_key = keys[0].eth_key;
    let eth_addr = eth_private_key.to_address();
    // reporter is another validator using their delegate key
    let submitter_private_key = keys[1].orch_key;
    // this is a false valset, one that happens to contain only the
    // validator signing it, as if they where trying to take over the
    // bridge. This valset isn't valid for submitting but that's not a
    // condition of the slashing
    let false_valset = Valset {
        nonce: 500,
        members: vec![ValsetMember {
            power: 1337,
            eth_address: eth_addr,
        }],
        reward_amount: u256!(0),
        reward_denom: "".to_string(),
    };
    let gravity_id = get_gravity_id(gravity_address, eth_addr, web30)
        .await
        .unwrap();

    let message = encode_valset_confirm(gravity_id.clone(), &false_valset);
    let checkpoint = encode_valset_confirm_hashed(gravity_id.clone(), &false_valset);
    let eth_signature = eth_private_key.sign_ethereum_msg(&message);
    info!(
        "Created signature {} over checkpoint {} with Gravity ID {} using address {}",
        eth_signature,
        bytes_to_hex_str(&checkpoint),
        gravity_id,
        eth_addr
    );

    // now we are prepared to submit our evidence, we check first that validator 0 is in the set
    print_validator_status(contact).await;
    let (is_in_set, jailed) =
        check_validator(contact, cosmos_private_key, "BOND_STATUS_BONDED").await;
    assert!(is_in_set);
    assert!(!jailed);
    info!("Target validator is in the set and not jailed");

    info!("Submitting Evidence");
    // submit the evidence
    let res = submit_bad_signature_evidence(
        submitter_private_key,
        get_fee(),
        contact,
        BadSignatureEvidence::Valset(false_valset),
        eth_signature,
    )
    .await
    .unwrap();
    trace!("{:?}", res);

    // confirm that the validator for which the evidence has been submitted is removed
    let (is_in_set, jailed) =
        check_validator(contact, cosmos_private_key, "BOND_STATUS_UNBONDING").await;
    assert!(is_in_set);
    assert!(jailed);
    info!("Evidence based slashing test succeeded! Validator now jailed!");
}

async fn check_validator(contact: &Contact, key: PrivateKey, filter: &str) -> (bool, bool) {
    let validators = contact
        .get_validators_list(QueryValidatorsRequest {
            pagination: None,
            status: filter.to_string(),
        })
        .await
        .unwrap();
    let addr = get_operator_address(key);
    for val in validators {
        if val.operator_address == addr.to_string() {
            return (true, val.jailed);
        }
    }
    (false, false)
}

async fn print_validator_status(contact: &Contact) {
    let validators = contact.get_active_validators().await.unwrap();
    for val in validators.iter() {
        info!(
            "Validator: {} Power: {} Jailed: {}",
            val.operator_address, val.tokens, val.jailed
        )
    }
}

/// Delegates to a specific validator
async fn delegate_to_validator(keys: &[ValidatorKeys], to: PrivateKey, contact: &Contact) {
    let delegate_address = get_operator_address(to);
    let amount = Coin {
        denom: STAKING_TOKEN.to_string(),
        amount: STARTING_STAKE_PER_VALIDATOR.wrapping_shr(2),
    };
    let res = contact
        .delegate_to_validator(
            delegate_address,
            amount,
            get_fee(),
            keys[1].validator_key,
            Some(TOTAL_TIMEOUT),
        )
        .await
        .unwrap();
    trace!("{:?}", res);
}
