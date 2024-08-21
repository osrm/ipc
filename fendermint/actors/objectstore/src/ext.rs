// Copyright 2024 ADM Contributors
// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

pub mod blobs {
    use cid::Cid;
    use fvm_ipld_encoding::tuple::*;
    use fvm_shared::{address::Address, bigint::BigInt, clock::ChainEpoch, ActorID};
    use serde::{Deserialize, Serialize};

    pub const BLOBS_ACTOR_ID: ActorID = 49;
    pub const BLOBS_ACTOR_ADDR: Address = Address::new_id(BLOBS_ACTOR_ID);

    pub const ADD_BLOB_METHOD: u64 = frc42_dispatch::method_hash!("AddBlob");
    pub const DELETE_BLOB_METHOD: u64 = frc42_dispatch::method_hash!("DeleteBlob");
    pub const GET_BLOB_METHOD: u64 = frc42_dispatch::method_hash!("GetBlob");

    /// Account storage and credit details.
    #[derive(Clone, Debug, PartialEq, Serialize_tuple, Deserialize_tuple)]
    pub struct Account {
        /// Total size of all blobs managed by the account.
        pub capacity_used: BigInt,
        /// Current free credit in byte-blocks that can be used for new commitments.
        pub credit_free: BigInt,
        /// Current committed credit in byte-blocks that will be used for debits.
        pub credit_committed: BigInt,
        /// The chain epoch of the last debit.
        pub last_debit_epoch: ChainEpoch,
    }

    /// Params for adding a blob.
    #[derive(Clone, Debug, Serialize_tuple, Deserialize_tuple)]
    pub struct AddBlobParams {
        /// Blob content identifier.
        pub cid: Cid,
        /// Blob size.
        pub size: u64,
        /// Blob expiry epoch.
        pub expiry: ChainEpoch,
        /// Optional source actor robust address. Required is source is a machine.
        pub source: Option<Address>,
    }

    /// Params for deleting a blob.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct DeleteBlobParams(pub Cid);

    /// Params for getting a blob.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct GetBlobParams(pub Cid);

    /// The stored representation of a blob.
    /// Copied from fendermint/actors/blobs/src/state.rs
    #[derive(Clone, Debug, PartialEq, Serialize_tuple, Deserialize_tuple)]
    pub struct Blob {
        /// The size of the content.
        pub size: u64,
        /// Expiry block.
        pub expiry: ChainEpoch,
        /// TODO: add subs
        //pub subs: HashMap<Address, Subscription>,
        /// Whether the blob has been resolved.
        /// TODO: change to enum: resolving, resolved, failed
        pub resolved: bool,
    }
}
