// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT
use anyhow::{anyhow, Context};
use cid::Cid;
use fendermint_actor_blob_reader::BLOB_READER_ACTOR_NAME;
use fendermint_actor_blobs::BLOBS_ACTOR_NAME;
use fendermint_actor_bucket::BUCKET_ACTOR_NAME;
use fendermint_actor_chainmetadata::CHAINMETADATA_ACTOR_NAME;
use fendermint_actor_eam::IPC_EAM_ACTOR_NAME;
use fendermint_actor_gas_market_eip1559::ACTOR_NAME as GAS_MARKET_EIP1559_ACTOR_NAME;
use fendermint_actor_recall_config::ACTOR_NAME as RECALL_CONFIG_ACTOR_NAME;
use fendermint_actor_timehub::TIMEHUB_ACTOR_NAME;
use fvm_ipld_blockstore::Blockstore;
use fvm_ipld_encoding::CborStore;
use std::collections::HashMap;

// array of required actors
pub const REQUIRED_ACTORS: &[&str] = &[
    BLOBS_ACTOR_NAME,
    BLOB_READER_ACTOR_NAME,
    BUCKET_ACTOR_NAME,
    CHAINMETADATA_ACTOR_NAME,
    GAS_MARKET_EIP1559_ACTOR_NAME,
    RECALL_CONFIG_ACTOR_NAME,
    IPC_EAM_ACTOR_NAME,
    TIMEHUB_ACTOR_NAME,
];

/// A mapping of internal actor CIDs to their respective types.
pub struct Manifest {
    code_by_name: HashMap<String, Cid>,
}

impl Manifest {
    /// Load a manifest from the blockstore.
    pub fn load<B: Blockstore>(bs: &B, root_cid: &Cid, ver: u32) -> anyhow::Result<Manifest> {
        if ver != 1 {
            return Err(anyhow!("unsupported manifest version {}", ver));
        }

        let vec: Vec<(String, Cid)> = match bs.get_cbor(root_cid)? {
            Some(vec) => vec,
            None => {
                return Err(anyhow!("cannot find manifest root cid {}", root_cid));
            }
        };

        Manifest::new(vec)
    }

    /// Construct a new manifest from actor name/cid tuples.
    pub fn new(iter: impl IntoIterator<Item = (impl Into<String>, Cid)>) -> anyhow::Result<Self> {
        let mut code_by_name = HashMap::new();
        for (name, code_cid) in iter.into_iter() {
            code_by_name.insert(name.into(), code_cid);
        }

        // loop over required actors and ensure they are present
        for &name in REQUIRED_ACTORS.iter() {
            let _ = code_by_name
                .get(name)
                .with_context(|| format!("manifest missing required actor {}", name))?;
        }

        Ok(Self { code_by_name })
    }

    /// Return a manifest subset from actor names.
    pub fn get_subset(&self, names: Vec<&str>) -> HashMap<String, Cid> {
        let mut code_by_name: HashMap<String, Cid> = HashMap::new();
        for name in names {
            let code_cid = self
                .code_by_name(name)
                .unwrap_or_else(|| panic!("actor {} not in manifest", name));
            code_by_name.insert(name.into(), *code_cid);
        }
        code_by_name
    }

    pub fn code_by_name(&self, str: &str) -> Option<&Cid> {
        self.code_by_name.get(str)
    }
}
