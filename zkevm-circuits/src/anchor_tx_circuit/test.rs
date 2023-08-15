#![allow(unused_imports)]
use std::{collections::HashMap, str::FromStr};

use super::{
    sign_verify::{GOLDEN_TOUCH_ADDRESS, GOLDEN_TOUCH_PRIVATEKEY, GX1, GX1_MUL_PRIVATEKEY, GX2, N},
    *,
};
use crate::{
    util::{log2_ceil, unusable_rows},
    witness::{block_convert, Block},
};
use bus_mapping::{
    circuit_input_builder::{CircuitInputBuilder, CircuitsParams},
    mock::BlockData,
};
use eth_types::{
    address, bytecode,
    geth_types::{GethData, Transaction},
    sign_types::{biguint_to_32bytes_le, ct_option_ok_or, sign, SignData, SECP256K1_Q},
    word, Address, Field, ToBigEndian, ToLittleEndian, ToWord, Word, H256, U256,
};
use ethers_core::types::Eip1559TransactionRequest;
use ethers_signers::{LocalWallet, Signer};
use gadgets::{
    is_equal::IsEqualChip,
    mul_add::{MulAddChip, MulAddConfig},
    util::{split_u256, Expr},
};
use halo2_proofs::{
    arithmetic::Field as _,
    circuit::{Layouter, Region, Value},
    dev::{MockProver, VerifyFailure},
    halo2curves::{
        bn256::Fr,
        ff::PrimeField,
        group::Curve,
        secp256k1::{self, Secp256k1Affine},
    },
    plonk::{Advice, Column, ConstraintSystem, Error, Expression, Fixed, SecondPhase, Selector},
    poly::Rotation,
};
use itertools::Itertools;
use log::error;
use mock::{AddrOrWallet, MockAccount, MockTransaction, TestContext, MOCK_CHAIN_ID};
use num::Integer;
use num_bigint::BigUint;
use once_cell::sync::Lazy;
use sha3::{Digest, Keccak256};

pub(crate) fn anchor_sign(
    anchor_tx: &Transaction,
    chain_id: u64,
) -> Result<SignData, eth_types::Error> {
    // msg = rlp([nonce, gasPrice, gas, to, value, data, sig_v, r, s])
    let req: Eip1559TransactionRequest = anchor_tx.into();
    let msg = req.chain_id(chain_id).rlp();
    let msg_hash: [u8; 32] = Keccak256::digest(&msg)
        .as_slice()
        .to_vec()
        .try_into()
        .expect("hash length isn't 32 bytes");
    // msg_hash = msg_hash % q
    let msg_hash = BigUint::from_bytes_be(msg_hash.as_slice());
    let msg_hash = msg_hash.mod_floor(&*SECP256K1_Q);
    let msg_hash_le = biguint_to_32bytes_le(msg_hash);
    let msg_hash = ct_option_ok_or(
        secp256k1::Fq::from_repr(msg_hash_le),
        libsecp256k1::Error::InvalidMessage,
    )?;
    let k1 = secp256k1::Fq::ONE;
    let sk = ct_option_ok_or(
        secp256k1::Fq::from_repr(GOLDEN_TOUCH_PRIVATEKEY.to_le_bytes()),
        libsecp256k1::Error::InvalidSecretKey,
    )?;
    let generator = Secp256k1Affine::generator();
    let pk = generator * sk;
    let pk = pk.to_affine();
    let (mut sig_r, mut sig_s) = sign(k1, sk, msg_hash);
    let gx1 = ct_option_ok_or(
        secp256k1::Fq::from_repr(GX1.to_le_bytes()),
        libsecp256k1::Error::InvalidSignature,
    )?;
    assert!(sig_r == gx1);
    if sig_s == secp256k1::Fq::ZERO {
        let k2 = secp256k1::Fq::ONE + secp256k1::Fq::ONE;
        (sig_r, sig_s) = sign(k2, sk, msg_hash);
        let gx2 = ct_option_ok_or(
            secp256k1::Fq::from_repr(GX2.to_le_bytes()),
            libsecp256k1::Error::InvalidSignature,
        )?;
        assert!(sig_r == gx2);
    }
    Ok(SignData {
        signature: (sig_r, sig_s),
        pk,
        msg_hash,
    })
}

fn run<F: Field>(block: &Block<F>, sign_hash: Option<H256>) -> Result<(), Vec<VerifyFailure>> {
    let k = log2_ceil(
        AnchorTxCircuit::<Fr>::unusable_rows()
            + AnchorTxCircuit::<Fr>::min_num_rows(block.circuits_params.max_txs),
    );
    let mut circuit = TestAnchorTxCircuit::<F>::new_from_block(block);
    if let Some(sign_hash) = sign_hash {
        circuit.sign_hash(sign_hash);
    }
    let prover = match MockProver::run(k + 3, &circuit, vec![]) {
        Ok(prover) => prover,
        Err(e) => panic!("{:#?}", e),
    };
    prover.verify()
}

pub(crate) fn add_anchor_accounts<const NACC: usize, FAcc>(
    accs: [&mut MockAccount; NACC],
    acc_fns: FAcc,
    protocol_instance: &ProtocolInstance,
) where
    FAcc: FnOnce([&mut MockAccount; NACC]),
{
    let code = bytecode! {
        PUSH1(0x01) // value
        PUSH1(0x02) // key
        SSTORE

        PUSH3(0xbb)
    };
    accs[0]
        .address(*GOLDEN_TOUCH_ADDRESS)
        .balance(Word::from(1u64 << 20));
    accs[1].address(protocol_instance.l2_contract).code(code);
    acc_fns(accs);
}

pub(crate) fn add_anchor_tx<const NACC: usize, FTx>(
    mut txs: Vec<&mut MockTransaction>,
    accs: [MockAccount; NACC],
    func_tx: FTx,
    extra_func_tx: fn(&mut MockTransaction),
    protocol_instance: &ProtocolInstance,
) where
    FTx: FnOnce(Vec<&mut MockTransaction>, [MockAccount; NACC]),
{
    txs[0]
        .gas(protocol_instance.anchor_gas_limit.to_word())
        .from(*GOLDEN_TOUCH_ADDRESS)
        .to(protocol_instance.l2_contract)
        .input(protocol_instance.anchor_call())
        .nonce(0)
        .value(ANCHOR_TX_VALUE.to_word());
    extra_func_tx(txs[0]);
    func_tx(txs, accs);
}

fn gen_block<const NUM_TXS: usize>(
    max_txs: usize,
    max_calldata: usize,
    protocol_instance: ProtocolInstance,
    extra_func_tx: fn(&mut MockTransaction),
) -> Block<Fr> {
    let chain_id = (*MOCK_CHAIN_ID).as_u64();
    let mut wallets = HashMap::new();

    let wallet =
        LocalWallet::from_str("0x92954368afd3caa1f3ce3ead0069c1af414054aefe1ef9aeacc1bf426222ce38")
            .unwrap()
            .with_chain_id(chain_id);

    wallets.insert(*GOLDEN_TOUCH_ADDRESS, wallet);

    let block: GethData = TestContext::<2, NUM_TXS>::new(
        None,
        |accs| {
            add_anchor_accounts(accs, |_| {}, &protocol_instance);
        },
        |txs, accs| {
            add_anchor_tx(txs, accs, |_, _| {}, extra_func_tx, &protocol_instance);
        },
        |block, _tx| block,
    )
    .unwrap()
    .into();
    let circuits_params = CircuitsParams {
        max_txs,
        max_calldata,
        ..Default::default()
    };
    let mut builder = BlockData::new_from_geth_data_with_params(block.clone(), circuits_params)
        .new_circuit_input_builder();
    builder
        .handle_block(&block.eth_block, &block.geth_traces)
        .unwrap();
    let mut block = block_convert::<Fr>(&builder.block, &builder.code_db).unwrap();
    block.protocol_instance = protocol_instance;
    block
}

pub(crate) fn sign_tx(tx: &mut MockTransaction) {
    let chain_id = (*MOCK_CHAIN_ID).as_u64();
    let _tx: Transaction = tx.to_owned().into();
    let sig_data = anchor_sign(&_tx, chain_id).unwrap();
    let sig_r = U256::from_little_endian(sig_data.signature.0.to_bytes().as_slice());
    let sig_s = U256::from_little_endian(sig_data.signature.1.to_bytes().as_slice());
    tx.sig_data((2712, sig_r, sig_s));
}

fn sign_tx_r_is_gx2(tx: &mut MockTransaction) {
    let msg_hash = *N - *GX1_MUL_PRIVATEKEY;
    let msg_hash = ct_option_ok_or(
        secp256k1::Fq::from_repr(msg_hash.to_le_bytes()),
        libsecp256k1::Error::InvalidMessage,
    )
    .unwrap();
    let k2 = secp256k1::Fq::ONE + secp256k1::Fq::ONE;
    let sk = ct_option_ok_or(
        secp256k1::Fq::from_repr(GOLDEN_TOUCH_PRIVATEKEY.to_le_bytes()),
        libsecp256k1::Error::InvalidSecretKey,
    )
    .unwrap();
    let (sig_r, sig_s) = sign(k2, sk, msg_hash);
    let sig_r = U256::from_little_endian(sig_r.to_bytes().as_slice());
    let sig_s = U256::from_little_endian(sig_s.to_bytes().as_slice());
    tx.sig_data((2712, sig_r, sig_s));
}

#[test]
fn anchor_tx_circuit_unusable_rows() {
    assert_eq!(
        AnchorTxCircuit::<Fr>::unusable_rows(),
        unusable_rows::<Fr, TestAnchorTxCircuit::<Fr>>(()),
    )
}

#[test]
fn anchor_test() {
    let protocol_instance = ProtocolInstance {
        anchor_gas_limit: 150000,
        ..Default::default()
    };
    let block = gen_block::<1>(2, 200, protocol_instance, sign_tx);
    assert_eq!(run::<Fr>(&block, None), Ok(()));
}

#[test]
fn anchor_test_when_sign_r_is_gx2() {
    let protocol_instance = ProtocolInstance {
        anchor_gas_limit: 150000,
        ..Default::default()
    };
    let msg_hash = *N - *GX1_MUL_PRIVATEKEY;
    let msg_hash = H256::from(msg_hash.to_le_bytes());
    let block = gen_block::<1>(2, 200, protocol_instance, sign_tx_r_is_gx2);
    assert_eq!(run::<Fr>(&block, Some(msg_hash)), Ok(()));
}
