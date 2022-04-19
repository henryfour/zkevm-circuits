// TODO Remove this
#![allow(missing_docs)]
// TODO Remove this
#![allow(unused_imports)]

mod sign_verify;

use crate::util::Expr;
use eth_types::{Address, Bytes, Field, ToBigEndian, ToLittleEndian, ToScalar, Word, U256, U64};
use ff::PrimeField;
use group::GroupEncoding;
use halo2_proofs::{
    arithmetic::{BaseExt, CurveAffine},
    circuit::{AssignedCell, Layouter, Region, SimpleFloorPlanner},
    plonk::{
        Advice, Circuit, Column, ConstraintSystem, Error, Expression, Fixed, Instance, Selector,
        VirtualCells,
    },
    poly::Rotation,
};
use k256::elliptic_curve::generic_array::{typenum::consts::U32, GenericArray};
use k256::elliptic_curve::AffineXCoordinate;
use k256::{ecdsa, PublicKey};
use libsecp256k1;
use rlp::RlpStream;
use secp256k1::Secp256k1Affine;
use sha3::{Digest, Keccak256};
use sign_verify::{SignData, SignVerifyChip, SignVerifyConfig};
pub use sign_verify::{POW_RAND_SIZE, VERIF_HEIGHT};
use std::convert::TryInto;
use std::{io::Cursor, marker::PhantomData, os::unix::prelude::FileTypeExt};

#[derive(Clone, Default, Debug)]
pub struct Transaction {
    /// Sender address
    pub from: Address,

    /// Recipient address (None for contract creation)
    pub to: Option<Address>,

    /// Supplied gas
    pub gas: U256,

    /// Gas price
    pub gas_price: U256,

    /// Transfered value (None for no transfer)
    pub value: U256,

    /// The compiled code of a contract OR the first 4 bytes of the hash of the
    /// invoked method signature and encoded parameters. For details see
    /// Ethereum Contract ABI
    pub data: Bytes,

    /// Transaction nonce
    pub nonce: U256,

    pub v: u64,
    pub r: U256,
    pub s: U256,
}

fn random_linear_combine<F: Field>(bytes: [u8; 32], randomness: F) -> F {
    crate::evm_circuit::util::Word::random_linear_combine(bytes, randomness)
}

fn recover_pk(v: u8, r: &Word, s: &Word, msg_hash: &GenericArray<u8, U32>) -> Secp256k1Affine {
    let r_be = r.to_be_bytes();
    let s_be = s.to_be_bytes();
    /*
    // let gar: &GenericArray<u8, 32> = GenericArray::from_slice(&r_be);
    // let gas: &GenericArray<u8, 32> = GenericArray::from_slice(&s_be);
    // let sig = ecdsa::Signature::from_scalars(*gar, *gas)?;
    let sig = ecdsa::Signature::from_scalars(r_be, s_be).expect("FIXME");
    let recovery_id = ecdsa::recoverable::Id::new(v).expect("FIXME");
    let sig = ecdsa::recoverable::Signature::new(&sig, recovery_id).expect("FIXME");
    let verif_key = sig
        .recover_verify_key_from_digest_bytes(msg_hash)
        .expect("FIXME");
    let pk: PublicKey = verif_key.into();
    pk.as_affine()
    // pk.as_affine().x()
    */
    let mut r = libsecp256k1::curve::Scalar::from_int(0);
    let _ = r.set_b32(&r_be); // TODO Check overflow
    let mut s = libsecp256k1::curve::Scalar::from_int(0);
    let _ = s.set_b32(&s_be); // TODO Check overflow
    let signature = libsecp256k1::Signature { r, s };
    let msg_hash = libsecp256k1::Message::parse_slice(msg_hash.as_slice()).expect("FIXME");
    let recovery_id = libsecp256k1::RecoveryId::parse(v).expect("FIXME");
    let pk = libsecp256k1::recover(&msg_hash, &signature, &recovery_id).expect("FIXME");
    let pk_be = pk.serialize();
    let mut pk_le = [0u8; 64];
    pk_le.copy_from_slice(&pk_be[1..]);
    println!("DBG recovered pk: {:x?} {:x?}", &pk_le[..32], &pk_le[32..]);
    pk_le[..32].reverse();
    pk_le[32..].reverse();
    Secp256k1Affine::from_bytes(&secp256k1::Serialized(pk_le)).unwrap()
}

fn tx_to_sign_data(tx: &Transaction, chain_id: u64) -> SignData {
    let sig_r_le = tx.r.to_le_bytes();
    let sig_s_le = tx.s.to_le_bytes();
    let sig_r = secp256k1::Fq::from_repr(sig_r_le).unwrap();
    let sig_s = secp256k1::Fq::from_repr(sig_s_le).unwrap();
    // msg = rlp([nonce, gasPrice, gas, to, value, data, sig_v, r, s])
    let mut stream = RlpStream::new_list(9);
    stream
        .append(&tx.nonce)
        .append(&tx.gas_price)
        .append(&tx.gas)
        .append(&tx.to.unwrap_or(Address::zero()))
        .append(&tx.value)
        .append(&tx.data.0)
        .append(&chain_id)
        .append(&0u32)
        .append(&0u32);
    let msg = stream.out();
    println!("DBG tx_rlp: {:x}", msg);
    let msg_hash = Keccak256::digest(&msg);
    println!("DBG sighash: {:x}", msg_hash);
    let v = (tx.v - 35 - chain_id * 2) as u8;
    let pk = recover_pk(v, &tx.r, &tx.s, &msg_hash);
    // TODO: msg_hash = msg_hash % q
    let mut msg_hash: [u8; 32] = msg_hash.as_slice().to_vec().try_into().unwrap();
    msg_hash.reverse();
    let msg_hash = secp256k1::Fq::from_repr(msg_hash).unwrap();
    println!("DBG sign_data sig: {:?} {:?}", sig_r, sig_s);
    println!("DBG sign_data pk: {:?}", pk);
    println!("DBG sign_data msg_hash: {:?}", msg_hash);
    SignData {
        signature: (sig_r, sig_s),
        pk,
        msg_hash,
        /* pub(crate) pk: Secp256k1Affine,
         * pub(crate) msg_hash: secp256k1::Fq, */
    }
}

// TODO: Deduplicate with
// `zkevm-circuits/src/evm_circuit/table.rs::TxContextFieldTag`.
#[derive(Clone, Copy, Debug)]
pub enum TxFieldTag {
    Null = 0,
    Nonce,
    Gas,
    GasPrice,
    CallerAddress,
    CalleeAddress,
    IsCreate,
    Value,
    CallDataLength,
    TxSignHash,
    CallData,
}

#[derive(Clone, Debug)]
struct TxCircuitConfig<F: Field> {
    tx_id: Column<Advice>,
    tag: Column<Advice>,
    index: Column<Advice>,
    value: Column<Advice>,
    sign_verify: SignVerifyConfig<F>,
    _marker: PhantomData<F>,
}

impl<F: Field> TxCircuitConfig<F> {
    fn new(meta: &mut ConstraintSystem<F>) -> Self {
        let tx_id = meta.advice_column();
        let tag = meta.advice_column();
        let index = meta.advice_column();
        let value = meta.advice_column();
        meta.enable_equality(value);

        let power_of_randomness = {
            // [(); POW_RAND_SIZE].map(|_| meta.instance_column())
            let columns = [(); sign_verify::POW_RAND_SIZE].map(|_| meta.instance_column());
            let mut power_of_randomness = None;

            meta.create_gate("power of randomness", |meta| {
                power_of_randomness =
                    Some(columns.map(|column| meta.query_instance(column, Rotation::cur())));

                [0.expr()]
            });

            power_of_randomness.unwrap()
        };
        let sign_verify = SignVerifyConfig::new(meta, power_of_randomness);

        Self {
            tx_id,
            tag,
            index,
            value,
            sign_verify,
            _marker: PhantomData,
        }
    }
}

#[derive(Default)]
struct TxCircuit<F: Field, const MAX_TXS: usize, const MAX_CALLDATA: usize> {
    sign_verify: SignVerifyChip<F, MAX_TXS>,
    randomness: F,
    txs: Vec<Transaction>,
    chain_id: u64,
}

/// Assigns a tx circuit row and returns the assigned cell of the value in
/// the row.
fn assign_row<F: Field>(
    region: &mut Region<'_, F>,
    config: &TxCircuitConfig<F>,
    offset: usize,
    tx_id: usize,
    tag: TxFieldTag,
    index: usize,
    value: F,
) -> Result<AssignedCell<F, F>, Error> {
    region.assign_advice(
        || "tx_id",
        config.tx_id,
        offset,
        || Ok(F::from(tx_id as u64)),
    )?;
    region.assign_advice(|| "tag", config.tag, offset, || Ok(F::from(tag as u64)))?;
    region.assign_advice(
        || "index",
        config.index,
        offset,
        || Ok(F::from(index as u64)),
    )?;
    region.assign_advice(|| "value", config.value, offset, || Ok(value))
}

impl<F: Field, const MAX_TXS: usize, const MAX_CALLDATA: usize> Circuit<F>
    for TxCircuit<F, MAX_TXS, MAX_CALLDATA>
{
    type Config = TxCircuitConfig<F>;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        Self::default()
    }

    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
        TxCircuitConfig::new(meta)
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        assert!(self.txs.len() <= MAX_TXS);
        let sign_datas: Vec<SignData> = self
            .txs
            .iter()
            .map(|tx| tx_to_sign_data(tx, self.chain_id))
            .collect();
        let assigned_sig_verifs = self.sign_verify.assign_txs(
            &config.sign_verify,
            &mut layouter,
            self.randomness,
            &sign_datas,
        )?;

        layouter.assign_region(
            || "tx table",
            |mut region| {
                let mut offset = 0;
                // Empty entry
                assign_row(
                    &mut region,
                    &config,
                    offset,
                    0,
                    TxFieldTag::Null,
                    0,
                    F::zero(),
                )?;
                offset += 1;
                // Assign al Tx fields except for call data
                let tx_default = Transaction::default();
                for i in 0..MAX_TXS {
                    let tx = if i < self.txs.len() {
                        &self.txs[i]
                    } else {
                        &tx_default
                    };
                    let assigned_sig_verif = &assigned_sig_verifs[i];
                    let address_cell = assigned_sig_verif.address.cell();
                    let msg_hash_rlc_cell = assigned_sig_verif.msg_hash_rlc.cell();
                    let msg_hash_rlc_value = assigned_sig_verif.msg_hash_rlc.value();
                    for (tag, value) in &[
                        (
                            TxFieldTag::Nonce,
                            random_linear_combine(tx.nonce.to_le_bytes(), self.randomness),
                        ),
                        (
                            TxFieldTag::Gas,
                            random_linear_combine(tx.gas.to_le_bytes(), self.randomness),
                        ),
                        (
                            TxFieldTag::GasPrice,
                            random_linear_combine(tx.gas_price.to_le_bytes(), self.randomness),
                        ),
                        (TxFieldTag::CallerAddress, tx.from.to_scalar().unwrap()),
                        (
                            TxFieldTag::CalleeAddress,
                            tx.to.unwrap_or(Address::zero()).to_scalar().unwrap(),
                        ),
                        (TxFieldTag::IsCreate, F::from(tx.to.is_none() as u64)),
                        (
                            TxFieldTag::Value,
                            random_linear_combine(tx.value.to_le_bytes(), self.randomness),
                        ),
                        (TxFieldTag::CallDataLength, F::from(tx.data.0.len() as u64)),
                        (
                            TxFieldTag::TxSignHash,
                            *msg_hash_rlc_value.unwrap_or(&F::zero()),
                        ),
                    ] {
                        let assigned_cell =
                            assign_row(&mut region, &config, offset, i + 1, *tag, 0, *value)?;
                        offset += 1;
                        match tag {
                            TxFieldTag::CallerAddress => {
                                region.constrain_equal(assigned_cell.cell(), address_cell)?
                            }
                            TxFieldTag::TxSignHash => {
                                region.constrain_equal(assigned_cell.cell(), msg_hash_rlc_cell)?
                            }
                            _ => (),
                        }
                    }
                }

                // Assign call data
                let mut calldata_count = 0;
                for (i, tx) in self.txs.iter().enumerate() {
                    for (index, byte) in tx.data.0.iter().enumerate() {
                        assert!(calldata_count < MAX_CALLDATA);
                        assign_row(
                            &mut region,
                            &config,
                            offset,
                            i + 1, // tx_id
                            TxFieldTag::CallData,
                            index,
                            F::from(*byte as u64),
                        )?;
                        offset += 1;
                        calldata_count += 1;
                    }
                }
                for i in calldata_count..MAX_CALLDATA {
                    assign_row(
                        &mut region,
                        &config,
                        offset,
                        0, // tx_id
                        TxFieldTag::CallData,
                        0,
                        F::zero(),
                    )?;
                    offset += 1;
                }
                Ok(())
            },
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tx_circuit_tests {
    use super::*;
    use ethers_core::types::{NameOrAddress, TransactionRequest};
    use ethers_core::utils::keccak256;
    use ethers_signers::LocalWallet;
    use rand::RngCore;
    use rand::SeedableRng;
    // use rand_xorshift::XorShiftRng;
    use ethers_signers::Signer;
    use group::Curve;
    use group::Group;
    use halo2_proofs::dev::MockProver;
    use halo2_proofs::pairing::bn256::Fr;
    use pretty_assertions::assert_eq;
    use rand_chacha::ChaCha20Rng;

    fn run<F: Field, const MAX_TXS: usize, const MAX_CALLDATA: usize>(
        txs: Vec<Transaction>,
        chain_id: u64,
    ) {
        let k = 20;
        let mut rng = ChaCha20Rng::seed_from_u64(2);
        let aux_generator =
            <Secp256k1Affine as CurveAffine>::CurveExt::random(&mut rng).to_affine();

        let randomness = F::random(&mut rng);
        let mut power_of_randomness: Vec<Vec<F>> = (1..POW_RAND_SIZE + 1)
            .map(|exp| vec![randomness.pow(&[exp as u64, 0, 0, 0]); txs.len() * VERIF_HEIGHT])
            .collect();
        // SignVerifyChip -> ECDSAChip -> MainGate instance column
        power_of_randomness.push(vec![]);
        // println!("DBG power_of_randomness: {:?}", power_of_randomness);
        let circuit = TxCircuit::<F, MAX_TXS, MAX_CALLDATA> {
            sign_verify: SignVerifyChip {
                aux_generator,
                window_size: 2,
                _marker: PhantomData,
            },
            randomness,
            txs,
            chain_id,
        };

        // let public_inputs = vec![vec![]];
        let prover = match MockProver::run(k, &circuit, power_of_randomness) {
            Ok(prover) => prover,
            Err(e) => panic!("{:#?}", e),
        };
        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn test_tx_pk_recovery() {
        // Generate a random wallet
        let mut rng = ChaCha20Rng::seed_from_u64(2);
        let chain_id: u64 = 1337;
        let mut txs = Vec::new();
        let wallet0 = LocalWallet::new(&mut rng).with_chain_id(chain_id);
        let signer = wallet0.signer();
        println!("DBG addr: {:#?}", wallet0.address());
        println!(
            "DBG pk: {:x?}",
            signer.verifying_key().to_bytes().as_slice()
        );
        let wallet1 = LocalWallet::new(&mut rng).with_chain_id(chain_id);
        let from = wallet0.address();
        let to = wallet1.address();
        let data = b"hello";
        let tx0 = TransactionRequest::new()
            .from(from)
            .to(to)
            .nonce(3)
            .value(1000)
            .data(data)
            .gas(500_000)
            .gas_price(1234);
        let tx = tx0;
        let tx_rlp = tx.rlp(chain_id);
        let sighash = keccak256(tx_rlp.as_ref()).into();
        let sig = wallet0.sign_hash(sighash, true);
        println!("tx: {:#?}", tx);
        println!("tx_rlp: {:x}", tx_rlp);
        println!("sighash: {:#?}", sighash);
        println!("sig: {:#?}", sig);
        let to = tx.to.map(|to| match to {
            NameOrAddress::Address(a) => a,
            _ => unreachable!(),
        });
        let tx = Transaction {
            from: tx.from.unwrap(),
            to,
            gas: tx.gas.unwrap(),
            gas_price: tx.gas_price.unwrap(),
            value: tx.value.unwrap(),
            data: tx.data.unwrap(),
            nonce: tx.nonce.unwrap(),
            v: sig.v,
            r: sig.r,
            s: sig.s,
        };
        txs.push(tx);

        const MAX_TXS: usize = 2;
        const MAX_CALLDATA: usize = 8;

        run::<Fr, MAX_TXS, MAX_CALLDATA>(txs, chain_id);
    }
}
