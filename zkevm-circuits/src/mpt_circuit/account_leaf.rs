use eth_types::Field;
use gadgets::util::Scalar;
use halo2_proofs::{
    circuit::{Region, Value},
    plonk::{Error, VirtualCells},
    poly::Rotation,
};

use super::{helpers::MainData, param::HASH_WIDTH};
use super::{
    helpers::{LeafKeyGadget, ParentDataWitness},
    rlp_gadgets::RLPValueGadget,
};
use crate::table::ProofType;
use crate::{
    assign, circuit,
    circuit_tools::cell_manager::Cell,
    circuit_tools::constraint_builder::RLCable,
    mpt_circuit::{
        helpers::{key_memory, parent_memory, KeyData, MPTConstraintBuilder, ParentData},
        param::{KEY_LEN_IN_NIBBLES, RLP_LIST_LONG, RLP_LONG},
        FixedTableTag,
    },
    mpt_circuit::{witness_row::MptWitnessRow, MPTContext},
    mpt_circuit::{MPTConfig, ProofValues},
};
use crate::{
    circuit_tools::constraint_builder::RLCChainable,
    mpt_circuit::helpers::{DriftedGadget, WrongGadget},
};
use crate::{
    circuit_tools::{constraint_builder::RLCableValue, gadgets::IsEqualGadget},
    mpt_circuit::helpers::{main_memory, Indexable, IsEmptyTreeGadget},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct AccountLeafConfig<F> {
    main_data: MainData<F>,
    key_data: [KeyData<F>; 2],
    parent_data: [ParentData<F>; 2],
    rlp_key: [LeafKeyGadget<F>; 2],
    key_mult: [Cell<F>; 2],
    rlp_nonce: [RLPValueGadget<F>; 2],
    rlp_balance: [RLPValueGadget<F>; 2],
    rlp_storage: [RLPValueGadget<F>; 2],
    rlp_codehash: [RLPValueGadget<F>; 2],
    nonce_mult: [Cell<F>; 2],
    balance_mult: [Cell<F>; 2],
    is_empty_trie: [IsEmptyTreeGadget<F>; 2],
    drifted: DriftedGadget<F>,
    wrong: WrongGadget<F>,
    is_non_existing_account_proof: IsEqualGadget<F>,
    is_account_delete_mod: IsEqualGadget<F>,
    is_nonce_mod: IsEqualGadget<F>,
    is_balance_mod: IsEqualGadget<F>,
    is_storage_mod: IsEqualGadget<F>,
    is_codehash_mod: IsEqualGadget<F>,
}

impl<F: Field> AccountLeafConfig<F> {
    pub fn configure(
        meta: &mut VirtualCells<'_, F>,
        cb: &mut MPTConstraintBuilder<F>,
        ctx: MPTContext<F>,
    ) -> Self {
        let r = ctx.r.clone();

        cb.base.cell_manager.as_mut().unwrap().reset();
        let mut config = AccountLeafConfig::default();

        circuit!([meta, cb.base], {
            let key_bytes = [
                ctx.expr(meta, 0)[..36].to_owned(),
                ctx.expr(meta, 1)[..36].to_owned(),
            ];
            let wrong_bytes = ctx.expr(meta, 2)[..36].to_owned();
            let value_rlp_bytes = [
                [
                    ctx.expr(meta, 3)[..2].to_owned(),
                    ctx.expr(meta, 3)[34..36].to_owned(),
                ]
                .concat(),
                [
                    ctx.expr(meta, 4)[..2].to_owned(),
                    ctx.expr(meta, 4)[34..36].to_owned(),
                ]
                .concat(),
            ];
            let nonce_bytes = [
                ctx.expr(meta, 3)[..34].to_owned(),
                ctx.expr(meta, 4)[..34].to_owned(),
            ];
            let balance_bytes = [
                ctx.expr(meta, 3)[34..].to_owned(),
                ctx.expr(meta, 4)[34..].to_owned(),
            ];
            let storage_bytes = [
                ctx.expr(meta, 5)[..34].to_owned(),
                ctx.expr(meta, 6)[..34].to_owned(),
            ];
            let codehash_bytes = [
                ctx.expr(meta, 5)[34..].to_owned(),
                ctx.expr(meta, 6)[34..].to_owned(),
            ];
            let drifted_bytes = ctx.expr(meta, 7)[..36].to_owned();

            let nonce_lookup_offset = 3;
            let balance_lookup_offset = 4;
            let storage_lookup_offset = 5;
            let codehash_lookup_offset = 6;
            let wrong_offset = 2;

            // The two string RLP bytes stored in the s RLP bytes.
            // The two list RLP bytes are stored in the c RLP bytes.
            // The RLP bytes of nonce/balance are stored bytes[0].

            config.main_data = MainData::load(
                "main storage",
                &mut cb.base,
                &ctx.memory[main_memory()],
                0.expr(),
            );

            // Don't allow an account node to follow an account node
            require!(config.main_data.is_below_account => false);

            let mut key_rlc = vec![0.expr(); 2];
            let mut nonce_rlc = vec![0.expr(); 2];
            let mut balance_rlc = vec![0.expr(); 2];
            let mut storage_rlc = vec![0.expr(); 2];
            let mut codehash_rlc = vec![0.expr(); 2];
            let mut leaf_no_key_rlc = vec![0.expr(); 2];
            for is_s in [true, false] {
                // Key data
                let key_data = &mut config.key_data[is_s.idx()];
                *key_data = KeyData::load(&mut cb.base, &ctx.memory[key_memory(is_s)], 0.expr());

                // Parent data
                let parent_data = &mut config.parent_data[is_s.idx()];
                *parent_data = ParentData::load(
                    "account load",
                    &mut cb.base,
                    &ctx.memory[parent_memory(is_s)],
                    0.expr(),
                );

                // Placeholder leaf checks
                config.is_empty_trie[is_s.idx()] =
                    IsEmptyTreeGadget::construct(&mut cb.base, parent_data.rlc.expr(), &r);

                // Calculate the key RLC
                let rlp_key = &mut config.rlp_key[is_s.idx()];
                *rlp_key = LeafKeyGadget::construct(&mut cb.base, &key_bytes[is_s.idx()]);
                config.rlp_nonce[is_s.idx()] =
                    RLPValueGadget::construct(&mut cb.base, &nonce_bytes[is_s.idx()][2..]);
                config.rlp_balance[is_s.idx()] =
                    RLPValueGadget::construct(&mut cb.base, &balance_bytes[is_s.idx()][2..]);
                config.rlp_storage[is_s.idx()] =
                    RLPValueGadget::construct(&mut cb.base, &storage_bytes[is_s.idx()][1..]);
                config.rlp_codehash[is_s.idx()] =
                    RLPValueGadget::construct(&mut cb.base, &codehash_bytes[is_s.idx()][1..]);

                // Storage root and codehash are always 32-byte hashes.
                require!(config.rlp_storage[is_s.idx()].len() => HASH_WIDTH);
                require!(config.rlp_codehash[is_s.idx()].len() => HASH_WIDTH);

                config.key_mult[is_s.idx()] = cb.base.query_cell();
                config.nonce_mult[is_s.idx()] = cb.base.query_cell();
                config.balance_mult[is_s.idx()] = cb.base.query_cell();
                require!((FixedTableTag::RMult, rlp_key.num_bytes_on_key_row(), config.key_mult[is_s.idx()].expr()) => @"fixed");
                require!((FixedTableTag::RMult, config.rlp_nonce[is_s.idx()].num_bytes() + 4.expr(), config.nonce_mult[is_s.idx()].expr()) => @format!("fixed"));
                require!((FixedTableTag::RMult, config.rlp_balance[is_s.idx()].num_bytes(), config.balance_mult[is_s.idx()].expr()) => @format!("fixed"));

                // RLC bytes zero check
                cb.set_length(rlp_key.num_bytes_on_key_row());

                let nonce_rlp_rlc;
                let balance_rlp_rlc;
                let storage_rlp_rlc;
                let codehash_rlp_rlc;
                (nonce_rlc[is_s.idx()], nonce_rlp_rlc) = config.rlp_nonce[is_s.idx()].rlc(&r);
                (balance_rlc[is_s.idx()], balance_rlp_rlc) = config.rlp_balance[is_s.idx()].rlc(&r);
                (storage_rlc[is_s.idx()], storage_rlp_rlc) = config.rlp_storage[is_s.idx()].rlc(&r);
                (codehash_rlc[is_s.idx()], codehash_rlp_rlc) =
                    config.rlp_codehash[is_s.idx()].rlc(&r);

                // Calculate the leaf RLC
                leaf_no_key_rlc[is_s.idx()] = (0.expr(), 1.expr()).rlc_chain(
                    (
                        [value_rlp_bytes[is_s.idx()].clone(), vec![nonce_rlp_rlc]]
                            .concat()
                            .rlc(&r),
                        config.nonce_mult[is_s.idx()].expr(),
                    )
                        .rlc_chain(
                            (balance_rlp_rlc, config.balance_mult[is_s.idx()].expr()).rlc_chain(
                                (storage_rlp_rlc, r[32].expr()).rlc_chain(codehash_rlp_rlc),
                            ),
                        ),
                );
                let leaf_rlc = (rlp_key.rlc(&r), config.key_mult[is_s.idx()].expr())
                    .rlc_chain(leaf_no_key_rlc[is_s.idx()].expr());

                // Key
                key_rlc[is_s.idx()] = key_data.rlc.expr()
                    + rlp_key.leaf_key_rlc(
                        &mut cb.base,
                        key_data.mult.expr(),
                        key_data.is_odd.expr(),
                        1.expr(),
                        &r,
                    );
                // Total number of nibbles needs to be KEY_LEN_IN_NIBBLES.
                let num_nibbles = rlp_key.num_key_nibbles(key_data.is_odd.expr());
                require!(key_data.num_nibbles.expr() + num_nibbles.expr() => KEY_LEN_IN_NIBBLES);

                // Check if the account is in its parent.
                // Check is skipped for placeholder leafs which are dummy leafs
                ifx! {not!(and::expr(&[not!(config.parent_data[is_s.idx()].is_placeholder), config.is_empty_trie[is_s.idx()].expr()])) => {
                    require!((1, leaf_rlc, config.rlp_key[is_s.idx()].num_bytes(), config.parent_data[is_s.idx()].rlc) => @"keccak");
                }}

                // Check the RLP encoding consistency.
                // RlP encoding: account = [key, [nonce, balance, storage, codehash]]
                // We always store between 55 and 256 bytes of data in the values list.
                require!(value_rlp_bytes[is_s.idx()][0] => RLP_LONG + 1);
                // The RLP encoded list always has 2 RLP bytes (the c RLP bytes).
                require!(value_rlp_bytes[is_s.idx()][1] => value_rlp_bytes[is_s.idx()][3].expr() + 2.expr());
                // `c_main.rlp1` always needs to be RLP_LIST_LONG + 1.
                require!(value_rlp_bytes[is_s.idx()][2] => RLP_LIST_LONG + 1);
                // The length of the list is `#(nonce) + #(balance) + 2 * (1 + #(hash))`.
                require!(value_rlp_bytes[is_s.idx()][3] => config.rlp_nonce[is_s.idx()].num_bytes() + config.rlp_balance[is_s.idx()].num_bytes() + (2 * (1 + 32)).expr());
                // Now check that the the key and value list length matches the account length.
                // The RLP encoded string always has 2 RLP bytes (the s RLP bytes).
                let value_list_num_bytes = value_rlp_bytes[is_s.idx()][1].expr() + 2.expr();
                // Account length needs to equal all key bytes and all values list bytes.
                require!(config.rlp_key[is_s.idx()].num_bytes() => config.rlp_key[is_s.idx()].num_bytes_on_key_row() + value_list_num_bytes);

                // Key done, set the starting values
                KeyData::store(
                    &mut cb.base,
                    &ctx.memory[key_memory(is_s)],
                    KeyData::default_values_expr(),
                );
                // Store the new parent
                ParentData::store(
                    &mut cb.base,
                    &ctx.memory[parent_memory(is_s)],
                    [
                        storage_rlc[is_s.idx()].expr(),
                        true.expr(),
                        false.expr(),
                        storage_rlc[is_s.idx()].expr(),
                    ],
                );
            }

            // Anything following this node is below the account
            // TODO(Brecht): For non-existing accounts it should be impossible to prove
            // storage leafs
            MainData::store(
                &mut cb.base,
                &ctx.memory[main_memory()],
                [
                    config.main_data.proof_type.expr(),
                    true.expr(),
                    a!(ctx.mpt_table.address_rlc, nonce_lookup_offset),
                ],
            );

            // Proof types
            config.is_non_existing_account_proof = IsEqualGadget::construct(
                &mut cb.base,
                config.main_data.proof_type.expr(),
                ProofType::AccountDoesNotExist.expr(),
            );
            config.is_account_delete_mod = IsEqualGadget::construct(
                &mut cb.base,
                config.main_data.proof_type.expr(),
                ProofType::AccountDestructed.expr(),
            );
            config.is_nonce_mod = IsEqualGadget::construct(
                &mut cb.base,
                config.main_data.proof_type.expr(),
                ProofType::NonceChanged.expr(),
            );
            config.is_balance_mod = IsEqualGadget::construct(
                &mut cb.base,
                config.main_data.proof_type.expr(),
                ProofType::BalanceChanged.expr(),
            );
            config.is_storage_mod = IsEqualGadget::construct(
                &mut cb.base,
                config.main_data.proof_type.expr(),
                ProofType::StorageChanged.expr(),
            );
            config.is_codehash_mod = IsEqualGadget::construct(
                &mut cb.base,
                config.main_data.proof_type.expr(),
                ProofType::CodeHashExists.expr(),
            );

            // Drifted leaf handling
            config.drifted = DriftedGadget::construct(
                cb,
                &config.parent_data,
                &config.key_data,
                &key_rlc,
                &leaf_no_key_rlc,
                &drifted_bytes,
                &ctx.r,
            );

            // Wrong leaf handling
            config.wrong = WrongGadget::construct(
                meta,
                cb,
                ctx.clone(),
                ctx.mpt_table.address_rlc,
                config.is_non_existing_account_proof.expr(),
                &config.rlp_key,
                &key_rlc,
                &wrong_bytes,
                wrong_offset,
                true,
                &ctx.r,
            );

            // Account delete
            // We need to make sure there is no leaf when account is deleted. Two possible
            // cases:
            // - 1. Account leaf is deleted and there is a nil object in
            // branch. In this case we have a placeholder leaf.
            // - 2. Account leaf is deleted from a branch with two leaves, the remaining
            // leaf moves one level up and replaces the branch. In this case we
            // have a branch placeholder.
            ifx! {config.is_account_delete_mod => {
                require!(or::expr([
                    config.key_data[false.idx()].is_placeholder_leaf_c.expr(),
                    config.parent_data[false.idx()].is_placeholder.expr()
                ]) => true);
            }}

            // Check that there is only one modification (except when the account is being
            // deleted).
            ifx! {not!(config.is_account_delete_mod) => {
                // Nonce needs to remain the same when not modifying the nonce
                ifx!{not!(config.is_nonce_mod) => {
                    require!(nonce_rlc[false.idx()] => nonce_rlc[true.idx()]);
                }}
                // Balance needs to remain the same when not modifying the balance
                ifx!{not!(config.is_balance_mod) => {
                    require!(balance_rlc[false.idx()] => balance_rlc[true.idx()]);
                }}
                // Storage root needs to remain the same when not modifying the storage root
                ifx!{not!(config.is_storage_mod) => {
                    require!(storage_rlc[false.idx()] => storage_rlc[true.idx()]);
                }}
                // Codehash root needs to remain the same when not modifying the codehash
                ifx!{not!(config.is_codehash_mod) => {
                    require!(codehash_rlc[false.idx()] => codehash_rlc[true.idx()]);
                }}
            }}

            // Lookup data
            // Set the address
            for is_s in [true, false] {
                // The computed key RLC needs to be the same as the value in `address_rlc`
                // column, except when below a placeholder branch because then `key_rlc` is
                // computed for the branch that is parallel to the placeholder branch.
                for lookup_offset in [
                    nonce_lookup_offset,
                    balance_lookup_offset,
                    storage_lookup_offset,
                    codehash_lookup_offset,
                ] {
                    ifx! {not!(config.parent_data[is_s.idx()].is_placeholder), not!(config.is_non_existing_account_proof) => {
                        require!(a!(ctx.mpt_table.address_rlc, lookup_offset) => key_rlc[is_s.idx()]);
                    }}
                }
            }
            // Nonce
            require!(a!(ctx.mpt_table.value_prev, nonce_lookup_offset) => nonce_rlc[true.idx()]);
            require!(a!(ctx.mpt_table.value, nonce_lookup_offset) => nonce_rlc[false.idx()]);
            // Balance
            require!(a!(ctx.mpt_table.value_prev, balance_lookup_offset) => balance_rlc[true.idx()]);
            require!(a!(ctx.mpt_table.value, balance_lookup_offset) => balance_rlc[false.idx()]);
            // Storage root
            require!(a!(ctx.mpt_table.value_prev, storage_lookup_offset) => storage_rlc[true.idx()]);
            require!(a!(ctx.mpt_table.value, storage_lookup_offset) => storage_rlc[false.idx()]);
            // Codehash
            require!(a!(ctx.mpt_table.value_prev, codehash_lookup_offset) => codehash_rlc[true.idx()]);
            require!(a!(ctx.mpt_table.value, codehash_lookup_offset) => codehash_rlc[false.idx()]);
        });

        config
    }

    pub fn assign(
        &self,
        region: &mut Region<'_, F>,
        ctx: &MPTConfig<F>,
        witness: &mut [MptWitnessRow<F>],
        pv: &mut ProofValues<F>,
        offset: usize,
        idx: usize,
    ) -> Result<(), Error> {
        let key_s = witness[idx].to_owned();
        let key_c = witness[idx + 1].to_owned();
        let nonce_balance_s = witness[idx + 3].to_owned();
        let nonce_balance_c = witness[idx + 4].to_owned();
        let storage_codehash_s = witness[idx + 5].to_owned();
        let storage_codehash_c = witness[idx + 6].to_owned();
        let row_drifted = witness[idx + 7].to_owned();

        let row_key = [&key_s, &key_c];
        let row_wrong = witness[idx + 2].to_owned();
        let nonce_bytes = [
            nonce_balance_s.bytes[..34].to_owned(),
            nonce_balance_c.bytes[..34].to_owned(),
        ];
        let balance_bytes = [
            nonce_balance_s.bytes[34..68].to_owned(),
            nonce_balance_c.bytes[34..68].to_owned(),
        ];
        let storage_bytes = [
            storage_codehash_s.bytes[..34].to_owned(),
            storage_codehash_c.bytes[..34].to_owned(),
        ];
        let codehash_bytes = [
            storage_codehash_s.bytes[34..68].to_owned(),
            storage_codehash_c.bytes[34..68].to_owned(),
        ];

        let key_s_lookup_offset = offset;
        let nonce_lookup_offset = offset + 3;
        let balance_lookup_offset = offset + 4;
        let storage_lookup_offset = offset + 5;
        let codehash_lookup_offset = offset + 6;
        let wrong_offset = offset + 2;

        let main_data =
            self.main_data
                .witness_load(region, offset, &pv.memory[main_memory()], 0)?;

        // Key
        let mut key_rlc = vec![0.scalar(); 2];
        let mut nonce_value_rlc = vec![0.scalar(); 2];
        let mut balance_value_rlc = vec![0.scalar(); 2];
        let mut storage_value_rlc = vec![0.scalar(); 2];
        let mut codehash_value_rlc = vec![0.scalar(); 2];
        let mut parent_data = vec![ParentDataWitness::default(); 2];
        for is_s in [true, false] {
            let key_row = &row_key[is_s.idx()];

            let key_data = self.key_data[is_s.idx()].witness_load(
                region,
                offset,
                &mut pv.memory[key_memory(is_s)],
                0,
            )?;

            parent_data[is_s.idx()] = self.parent_data[is_s.idx()].witness_load(
                region,
                offset,
                &mut pv.memory[parent_memory(is_s)],
                0,
            )?;

            self.is_empty_trie[is_s.idx()].assign(
                region,
                offset,
                parent_data[is_s.idx()].rlc,
                ctx.r,
            )?;

            let rlp_key_witness =
                self.rlp_key[is_s.idx()].assign(region, offset, &key_row.bytes)?;
            let nonce_witness =
                self.rlp_nonce[is_s.idx()].assign(region, offset, &nonce_bytes[is_s.idx()][2..])?;
            let balance_witness = self.rlp_balance[is_s.idx()].assign(
                region,
                offset,
                &balance_bytes[is_s.idx()][2..],
            )?;
            let storage_witness = self.rlp_storage[is_s.idx()].assign(
                region,
                offset,
                &storage_bytes[is_s.idx()][1..],
            )?;
            let codehash_witness = self.rlp_codehash[is_s.idx()].assign(
                region,
                offset,
                &codehash_bytes[is_s.idx()][1..],
            )?;

            nonce_value_rlc[is_s.idx()] = nonce_witness.rlc_value(ctx.r);
            balance_value_rlc[is_s.idx()] = balance_witness.rlc_value(ctx.r);
            storage_value_rlc[is_s.idx()] = storage_witness.rlc_value(ctx.r);
            codehash_value_rlc[is_s.idx()] = codehash_witness.rlc_value(ctx.r);

            // + 4 because of s_rlp1, s_rlp2, c_rlp1, c_rlp2
            let mut mult_nonce = F::one();
            for _ in 0..nonce_witness.num_bytes() + 4 {
                mult_nonce *= ctx.r;
            }
            let mut mult_balance = F::one();
            for _ in 0..balance_witness.num_bytes() {
                mult_balance *= ctx.r;
            }
            self.nonce_mult[is_s.idx()].assign(region, offset, mult_nonce)?;
            self.balance_mult[is_s.idx()].assign(region, offset, mult_balance)?;

            // Key
            (key_rlc[is_s.idx()], _) =
                rlp_key_witness.leaf_key_rlc(key_data.rlc, key_data.mult, ctx.r);

            let mut key_mult = F::one();
            for _ in 0..rlp_key_witness.num_bytes_on_key_row() {
                key_mult *= ctx.r;
            }
            self.key_mult[is_s.idx()].assign(region, offset, key_mult)?;

            // Update key and parent state
            self.key_data[is_s.idx()].witness_store(
                region,
                offset,
                &mut pv.memory[key_memory(is_s)],
                F::zero(),
                F::one(),
                0,
                false,
                false,
                0,
                false,
                F::zero(),
                F::one(),
            )?;
            self.parent_data[is_s.idx()].witness_store(
                region,
                offset,
                &mut pv.memory[parent_memory(is_s)],
                storage_value_rlc[is_s.idx()],
                true,
                false,
                storage_value_rlc[is_s.idx()],
            )?;
        }

        // Anything following this node is below the account
        let address_rlc = witness[idx + (nonce_lookup_offset - offset)]
            .address_bytes()
            .rlc_value(ctx.r);
        pv.memory[main_memory()].witness_store(
            offset,
            &[main_data.proof_type.scalar(), true.scalar(), address_rlc],
        );

        // Proof types
        let is_non_existing_proof = self.is_non_existing_account_proof.assign(
            region,
            offset,
            main_data.proof_type.scalar(),
            ProofType::AccountDoesNotExist.scalar(),
        )? == true.scalar();
        let is_account_delete_mod = self.is_account_delete_mod.assign(
            region,
            offset,
            main_data.proof_type.scalar(),
            ProofType::AccountDestructed.scalar(),
        )? == true.scalar();
        let is_nonce_mod = self.is_nonce_mod.assign(
            region,
            offset,
            main_data.proof_type.scalar(),
            ProofType::NonceChanged.scalar(),
        )? == true.scalar();
        let is_balance_mod = self.is_balance_mod.assign(
            region,
            offset,
            main_data.proof_type.scalar(),
            ProofType::BalanceChanged.scalar(),
        )? == true.scalar();
        let _is_storage_mod = self.is_storage_mod.assign(
            region,
            offset,
            main_data.proof_type.scalar(),
            ProofType::StorageChanged.scalar(),
        )? == true.scalar();
        let is_codehash_mod = self.is_codehash_mod.assign(
            region,
            offset,
            main_data.proof_type.scalar(),
            ProofType::CodeHashExists.scalar(),
        )? == true.scalar();

        // Drifted leaf handling
        self.drifted
            .assign(region, offset, &parent_data, &row_drifted.bytes, ctx.r)?;

        // Wrong leaf handling
        self.wrong.assign(
            region,
            offset,
            ctx,
            ctx.mpt_table.address_rlc,
            is_non_existing_proof,
            &mut pv.memory,
            &key_rlc,
            &row_wrong.bytes,
            wrong_offset,
            row_key,
            true,
            ProofType::AccountDoesNotExist,
            ctx.r,
        )?;

        // Lookup data
        // Set the address
        for lookup_offset in [
            nonce_lookup_offset,
            balance_lookup_offset,
            storage_lookup_offset,
            codehash_lookup_offset,
        ] {
            assign!(region, (ctx.mpt_table.address_rlc, lookup_offset) => address_rlc)?;
        }
        if is_account_delete_mod {
            assign!(region, (ctx.mpt_table.proof_type, key_s_lookup_offset) => ProofType::AccountDoesNotExist.scalar())?;
        }
        // Nonce
        if is_nonce_mod {
            assign!(region, (ctx.mpt_table.proof_type, nonce_lookup_offset) => ProofType::NonceChanged.scalar())?;
        }
        assign!(region, (ctx.mpt_table.value_prev, nonce_lookup_offset) => nonce_value_rlc[true.idx()])?;
        assign!(region, (ctx.mpt_table.value, nonce_lookup_offset) => nonce_value_rlc[false.idx()])?;
        // Balance
        if is_balance_mod {
            assign!(region, (ctx.mpt_table.proof_type, balance_lookup_offset) => ProofType::BalanceChanged.scalar())?;
        }
        assign!(region, (ctx.mpt_table.value_prev, balance_lookup_offset) => balance_value_rlc[true.idx()])?;
        assign!(region, (ctx.mpt_table.value, balance_lookup_offset) => balance_value_rlc[false.idx()])?;
        // Storage root
        assign!(region, (ctx.mpt_table.value_prev, storage_lookup_offset) => storage_value_rlc[true.idx()])?;
        assign!(region, (ctx.mpt_table.value, storage_lookup_offset) => storage_value_rlc[false.idx()])?;
        // Codehash
        if is_codehash_mod {
            assign!(region, (ctx.mpt_table.proof_type, codehash_lookup_offset) => ProofType::CodeHashExists.scalar())?;
        }
        assign!(region, (ctx.mpt_table.value_prev, codehash_lookup_offset) => codehash_value_rlc[true.idx()])?;
        assign!(region, (ctx.mpt_table.value, codehash_lookup_offset) => codehash_value_rlc[false.idx()])?;

        Ok(())
    }
}
