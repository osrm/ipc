// Copyright 2024 Textile
// Copyright 2021-2023 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound::{Included, Unbounded};

use fendermint_actor_blobs_shared::params::GetStatsReturn;
use fendermint_actor_blobs_shared::state::{
    Account, Blob, BlobStatus, CreditApproval, Hash, PublicKey, Subscription, SubscriptionGroup,
    SubscriptionId,
};
use fil_actors_runtime::ActorError;
use fvm_ipld_encoding::tuple::*;
use fvm_shared::bigint::{BigInt, BigUint};
use fvm_shared::clock::ChainEpoch;
use fvm_shared::econ::TokenAmount;
use fvm_shared::{address::Address, MethodNum};
use log::{debug, warn};
use num_traits::{Signed, ToPrimitive, Zero};
use sha2::{Digest, Sha256};
/// The minimum epoch duration a blob can be stored.
const MIN_TTL: ChainEpoch = 3600; // one hour
/// The rolling epoch duration used for non-expiring blobs.
const AUTO_TTL: ChainEpoch = 3600; // one hour

/// The state represents all accounts and stored blobs.
#[derive(Debug, Serialize_tuple, Deserialize_tuple)]
pub struct State {
    /// The total storage capacity of the subnet.
    pub capacity_total: BigInt,
    /// The total used storage capacity of the subnet.
    pub capacity_used: BigInt,
    /// The total number of credits sold in the subnet.
    pub credit_sold: BigInt,
    /// The total number of credits committed to active storage in the subnet.
    pub credit_committed: BigInt,
    /// The total number of credits debited in the subnet.
    pub credit_debited: BigInt,
    /// The byte-blocks per atto token rate set at genesis.
    pub credit_debit_rate: u64,
    /// Map containing all accounts by robust (non-ID) actor address.
    pub accounts: HashMap<Address, Account>,
    /// Map containing all blobs.
    pub blobs: HashMap<Hash, Blob>,
    /// Map of expiries to blob hashes.
    pub expiries: BTreeMap<ChainEpoch, HashMap<Address, HashMap<ExpiryKey, bool>>>,
    /// Map of currently added blob hashes to account and source Iroh node IDs.
    pub added: BTreeMap<Hash, HashSet<(Address, SubscriptionId, PublicKey)>>,
    /// Map of currently pending blob hashes to account and source Iroh node IDs.
    pub pending: BTreeMap<Hash, HashSet<(Address, SubscriptionId, PublicKey)>>,
}

/// Key used to namespace subscriptions in the expiry index.
#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize_tuple, Deserialize_tuple)]
pub struct ExpiryKey {
    /// Key hash.
    pub hash: Hash,
    /// Key subscription ID.
    pub id: SubscriptionId,
}

impl ExpiryKey {
    /// Create a new expiry key.
    pub fn new(hash: Hash, id: &SubscriptionId) -> Self {
        Self {
            hash,
            id: id.clone(),
        }
    }
}

/// Helper for handling credit approvals.
struct CreditDelegation<'a> {
    /// The address that is submitting the transaction to add this blob.
    pub origin: Address,
    /// The address that is calling into the blob actor.
    /// If the blob actor is accessed directly, this will be the same as "origin".
    /// However, most of the time this will be the address of the actor instance that is
    /// calling into the blobs actor, i.e., a specific Bucket or Timehub instance.
    pub caller: Address,
    /// Information about the approval that allows "origin" to use credits via "caller".
    /// Note that the Address that has issued this approval (the subscriber/sponsor), and whose
    /// credits are being allowed to be used, are not stored internal to this struct.
    pub approval: &'a mut CreditApproval,
}

impl<'a> CreditDelegation<'a> {
    pub fn new(origin: Address, caller: Address, approval: &'a mut CreditApproval) -> Self {
        Self {
            origin,
            caller,
            approval,
        }
    }

    /// Tuple of (Origin, Caller) addresses.
    pub fn addresses(&self) -> (Address, Address) {
        (self.origin, self.caller)
    }
}

fn pad_to_32_bytes(input: &[u8]) -> [u8; 32] {
    let mut padded = [0u8; 32];
    let start = 32_usize.saturating_sub(input.len());
    padded[start..].copy_from_slice(&input[..input.len().min(32)]);
    padded
}

impl State {
    pub fn new(capacity: u64, credit_debit_rate: u64) -> Self {
        Self {
            capacity_total: BigInt::from(capacity),
            capacity_used: BigInt::zero(),
            credit_sold: BigInt::zero(),
            credit_committed: BigInt::zero(),
            credit_debited: BigInt::zero(),
            credit_debit_rate,
            accounts: HashMap::new(),
            blobs: HashMap::new(),
            expiries: BTreeMap::new(),
            added: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }

    // TODO: Don't calculate stats on the fly, use running counters.
    pub fn get_stats(&self, balance: TokenAmount) -> GetStatsReturn {
        GetStatsReturn {
            balance,
            capacity_free: self.capacity_available(),
            capacity_used: self.capacity_used.clone(),
            credit_sold: self.credit_sold.clone(),
            credit_committed: self.credit_committed.clone(),
            credit_debited: self.credit_debited.clone(),
            credit_debit_rate: self.credit_debit_rate,
            num_accounts: self.accounts.len() as u64,
            num_blobs: self.blobs.len() as u64,
            num_resolving: self.pending.len() as u64,
            bytes_resolving: self.pending.keys().map(|hash| self.blobs[hash].size).sum(),
            num_added: self.added.len() as u64,
            bytes_added: self.added.keys().map(|hash| self.blobs[hash].size).sum(),
        }
    }

    pub fn buy_credit(
        &mut self,
        recipient: Address,
        amount: TokenAmount,
        current_epoch: ChainEpoch,
    ) -> anyhow::Result<Account, ActorError> {
        if amount.is_negative() {
            return Err(ActorError::illegal_argument(
                "token amount must be positive".into(),
            ));
        }
        let credits = self.credit_debit_rate * amount.atto();
        // Don't sell credits if we're at storage capacity
        if self.capacity_available().is_zero() {
            return Err(ActorError::forbidden(
                "credits not available (subnet has reached storage capacity)".into(),
            ));
        }
        self.credit_sold += &credits;
        let account = self
            .accounts
            .entry(recipient)
            .and_modify(|a| a.credit_free += &credits)
            .or_insert(Account::new(credits.clone(), current_epoch));
        Ok(account.clone())
    }

    pub fn approve_credit(
        &mut self,
        from: Address,
        to: Address,
        require_caller: Option<Address>,
        current_epoch: ChainEpoch,
        limit: Option<BigUint>,
        ttl: Option<ChainEpoch>,
    ) -> anyhow::Result<CreditApproval, ActorError> {
        let limit = limit.map(BigInt::from);
        if let Some(ttl) = ttl {
            if ttl < MIN_TTL {
                return Err(ActorError::illegal_argument(format!(
                    "minimum approval TTL is {}",
                    MIN_TTL
                )));
            }
        }
        let expiry = ttl.map(|t| t + current_epoch);
        let account = self
            .accounts
            .entry(from)
            .or_insert(Account::new(BigInt::zero(), current_epoch));
        // Get or add a new approval
        let caller = require_caller.unwrap_or(to);
        let approval = account
            .approvals
            .entry(to)
            .or_default()
            .entry(caller)
            .or_insert(CreditApproval {
                limit: limit.clone(),
                expiry,
                used: BigInt::zero(),
            });
        // Validate approval changes
        if let Some(limit) = limit.clone() {
            if approval.used > limit {
                return Err(ActorError::illegal_argument(format!(
                    "limit cannot be less than amount of already spent credits ({})",
                    approval.used
                )));
            }
        }
        approval.limit = limit;
        approval.expiry = expiry;
        Ok(approval.clone())
    }

    /// Returns the CreditApproval if one exists from the given address, to the given address,
    /// for the given caller, or None if no approval exists.
    pub fn get_credit_approval(
        &self,
        from: Address,
        receiver: Address,
        caller: Address,
    ) -> Option<CreditApproval> {
        let account = match self.accounts.get(&from) {
            None => return None,
            Some(account) => account,
        };
        // First look for an approval for "to" keyed by "to", which denotes it's valid for
        // any caller.
        // Second look for an approval for the supplied caller.
        let approval = if let Some(approvals) = account.approvals.get(&receiver) {
            if let Some(approval) = approvals.get(&receiver) {
                Some(approval)
            } else {
                approvals.get(&caller)
            }
        } else {
            None
        };
        approval.cloned()
    }

    pub fn revoke_credit(
        &mut self,
        from: Address,
        to: Address,
        require_caller: Option<Address>,
    ) -> anyhow::Result<(), ActorError> {
        let account = self
            .accounts
            .get_mut(&from)
            .ok_or(ActorError::not_found(format!("account {} not found", from)))?;
        let caller = require_caller.unwrap_or(to);
        if let Some(approvals) = account.approvals.get_mut(&to) {
            approvals.remove(&caller);
            if approvals.is_empty() {
                account.approvals.remove(&to);
            }
        }
        Ok(())
    }

    pub fn get_account(&self, address: Address) -> Option<Account> {
        self.accounts.get(&address).cloned()
    }

    #[allow(clippy::type_complexity)]
    pub fn debit_accounts(
        &mut self,
        current_epoch: ChainEpoch,
    ) -> anyhow::Result<HashSet<Hash>, ActorError> {
        // Delete expired subscriptions
        let mut delete_from_disc = HashSet::new();
        let expiries: Vec<(ChainEpoch, HashMap<Address, HashMap<ExpiryKey, bool>>)> = self
            .expiries
            .range((Unbounded, Included(current_epoch)))
            .map(|(expiry, entry)| (*expiry, entry.clone()))
            .collect();
        let mut num_renewed = 0;
        let mut num_deleted = 0;
        for (_, entry) in expiries {
            for (subscriber, subs) in entry {
                for (key, auto_renew) in subs {
                    if auto_renew {
                        if let Err(e) =
                            self.renew_blob(subscriber, current_epoch, key.hash, key.id.clone())
                        {
                            // Warn and skip down to delete
                            warn!(
                                "failed to renew blob {} for {} (id: {}): {}",
                                key.hash, subscriber, key.id, e
                            );
                        } else {
                            num_renewed += 1;
                            continue;
                        }
                    }
                    match self.delete_blob(
                        subscriber,
                        subscriber,
                        subscriber,
                        current_epoch,
                        key.hash,
                        key.id.clone(),
                    ) {
                        Ok(from_disc) => {
                            num_deleted += 1;
                            if from_disc {
                                delete_from_disc.insert(key.hash);
                            }
                        }
                        Err(e) => {
                            warn!(
                                "failed to delete blob {} for {} (id: {}): {}",
                                key.hash, subscriber, key.id, e
                            )
                        }
                    }
                }
            }
        }
        debug!("renewed {} expired subscriptions", num_renewed);
        debug!("deleted {} expired subscriptions", num_deleted);
        debug!(
            "{} blobs marked for deletion from disc",
            delete_from_disc.len()
        );
        // Debit for existing usage
        for (address, account) in self.accounts.iter_mut() {
            let debit_blocks = current_epoch - account.last_debit_epoch;
            let debit = debit_blocks as u64 * &account.capacity_used;
            self.credit_debited += &debit;
            self.credit_committed -= &debit;
            account.credit_committed -= &debit;
            account.last_debit_epoch = current_epoch;
            debug!("debited {} credits from {}", debit, address);
        }
        Ok(delete_from_disc)
    }

    /// Add a blob.
    ///
    /// @param origin - The address that is submitting the transaction to add this blob.
    /// @param caller - The address that is calling into this function.
    //    If the blob actor is accessed directly, this will be the same as "origin".
    //    However, most of the time this will be the address of the actor instance that is
    //    calling into the blobs actor, i.e., a specific Bucket or Timehub instance.
    /// @param subscriber - The address responsible for the subscription to keep this blob around.
    ///   This is whose credits will be spent by this transaction, and going forward to continue to
    ///   pay for the blob over time. Generally, this is the owner of the wrapping Actor
    ///   (e.g., Buckets, Timehub).
    #[allow(clippy::too_many_arguments)]
    pub fn add_blob(
        &mut self,
        origin: Address,
        caller: Address,
        subscriber: Address,
        current_epoch: ChainEpoch,
        hash: Hash,
        metadata_hash: Hash,
        id: SubscriptionId,
        size: u64,
        ttl: Option<ChainEpoch>,
        source: PublicKey,
        tokens_received: TokenAmount,
    ) -> anyhow::Result<(Subscription, TokenAmount), ActorError> {
        let (ttl, auto_renew) = accept_ttl(ttl)?;
        let account = self
            .accounts
            .entry(subscriber)
            .or_insert(Account::new(BigInt::zero(), current_epoch));
        let delegation = if origin != subscriber {
            // First look for an approval for origin keyed by origin, which denotes it's valid for
            // any caller.
            // Second look for an approval for the supplied caller.
            let approval = if let Some(approvals) = account.approvals.get_mut(&origin) {
                if let Some(approval) = approvals.get_mut(&origin) {
                    Some(approval)
                } else {
                    approvals.get_mut(&caller)
                }
            } else {
                None
            };
            let approval = approval.ok_or(ActorError::forbidden(format!(
                "approval from {} to {} via caller {} not found",
                subscriber, origin, caller
            )))?;
            Some(CreditDelegation::new(origin, caller, approval))
        } else {
            None
        };
        // Capacity updates and required credit depend on whether the subscriber is already
        // subcribing to this blob
        let size = BigInt::from(size);
        let expiry = current_epoch + ttl;
        let mut new_capacity = BigInt::zero();
        let mut new_account_capacity = BigInt::zero();
        let credit_required: BigInt;
        // Like cashback but for sending unspent tokens back
        let tokens_unspent: TokenAmount;
        let sub = if let Some(blob) = self.blobs.get_mut(&hash) {
            let sub = if let Some(group) = blob.subscribers.get_mut(&subscriber) {
                let (group_expiry, new_group_expiry) = group.max_expiries(&id, Some(expiry));
                // If the subscriber has been debited after the group's max expiry, we need to
                // clean up the accounting with a refund.
                // If the ensure-credit check below fails, the refund won't be saved in the
                // subscriber's state.
                // However, they will get rerefunded during the next auto debit tick.
                if let Some(group_expiry) = group_expiry {
                    if account.last_debit_epoch > group_expiry {
                        // The refund extends up to the current epoch because we need to
                        // account for the charge that will happen below at the current epoch.
                        let refund_blocks = current_epoch - group_expiry;
                        let refund = refund_blocks as u64 * &size;
                        // Re-mint spent credit
                        self.credit_debited -= &refund;
                        self.credit_committed += &refund;
                        account.credit_committed += &refund;
                        debug!("refunded {} credits to {}", refund, subscriber);
                    }
                }
                // Ensure subscriber has enough credits, considering the subscription group may
                // have expiries that cover a portion of the addition.
                // Required credit can be negative if subscriber is reducing expiry.
                // When adding, the new group expiry will always contain a value.
                let new_group_expiry = new_group_expiry.unwrap();
                credit_required = if let Some(group_expiry) = group_expiry {
                    (new_group_expiry - group_expiry.max(current_epoch)) as u64 * &size
                } else {
                    (new_group_expiry - current_epoch) as u64 * &size
                };
                tokens_unspent = ensure_credit_or_buy(
                    &mut account.credit_free,
                    &mut self.credit_sold,
                    &credit_required,
                    &tokens_received,
                    self.credit_debit_rate,
                    &subscriber,
                    current_epoch,
                    &delegation,
                )?;
                if let Some(sub) = group.subscriptions.get_mut(&id) {
                    // Update expiry index
                    if expiry != sub.expiry {
                        update_expiry_index(
                            &mut self.expiries,
                            subscriber,
                            hash,
                            &id,
                            Some((expiry, auto_renew)),
                            Some(sub.expiry),
                        );
                    }
                    sub.expiry = expiry;
                    sub.auto_renew = auto_renew;
                    // Overwrite source allows subscriber to retry resolving
                    sub.source = source;
                    sub.delegate = delegation.as_ref().map(|d| d.addresses());
                    sub.failed = false;
                    debug!(
                        "updated subscription to blob {} for {} (key: {})",
                        hash, subscriber, id
                    );
                    sub.clone()
                } else {
                    // Add new subscription
                    let sub = Subscription {
                        added: current_epoch,
                        expiry,
                        auto_renew,
                        source,
                        delegate: delegation.as_ref().map(|d| d.addresses()),
                        failed: false,
                    };
                    group.subscriptions.insert(id.clone(), sub.clone());
                    debug!(
                        "created new subscription to blob {} for {} (key: {})",
                        hash, subscriber, id
                    );
                    // Update expiry index
                    update_expiry_index(
                        &mut self.expiries,
                        subscriber,
                        hash,
                        &id,
                        Some((expiry, auto_renew)),
                        None,
                    );
                    sub
                }
            } else {
                new_account_capacity = size.clone();
                // One or more accounts have already committed credit.
                // However, we still need to reserve the full required credit from the new
                // subscriber, as the existing account(s) may decide to change the expiry or cancel.
                credit_required = ttl as u64 * &size;
                tokens_unspent = ensure_credit_or_buy(
                    &mut account.credit_free,
                    &mut self.credit_sold,
                    &credit_required,
                    &tokens_received,
                    self.credit_debit_rate,
                    &subscriber,
                    current_epoch,
                    &delegation,
                )?;
                // Add new subscription
                let sub = Subscription {
                    added: current_epoch,
                    expiry,
                    auto_renew,
                    source,
                    delegate: delegation.as_ref().map(|d| d.addresses()),
                    failed: false,
                };
                blob.subscribers.insert(
                    subscriber,
                    SubscriptionGroup {
                        subscriptions: HashMap::from([(id.clone(), sub.clone())]),
                    },
                );
                debug!(
                    "created new subscription to blob {} for {} (key: {})",
                    hash, subscriber, id
                );
                // Update expiry index
                update_expiry_index(
                    &mut self.expiries,
                    subscriber,
                    hash,
                    &id,
                    Some((expiry, auto_renew)),
                    None,
                );
                sub
            };
            if !matches!(blob.status, BlobStatus::Resolved) {
                // It's pending or failed, reset to added status
                blob.status = BlobStatus::Added;
                // Add/update added with hash and its source
                self.added
                    .entry(hash)
                    .and_modify(|sources| {
                        sources.insert((subscriber, id.clone(), source));
                    })
                    .or_insert(HashSet::from([(subscriber, id, source)]));
            }
            sub
        } else {
            new_account_capacity = size.clone();
            // New blob increases network capacity as well.
            // Ensure there is enough capacity available.
            let available_capacity = &self.capacity_total - &self.capacity_used;
            if size > available_capacity {
                return Err(ActorError::forbidden(format!(
                    "subnet has insufficient storage capacity (available: {}; required: {})",
                    available_capacity, size
                )));
            }
            new_capacity = size.clone();
            credit_required = ttl as u64 * &size;
            tokens_unspent = ensure_credit_or_buy(
                &mut account.credit_free,
                &mut self.credit_sold,
                &credit_required,
                &tokens_received,
                self.credit_debit_rate,
                &subscriber,
                current_epoch,
                &delegation,
            )?;
            // Create new blob
            let sub = Subscription {
                added: current_epoch,
                expiry,
                auto_renew,
                source,
                delegate: delegation.as_ref().map(|d| d.addresses()),
                failed: false,
            };
            let blob = Blob {
                size: size.to_u64().unwrap(),
                metadata_hash,
                subscribers: HashMap::from([(
                    subscriber,
                    SubscriptionGroup {
                        subscriptions: HashMap::from([(id.clone(), sub.clone())]),
                    },
                )]),
                status: BlobStatus::Added,
            };
            self.blobs.insert(hash, blob);
            debug!("created new blob {}", hash);
            debug!(
                "created new subscription to blob {} for {} (key: {})",
                hash, subscriber, id
            );
            // Update expiry index
            update_expiry_index(
                &mut self.expiries,
                subscriber,
                hash,
                &id,
                Some((expiry, auto_renew)),
                None,
            );
            // Add to added
            self.added
                .insert(hash, HashSet::from([(subscriber, id, source)]));
            sub
        };
        // Account capacity is changing, debit for existing usage
        let debit_blocks = current_epoch - account.last_debit_epoch;
        let debit = debit_blocks as u64 * &account.capacity_used;
        self.credit_debited += &debit;
        self.credit_committed -= &debit;
        account.credit_committed -= &debit;
        account.last_debit_epoch = current_epoch;
        debug!("debited {} credits from {}", debit, subscriber);
        // Account for new size and move free credit to committed credit
        self.capacity_used += &new_capacity;
        debug!("used {} bytes from subnet", new_account_capacity);
        account.capacity_used += &new_account_capacity;
        debug!("used {} bytes from {}", new_account_capacity, subscriber);
        self.credit_committed += &credit_required;
        account.credit_committed += &credit_required;
        account.credit_free -= &credit_required;
        // Update credit approval
        if let Some(delegation) = delegation {
            delegation.approval.used += &credit_required;
        }
        if credit_required.is_positive() {
            debug!("committed {} credits from {}", credit_required, subscriber);
        } else {
            debug!(
                "released {} credits to {}",
                credit_required.magnitude(),
                subscriber
            );
        }
        Ok((sub, tokens_unspent))
    }

    fn renew_blob(
        &mut self,
        subscriber: Address,
        current_epoch: ChainEpoch,
        hash: Hash,
        id: SubscriptionId,
    ) -> anyhow::Result<Account, ActorError> {
        let account = self
            .accounts
            .entry(subscriber)
            .or_insert(Account::new(BigInt::zero(), current_epoch));
        let blob = self
            .blobs
            .get_mut(&hash)
            .ok_or(ActorError::not_found(format!("blob {} not found", hash)))?;
        if matches!(blob.status, BlobStatus::Failed) {
            // Do not renew failed blobs.
            return Err(ActorError::illegal_state(format!(
                "cannot renew failed blob {}",
                hash
            )));
        }
        let group = blob
            .subscribers
            .get_mut(&subscriber)
            .ok_or(ActorError::forbidden(format!(
                "subscriber {} is not subscribed to blob {}",
                subscriber, hash
            )))?;
        // Renewal must begin at the current epoch to avoid potential issues with auto-renewal.
        // Simply adding TTL to the current max expiry could result in an expiry date that's not
        // truly in the future, depending on how frequently auto-renewal occurs.
        // We'll ensure below that past unpaid for blocks are accounted for.
        let expiry = current_epoch + AUTO_TTL;
        let (group_expiry, new_group_expiry) = group.max_expiries(&id, Some(expiry));
        let sub = group
            .subscriptions
            .get_mut(&id)
            .ok_or(ActorError::not_found(format!(
                "subscription id {} not found",
                id.clone()
            )))?;
        let delegation = if let Some((origin, caller)) = sub.delegate {
            // First look for an approval for origin keyed by origin, which denotes it's valid for
            // any caller.
            // Second look for an approval for the supplied caller.
            let approval = if let Some(approvals) = account.approvals.get_mut(&origin) {
                if let Some(approval) = approvals.get_mut(&origin) {
                    Some(approval)
                } else {
                    approvals.get_mut(&caller)
                }
            } else {
                None
            };
            let approval = approval.ok_or(ActorError::forbidden(format!(
                "approval from {} to {} via caller {} not found",
                subscriber, origin, caller
            )))?;
            Some(CreditDelegation::new(origin, caller, approval))
        } else {
            None
        };
        // If the subscriber has been debited after the group's max expiry, we need to
        // clean up the accounting with a refund.
        // We could just account for the refund amount when ensuring credit below, but if that
        // fails, the overcharge would still exist.
        // When renewing, the existing group expiry will always contain a value.
        let group_expiry = group_expiry.unwrap();
        let size = BigInt::from(blob.size);
        if account.last_debit_epoch > group_expiry {
            // The refund extends up to the last debit epoch
            let refund_blocks = account.last_debit_epoch - group_expiry;
            let refund = refund_blocks as u64 * &size;
            // Re-mint spent credit
            self.credit_debited -= &refund;
            self.credit_committed += &refund;
            account.credit_committed += &refund;
            debug!("refunded {} credits to {}", refund, subscriber);
        }
        // Ensure subscriber has enough credits, considering the subscription group may
        // have expiries that cover a portion of the renewal.
        // Required credit can be negative if subscriber is reducing expiry.
        // When renewing, the new group expiry will always contain a value.
        // There may be a gap between the existing expiry and the last debit that will make
        // the renewal discontinuous.
        let new_group_expiry = new_group_expiry.unwrap();
        let credit_required =
            (new_group_expiry - group_expiry.max(account.last_debit_epoch)) as u64 * &size;
        ensure_credit(
            &subscriber,
            current_epoch,
            &account.credit_free,
            &credit_required,
            &delegation,
        )?;
        // Update expiry index
        if expiry != sub.expiry {
            update_expiry_index(
                &mut self.expiries,
                subscriber,
                hash,
                &id,
                Some((expiry, sub.auto_renew)),
                Some(sub.expiry),
            );
        }
        sub.expiry = expiry;
        debug!(
            "renewed subscription to blob {} for {} (key: {})",
            hash, subscriber, id
        );
        // Move free credit to committed credit
        self.credit_committed += &credit_required;
        account.credit_committed += &credit_required;
        account.credit_free -= &credit_required;
        // Update credit approval
        if let Some(delegation) = delegation {
            delegation.approval.used += &credit_required;
        }
        debug!("committed {} credits from {}", credit_required, subscriber);
        Ok(account.clone())
    }

    pub fn get_blob(&self, hash: Hash) -> Option<Blob> {
        self.blobs.get(&hash).cloned()
    }

    pub fn get_blob_status(
        &self,
        subscriber: Address,
        hash: Hash,
        id: SubscriptionId,
    ) -> Option<BlobStatus> {
        let blob = self.blobs.get(&hash)?;
        if blob.subscribers.contains_key(&subscriber) {
            match blob.status {
                BlobStatus::Added => Some(BlobStatus::Added),
                BlobStatus::Pending => Some(BlobStatus::Pending),
                BlobStatus::Resolved => Some(BlobStatus::Resolved),
                BlobStatus::Failed => {
                    // The blob state's status may have been finalized as failed by another
                    // subscription.
                    // We need to see if this specific subscription failed.
                    if let Some(sub) = blob
                        .subscribers
                        .get(&subscriber)
                        .unwrap() // safe here
                        .subscriptions
                        .get(&id)
                    {
                        if sub.failed {
                            Some(BlobStatus::Failed)
                        } else {
                            Some(BlobStatus::Pending)
                        }
                    } else {
                        None
                    }
                }
            }
        } else {
            None
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn get_added_blobs(
        &self,
        size: u32,
    ) -> Vec<(Hash, HashSet<(Address, SubscriptionId, PublicKey)>)> {
        self.added
            .iter()
            .take(size as usize)
            .map(|element| (*element.0, element.1.clone()))
            .collect::<Vec<_>>()
    }

    #[allow(clippy::type_complexity)]
    pub fn get_pending_blobs(
        &self,
        size: u32,
    ) -> Vec<(Hash, HashSet<(Address, SubscriptionId, PublicKey)>)> {
        self.pending
            .iter()
            .take(size as usize)
            .map(|element| (*element.0, element.1.clone()))
            .collect::<Vec<_>>()
    }

    pub fn set_blob_pending(
        &mut self,
        subscriber: Address,
        hash: Hash,
        id: SubscriptionId,
        source: PublicKey,
    ) -> anyhow::Result<(), ActorError> {
        let blob = if let Some(blob) = self.blobs.get_mut(&hash) {
            blob
        } else {
            // The blob may have been deleted before it was set to pending
            return Ok(());
        };
        blob.status = BlobStatus::Pending;
        // Add to pending
        self.pending
            .insert(hash, HashSet::from([(subscriber, id, source)]));
        // Remove from added
        self.added.remove(&hash);
        Ok(())
    }

    pub fn finalize_blob(
        &mut self,
        subscriber: Address,
        current_epoch: ChainEpoch,
        hash: Hash,
        id: SubscriptionId,
        status: BlobStatus,
    ) -> anyhow::Result<(), ActorError> {
        if matches!(status, BlobStatus::Added | BlobStatus::Pending) {
            return Err(ActorError::illegal_state(format!(
                "cannot finalize blob {} as added or pending",
                hash
            )));
        }
        let account = self
            .accounts
            .entry(subscriber)
            .or_insert(Account::new(BigInt::zero(), current_epoch));
        let blob = if let Some(blob) = self.blobs.get_mut(&hash) {
            blob
        } else {
            // The blob may have been deleted before it was finalized
            return Ok(());
        };
        if matches!(blob.status, BlobStatus::Added) {
            return Err(ActorError::illegal_state(format!(
                "blob {} cannot be finalized from status added",
                hash
            )));
        } else if matches!(blob.status, BlobStatus::Resolved) {
            // Blob is already finalized as resolved.
            // We can ignore later finalizations, even if they are failed.
            return Ok(());
        }
        let group = blob
            .subscribers
            .get_mut(&subscriber)
            .ok_or(ActorError::forbidden(format!(
                "subscriber {} is not subscribed to blob {}",
                subscriber, hash
            )))?;
        // Get max expiries with the current subscription removed in case we need them below.
        // We have to do this here to avoid breaking borrow rules.
        let (group_expiry, new_group_expiry) = group.max_expiries(&id, Some(0));
        let (sub_is_min_added, next_min_added) = group.is_min_added(&id)?;
        let sub = group
            .subscriptions
            .get_mut(&id)
            .ok_or(ActorError::not_found(format!(
                "subscription id {} not found",
                id.clone()
            )))?;
        // Do not error if the approval was removed while this blob was pending
        let delegation = if let Some((origin, caller)) = sub.delegate {
            // First look for an approval for origin keyed by origin, which denotes it's valid for
            // any caller.
            // Second look for an approval for the supplied caller.
            let approval = if let Some(approvals) = account.approvals.get_mut(&origin) {
                if let Some(approval) = approvals.get_mut(&origin) {
                    Some(approval)
                } else {
                    approvals.get_mut(&caller)
                }
            } else {
                None
            };
            approval.map(|approval| CreditDelegation::new(origin, caller, approval))
        } else {
            None
        };
        // Update blob status
        blob.status = status;
        debug!("finalized blob {} to status {}", hash, blob.status);
        if matches!(blob.status, BlobStatus::Failed) {
            let size = BigInt::from(blob.size);
            // We're not going to make a debit, but we need to refund any spent credits that may
            // have been used on this group in the event the last debit is later than the
            // added epoch.
            if account.last_debit_epoch > sub.added && sub_is_min_added {
                // The refund extends up to either the next minimum added epoch that is less
                // than the last debit epoch, or the last debit epoch.
                let refund_cutoff = next_min_added
                    .unwrap_or(account.last_debit_epoch)
                    .min(account.last_debit_epoch);
                let refund_blocks = refund_cutoff - sub.added;
                let refund = refund_blocks as u64 * &size;
                // Re-mint spent credit
                self.credit_debited -= &refund;
                account.credit_free += &refund; // move directly to free
                debug!("refunded {} credits to {}", refund, subscriber);
            }
            // If there's no new group expiry, all subscriptions have failed.
            if new_group_expiry.is_none() {
                // Account for reclaimed size and move committed credit to free credit
                self.capacity_used -= &size;
                debug!("released {} bytes to subnet", size);
                account.capacity_used -= &size;
                debug!("released {} bytes to {}", size, subscriber);
            }
            // Release credits considering other subscriptions may still be pending.
            // When failing, the existing group expiry will always contain a value.
            let group_expiry = group_expiry.unwrap();
            if account.last_debit_epoch < group_expiry {
                let reclaim = if let Some(new_group_expiry) = new_group_expiry {
                    (group_expiry - new_group_expiry.max(account.last_debit_epoch)) * &size
                } else {
                    (group_expiry - account.last_debit_epoch) * &size
                };
                self.credit_committed -= &reclaim;
                account.credit_committed -= &reclaim;
                account.credit_free += &reclaim;
                // Update credit approval
                if let Some(delegation) = delegation {
                    delegation.approval.used -= &reclaim;
                }
                debug!("released {} credits to {}", reclaim, subscriber);
            }
            sub.failed = true;
        }
        // Remove entry from pending
        if let Some(entry) = self.pending.get_mut(&hash) {
            entry.remove(&(subscriber, id, sub.source));
            if entry.is_empty() {
                self.pending.remove(&hash);
            }
        }
        Ok(())
    }

    pub fn delete_blob(
        &mut self,
        origin: Address,
        caller: Address,
        subscriber: Address,
        current_epoch: ChainEpoch,
        hash: Hash,
        id: SubscriptionId,
    ) -> anyhow::Result<bool, ActorError> {
        let account = self
            .accounts
            .entry(subscriber)
            .or_insert(Account::new(BigInt::zero(), current_epoch));
        let blob = if let Some(blob) = self.blobs.get_mut(&hash) {
            blob
        } else {
            // We could error here, but since this method is called from other actors,
            // they would need to be able to identify this specific case.
            // For example, the bucket actor may need to delete a blob while overwriting
            // an existing key.
            // However, the system may have already deleted the blob due to expiration or
            // insufficient funds.
            // We could use a custom error code, but this is easier.
            return Ok(false);
        };
        let num_subscribers = blob.subscribers.len();
        let group = blob
            .subscribers
            .get_mut(&subscriber)
            .ok_or(ActorError::forbidden(format!(
                "subscriber {} is not subscribed to blob {}",
                subscriber, hash
            )))?;
        let (group_expiry, new_group_expiry) = group.max_expiries(&id, Some(0));
        let sub = group
            .subscriptions
            .get(&id)
            .ok_or(ActorError::not_found(format!(
                "subscription id {} not found",
                id.clone()
            )))?;
        let delegation = if let Some((origin, caller)) = sub.delegate {
            // First look for an approval for origin keyed by origin, which denotes it's valid for
            // any caller.
            // Second look for an approval for the supplied caller.
            let approval = if let Some(approvals) = account.approvals.get_mut(&origin) {
                if let Some(approval) = approvals.get_mut(&origin) {
                    Some(approval)
                } else {
                    approvals.get_mut(&caller)
                }
            } else {
                None
            };
            if let Some(approval) = approval {
                Some(CreditDelegation::new(origin, caller, approval))
            } else {
                // Approval may have been removed, or this is a call from the system actor,
                // in which case the origin will be supplied as the subscriber
                if origin != subscriber {
                    return Err(ActorError::forbidden(format!(
                        "approval from {} to {} via caller {} not found",
                        subscriber, origin, caller
                    )));
                }
                None
            }
        } else {
            None
        };
        // If the subscription does not have a delegate, the caller must be the subscriber.
        // If the subscription has a delegate, it must be the caller or the
        // caller must be the subscriber.
        match &delegation {
            None => {
                if origin != subscriber {
                    return Err(ActorError::forbidden(format!(
                        "origin {} is not subscriber {} for blob {}",
                        caller, subscriber, hash
                    )));
                }
            }
            Some(delegation) => {
                if !(origin == delegation.origin && caller == delegation.caller)
                    && origin != subscriber
                {
                    return Err(ActorError::forbidden(format!(
                        "origin {} is not delegate origin {} or caller {} is not delegate caller {} or subscriber {} for blob {}",
                        origin, delegation.origin, caller, delegation.caller, subscriber, hash
                    )));
                }
                if let Some(expiry) = delegation.approval.expiry {
                    if expiry <= current_epoch {
                        return Err(ActorError::forbidden(format!(
                            "approval from {} to {} via caller {} expired",
                            subscriber, delegation.origin, delegation.caller
                        )));
                    }
                }
            }
        }
        // Since the charge will be for all the account's blobs, we can only
        // account for capacity up to this blob's expiry if it is less than
        // the current epoch.
        // When deleting, the existing group expiry will always contain a value.
        let group_expiry = group_expiry.unwrap();
        let debit_epoch = group_expiry.min(current_epoch);
        // Account capacity is changing, debit for existing usage.
        // It could be possible that debit epoch is less than the last debit,
        // in which case we need to refund for that duration.
        if account.last_debit_epoch < debit_epoch {
            let debit_blocks = debit_epoch - account.last_debit_epoch;
            let debit = debit_blocks as u64 * &account.capacity_used;
            self.credit_debited += &debit;
            self.credit_committed -= &debit;
            account.credit_committed -= &debit;
            account.last_debit_epoch = debit_epoch;
            debug!("debited {} credits from {}", debit, subscriber);
        } else {
            // The account was debited after this blob's expiry
            let refund_blocks = account.last_debit_epoch - group_expiry;
            let refund = refund_blocks as u64 * &BigInt::from(blob.size);
            // Re-mint spent credit
            self.credit_debited -= &refund;
            self.credit_committed += &refund;
            account.credit_committed += &refund;
            debug!("refunded {} credits to {}", refund, subscriber);
        }
        // Account for reclaimed size and move committed credit to free credit
        // If blob failed, capacity and committed credits have already been returned
        if !matches!(blob.status, BlobStatus::Failed) {
            let size = BigInt::from(blob.size);
            // If there's no new group expiry, we can reclaim capacity.
            if new_group_expiry.is_none() {
                account.capacity_used -= &size;
                if num_subscribers == 1 {
                    self.capacity_used -= &size;
                    debug!("released {} bytes to subnet", size);
                }
                debug!("released {} bytes to {}", size, subscriber);
            }
            // We can release credits if the new group expiry is in the future,
            // considering other subscriptions may still be active.
            if account.last_debit_epoch < group_expiry {
                let reclaim = if let Some(new_group_expiry) = new_group_expiry {
                    (group_expiry - new_group_expiry.max(account.last_debit_epoch)) * &size
                } else {
                    (group_expiry - account.last_debit_epoch) * &size
                };
                self.credit_committed -= &reclaim;
                account.credit_committed -= &reclaim;
                account.credit_free += &reclaim;
                // Update credit approval
                if let Some(delegation) = delegation {
                    delegation.approval.used -= &reclaim;
                }
                debug!("released {} credits to {}", reclaim, subscriber);
            }
        }
        // Update expiry index
        update_expiry_index(
            &mut self.expiries,
            subscriber,
            hash,
            &id,
            None,
            Some(sub.expiry),
        );
        // Remove entry from added
        if let Some(entry) = self.added.get_mut(&hash) {
            entry.remove(&(subscriber, id.clone(), sub.source));
            if entry.is_empty() {
                self.added.remove(&hash);
            }
        }
        // Remove entry from pending
        if let Some(entry) = self.pending.get_mut(&hash) {
            entry.remove(&(subscriber, id.clone(), sub.source));
            if entry.is_empty() {
                self.pending.remove(&hash);
            }
        }
        // Delete subscription
        group.subscriptions.remove(&id);
        debug!(
            "deleted subscription to blob {} for {} (key: {})",
            hash, subscriber, id
        );
        // Delete the group if empty
        let delete_blob = if group.subscriptions.is_empty() {
            blob.subscribers.remove(&subscriber);
            debug!("deleted subscriber {} to blob {}", subscriber, hash);
            // Delete or update blob
            let delete_blob = blob.subscribers.is_empty();
            if delete_blob {
                self.blobs.remove(&hash);
                debug!("deleted blob {}", hash);
            }
            delete_blob
        } else {
            false
        };
        Ok(delete_blob)
    }

    /// Return available capacity as a difference between `capacity_total` and `capacity_used`.
    fn capacity_available(&self) -> BigInt {
        &self.capacity_total - &self.capacity_used
    }
}

/// Check if `subscriber` has enough credits, including delegated credits.
fn ensure_credit(
    subscriber: &Address,
    current_epoch: ChainEpoch,
    credit_free: &BigInt,
    required_credit: &BigInt,
    delegation: &Option<CreditDelegation>,
) -> anyhow::Result<(), ActorError> {
    ensure_enough_credits(subscriber, credit_free, required_credit)?;
    ensure_delegated_credit(subscriber, current_epoch, required_credit, delegation)
}

/// Check if `subscriber` owns enough free credits.
fn ensure_enough_credits(
    subscriber: &Address,
    credit_free: &BigInt,
    required_credit: &BigInt,
) -> anyhow::Result<(), ActorError> {
    if credit_free >= required_credit {
        Ok(())
    } else {
        Err(ActorError::insufficient_funds(format!(
            "account {} has insufficient credit (available: {}; required: {})",
            subscriber, credit_free, required_credit
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn ensure_credit_or_buy(
    account_credit_free: &mut BigInt,
    state_credit_sold: &mut BigInt,
    credit_required: &BigInt,
    tokens_received: &TokenAmount,
    debit_credit_rate: u64,
    subscriber: &Address,
    current_epoch: ChainEpoch,
    delegate: &Option<CreditDelegation>,
) -> anyhow::Result<TokenAmount, ActorError> {
    let tokens_received_non_zero = !tokens_received.is_zero();
    let has_delegation = delegate.is_some();
    match (tokens_received_non_zero, has_delegation) {
        (true, true) => Err(ActorError::illegal_argument(format!(
            "can not buy credits inline for {}",
            subscriber,
        ))),
        (true, false) => {
            // Try buying credits for self
            let not_enough_credits = *account_credit_free < *credit_required;
            if not_enough_credits {
                let credits_needed = credit_required - &*account_credit_free;
                let tokens_needed_atto = &credits_needed / debit_credit_rate;
                let tokens_needed = TokenAmount::from_atto(tokens_needed_atto);
                if tokens_needed <= *tokens_received {
                    let tokens_to_rebate = tokens_received - tokens_needed;
                    *state_credit_sold += &credits_needed;
                    *account_credit_free += &credits_needed;
                    Ok(tokens_to_rebate)
                } else {
                    Err(ActorError::insufficient_funds(format!(
                        "account {} sent insufficient tokens (received: {}; required: {})",
                        subscriber, tokens_received, tokens_needed
                    )))
                }
            } else {
                Ok(TokenAmount::zero())
            }
        }
        (false, true) => {
            ensure_credit(
                subscriber,
                current_epoch,
                account_credit_free,
                credit_required,
                delegate,
            )?;
            Ok(TokenAmount::zero())
        }
        (false, false) => {
            ensure_credit(
                subscriber,
                current_epoch,
                account_credit_free,
                credit_required,
                delegate,
            )?;
            Ok(TokenAmount::zero())
        }
    }
}

fn ensure_delegated_credit(
    subscriber: &Address,
    current_epoch: ChainEpoch,
    required_credit: &BigInt,
    delegation: &Option<CreditDelegation>,
) -> anyhow::Result<(), ActorError> {
    if let Some(delegation) = delegation {
        if let Some(limit) = &delegation.approval.limit {
            let unused = &(limit - &delegation.approval.used);
            if unused < required_credit {
                return Err(ActorError::insufficient_funds(format!(
                    "approval from {} to {} via caller {} has insufficient credit (available: {}; required: {})",
                    subscriber, delegation.origin, delegation.caller, unused, required_credit
                )));
            }
        }
        if let Some(expiry) = delegation.approval.expiry {
            if expiry <= current_epoch {
                return Err(ActorError::forbidden(format!(
                    "approval from {} to {} via caller {} expired",
                    subscriber, delegation.origin, delegation.caller
                )));
            }
        }
    }
    Ok(())
}

fn update_expiry_index(
    expiries: &mut BTreeMap<ChainEpoch, HashMap<Address, HashMap<ExpiryKey, bool>>>,
    subscriber: Address,
    hash: Hash,
    id: &SubscriptionId,
    add: Option<(ChainEpoch, bool)>,
    remove: Option<ChainEpoch>,
) {
    if let Some((add, auto_renew)) = add {
        expiries
            .entry(add)
            .and_modify(|entry| {
                entry
                    .entry(subscriber)
                    .and_modify(|subs| {
                        subs.insert(ExpiryKey::new(hash, id), auto_renew);
                    })
                    .or_insert(HashMap::from([(ExpiryKey::new(hash, id), auto_renew)]));
            })
            .or_insert(HashMap::from([(
                subscriber,
                HashMap::from([(ExpiryKey::new(hash, id), auto_renew)]),
            )]));
    }
    if let Some(remove) = remove {
        if let Some(entry) = expiries.get_mut(&remove) {
            if let Some(subs) = entry.get_mut(&subscriber) {
                subs.remove(&ExpiryKey::new(hash, id));
                if subs.is_empty() {
                    entry.remove(&subscriber);
                }
            }
            if entry.is_empty() {
                expiries.remove(&remove);
            }
        }
    }
}

fn accept_ttl(ttl: Option<ChainEpoch>) -> anyhow::Result<(ChainEpoch, bool), ActorError> {
    let (ttl, auto_renew) = ttl.map(|ttl| (ttl, false)).unwrap_or((AUTO_TTL, true));
    if ttl < MIN_TTL {
        Err(ActorError::illegal_argument(format!(
            "minimum blob TTL is {}",
            MIN_TTL
        )))
    } else {
        Ok((ttl, auto_renew))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rand::RngCore;

    fn setup_logs() {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::EnvFilter;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .event_format(tracing_subscriber::fmt::format().with_line_number(true))
                    .with_writer(std::io::stdout),
            )
            .with(EnvFilter::from_default_env())
            .try_init()
            .ok();
    }

    fn new_hash(size: usize) -> (Hash, u64) {
        let mut rng = rand::thread_rng();
        let mut data = vec![0u8; size];
        rng.fill_bytes(&mut data);
        (
            Hash(*iroh_base::hash::Hash::new(&data).as_bytes()),
            size as u64,
        )
    }

    fn new_metadata_hash() -> Hash {
        let mut rng = rand::thread_rng();
        let mut data = vec![0u8; 8];
        rng.fill_bytes(&mut data);
        Hash(*iroh_base::hash::Hash::new(&data).as_bytes())
    }

    pub fn new_pk() -> PublicKey {
        let mut rng = rand::thread_rng();
        let mut data = [0u8; 32];
        rng.fill_bytes(&mut data);
        PublicKey(data)
    }

    // The state does not care about the address type.
    // We use an actor style (t/f2) addresses because they are straightforward to create.
    fn new_address() -> Address {
        let mut rng = rand::thread_rng();
        let mut data = vec![0u8; 32];
        rng.fill_bytes(&mut data);
        Address::new_actor(&data)
    }

    fn check_approval(account: Account, origin: Address, caller: Address, expect_used: BigInt) {
        if !account.approvals.is_empty() {
            let approvals = account.approvals.get(&origin).unwrap();
            let approval = if origin == caller {
                approvals.get(&origin).unwrap()
            } else {
                approvals.get(&caller).unwrap()
            };
            assert_eq!(approval.used, expect_used);
        }
    }

    #[test]
    fn test_buy_credit_success() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let recipient = new_address();
        let amount = TokenAmount::from_whole(1);

        let res = state.buy_credit(recipient, amount.clone(), 1);
        assert!(res.is_ok());
        let account = res.unwrap();
        let credit_sold = amount.atto() * state.credit_debit_rate;
        assert_eq!(account.credit_free, credit_sold);
        assert_eq!(state.credit_sold, credit_sold);
        assert_eq!(state.accounts.len(), 1);
    }

    #[test]
    fn test_buy_credit_negative_amount() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let recipient = new_address();
        let amount = TokenAmount::from_whole(-1);

        let res = state.buy_credit(recipient, amount, 1);
        assert!(res.is_err());
        assert_eq!(res.err().unwrap().msg(), "token amount must be positive");
    }

    #[test]
    fn test_buy_credit_at_capacity() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let recipient = new_address();
        let amount = TokenAmount::from_whole(1);

        state.capacity_used = BigInt::from(capacity);
        let res = state.buy_credit(recipient, amount, 1);
        assert!(res.is_err());
        assert_eq!(
            res.err().unwrap().msg(),
            "credits not available (subnet has reached storage capacity)"
        );
    }

    #[test]
    fn test_approve_credit_success() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let from = new_address();
        let to = new_address();
        let current_epoch = 1;

        // No limit or expiry
        let res = state.approve_credit(from, to, None, current_epoch, None, None);
        assert!(res.is_ok());
        let approval = res.unwrap();
        assert_eq!(approval.limit, None);
        assert_eq!(approval.expiry, None);

        // Add limit
        let limit = 1_000_000_000_000_000_000u64;
        let res = state.approve_credit(
            from,
            to,
            None,
            current_epoch,
            Some(BigUint::from(limit)),
            None,
        );
        assert!(res.is_ok());
        let approval = res.unwrap();
        assert_eq!(approval.limit, Some(BigInt::from(limit)));
        assert_eq!(approval.expiry, None);

        // Add ttl
        let ttl = ChainEpoch::from(MIN_TTL);
        let res = state.approve_credit(
            from,
            to,
            None,
            current_epoch,
            Some(BigUint::from(limit)),
            Some(ttl),
        );
        assert!(res.is_ok());
        let approval = res.unwrap();
        assert_eq!(approval.limit, Some(BigInt::from(limit)));
        assert_eq!(approval.expiry, Some(ttl + current_epoch));

        // Require caller
        let require_caller = new_address();
        let res = state.approve_credit(from, to, Some(require_caller), current_epoch, None, None);
        assert!(res.is_ok());

        // Check the account approvals
        let account = state.get_account(from).unwrap();
        assert_eq!(account.approvals.len(), 1);
        let approvals = account.approvals.get(&to).unwrap();
        assert!(approvals.contains_key(&to));
        assert!(approvals.contains_key(&require_caller));
    }

    #[test]
    fn test_approve_credit_invalid_ttl() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let from = new_address();
        let to = new_address();
        let current_epoch = 1;

        let ttl = ChainEpoch::from(MIN_TTL - 1);
        let res = state.approve_credit(from, to, None, current_epoch, None, Some(ttl));
        assert!(res.is_err());
        assert_eq!(
            res.err().unwrap().msg(),
            format!("minimum approval TTL is {}", MIN_TTL)
        );
    }

    #[test]
    fn test_approve_credit_insufficient_credit() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let from = new_address();
        let to = new_address();
        let current_epoch = 1;

        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(from, amount.clone(), current_epoch)
            .unwrap();
        let res = state.approve_credit(from, to, None, current_epoch, None, None);
        assert!(res.is_ok());

        // Add a blob
        let (hash, size) = new_hash(1024);
        let res = state.add_blob(
            to,
            to,
            from,
            current_epoch,
            hash,
            new_metadata_hash(),
            SubscriptionId::Default,
            size,
            None,
            new_pk(),
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check approval
        let account = state.get_account(from).unwrap();
        let approval = account.approvals.get(&to).unwrap().get(&to).unwrap();
        assert_eq!(account.credit_committed, approval.used);

        // Try to update approval with a limit below what's already been committed
        let limit = 1_000u64;
        let res = state.approve_credit(
            from,
            to,
            None,
            current_epoch,
            Some(BigUint::from(limit)),
            None,
        );
        assert!(res.is_err());
        assert_eq!(
            res.err().unwrap().msg(),
            format!(
                "limit cannot be less than amount of already spent credits ({})",
                approval.used
            )
        );
    }

    #[test]
    fn test_revoke_credit_success() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let from = new_address();
        let to = new_address();
        let current_epoch = 1;

        let res = state.approve_credit(from, to, None, current_epoch, None, None);
        assert!(res.is_ok());

        // Add another and require caller
        let require_caller = new_address();
        let res = state.approve_credit(from, to, Some(require_caller), current_epoch, None, None);
        assert!(res.is_ok());

        // Check the account approvals
        let account = state.get_account(from).unwrap();
        assert_eq!(account.approvals.len(), 1);
        let approvals = account.approvals.get(&to).unwrap();
        assert!(approvals.contains_key(&to));
        assert!(approvals.contains_key(&require_caller));

        // Remove first
        let res = state.revoke_credit(from, to, None);
        assert!(res.is_ok());
        let account = state.get_account(from).unwrap();
        assert_eq!(account.approvals.len(), 1);
        let approvals = account.approvals.get(&to).unwrap();
        assert!(!approvals.contains_key(&to));
        assert!(approvals.contains_key(&require_caller));

        // Remove second
        let res = state.revoke_credit(from, to, Some(require_caller));
        assert!(res.is_ok());
        let account = state.get_account(from).unwrap();
        assert_eq!(account.approvals.len(), 0);
    }

    #[test]
    fn test_revoke_credit_account_not_found() {
        setup_logs();
        let capacity = 1024;
        let mut state = State::new(capacity, 1);
        let from = new_address();
        let to = new_address();

        let res = state.revoke_credit(from, to, None);
        assert!(res.is_err());
        assert_eq!(
            res.err().unwrap().msg(),
            format!("account {} not found", from)
        );
    }

    #[test]
    fn test_debit_accounts_delete_from_disc() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(origin, token_amount.clone(), current_epoch)
            .unwrap();
        debit_accounts_delete_from_disc(state, origin, origin, origin, current_epoch, token_amount);
    }

    #[test]
    fn test_debit_accounts_delete_from_disc_with_approval() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, token_amount.clone(), current_epoch)
            .unwrap();
        state
            .approve_credit(subscriber, origin, None, current_epoch, None, None)
            .unwrap();
        debit_accounts_delete_from_disc(
            state,
            origin,
            origin,
            subscriber,
            current_epoch,
            token_amount,
        );
    }

    fn debit_accounts_delete_from_disc(
        mut state: State,
        origin: Address,
        caller: Address,
        subscriber: Address,
        current_epoch: ChainEpoch,
        token_amount: TokenAmount,
    ) {
        let mut credit_amount = token_amount.atto() * state.credit_debit_rate;

        // Add blob with default a subscription ID
        let (hash, size) = new_hash(1024);
        let add1_epoch = current_epoch;
        let id1 = SubscriptionId::Default;
        let ttl1 = ChainEpoch::from(MIN_TTL);
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add1_epoch,
            hash,
            new_metadata_hash(),
            id1.clone(),
            size,
            Some(ttl1),
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Set to status pending
        let res = state.set_blob_pending(subscriber, hash, id1.clone(), source);
        assert!(res.is_ok());

        // Finalize as resolved
        let finalize_epoch = ChainEpoch::from(11);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            id1.clone(),
            BlobStatus::Resolved,
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add1_epoch);
        assert_eq!(account.credit_committed, BigInt::from(ttl1 as u64 * size));
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size));

        // Add the same blob but this time uses a different subscription ID
        let add2_epoch = ChainEpoch::from(21);
        let ttl2 = ChainEpoch::from(MIN_TTL);
        let id2 = SubscriptionId::Key(b"foo".to_vec());
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add2_epoch,
            hash,
            new_metadata_hash(),
            id2.clone(),
            size,
            Some(ttl2),
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch);
        assert_eq!(
            account.credit_committed, // stays the same becuase we're starting over
            BigInt::from(ttl2 as u64 * size),
        );
        credit_amount -= BigInt::from((add2_epoch - add1_epoch) as u64 * size);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size)); // not changed

        // Check the subscription group
        let blob = state.get_blob(hash).unwrap();
        let group = blob.subscribers.get(&subscriber).unwrap();
        assert_eq!(group.subscriptions.len(), 2);

        // Debit all accounts at an epoch between the two expiries (3601-3621)
        let debit_epoch = ChainEpoch::from(MIN_TTL + 11);
        let deletes_from_disc = state.debit_accounts(debit_epoch).unwrap();
        assert!(deletes_from_disc.is_empty());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, debit_epoch);
        assert_eq!(
            account.credit_committed, // debit reduces this
            BigInt::from((ttl2 - (debit_epoch - add2_epoch)) as u64 * size),
        );
        assert_eq!(account.credit_free, credit_amount); // not changed
        assert_eq!(account.capacity_used, BigInt::from(size)); // not changed

        // Check the subscription group
        let blob = state.get_blob(hash).unwrap();
        let group = blob.subscribers.get(&subscriber).unwrap();
        assert_eq!(group.subscriptions.len(), 1); // the first subscription was deleted

        // Debit all accounts at an epoch greater than group expiry (3621)
        let debit_epoch = ChainEpoch::from(MIN_TTL + 31);
        let deletes_from_disc = state.debit_accounts(debit_epoch).unwrap();
        assert!(!deletes_from_disc.is_empty()); // blob is marked for deletion

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, debit_epoch);
        assert_eq!(
            account.credit_committed, // the second debit reduces this to zero
            BigInt::from(0),
        );
        assert_eq!(account.credit_free, credit_amount); // not changed
        assert_eq!(account.capacity_used, BigInt::from(0));

        // Check state
        assert_eq!(state.credit_committed, BigInt::from(0)); // credit was released
        assert_eq!(
            state.credit_debited,
            token_amount.atto() - &account.credit_free
        );
        assert_eq!(state.capacity_used, BigInt::from(0)); // capacity was released

        // Check indexes
        assert_eq!(state.expiries.len(), 0);
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);

        // Check approval
        let account_committed = account.credit_committed.clone();
        check_approval(
            account,
            origin,
            caller,
            state.credit_debited + account_committed,
        );
    }

    #[test]
    fn test_add_blob_refund() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(origin, token_amount.clone(), current_epoch)
            .unwrap();
        add_blob_refund(state, origin, origin, origin, current_epoch, token_amount);
    }

    #[test]
    fn test_add_blob_refund_with_approval() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, token_amount.clone(), current_epoch)
            .unwrap();
        state
            .approve_credit(subscriber, origin, None, current_epoch, None, None)
            .unwrap();
        add_blob_refund(
            state,
            origin,
            origin,
            subscriber,
            current_epoch,
            token_amount,
        );
    }

    fn add_blob_refund(
        mut state: State,
        origin: Address,
        caller: Address,
        subscriber: Address,
        current_epoch: ChainEpoch,
        token_amount: TokenAmount,
    ) {
        let mut credit_amount = token_amount.atto() * state.credit_debit_rate;

        // Add blob with default a subscription ID
        let (hash1, size1) = new_hash(1024);
        let add1_epoch = current_epoch;
        let id1 = SubscriptionId::Default;
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add1_epoch,
            hash1,
            new_metadata_hash(),
            id1.clone(),
            size1,
            Some(MIN_TTL),
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add1_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(MIN_TTL as u64 * size1),
        );
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1));

        // Add another blob past the first blob's expiry
        let (hash2, size2) = new_hash(2048);
        let add2_epoch = ChainEpoch::from(MIN_TTL + 11);
        let id2 = SubscriptionId::Key(b"foo".to_vec());
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add2_epoch,
            hash2,
            new_metadata_hash(),
            id2.clone(),
            size2,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch);
        let blob1_expiry = ChainEpoch::from(MIN_TTL + add1_epoch);
        let overcharge = BigInt::from((add2_epoch - blob1_expiry) as u64 * size1);
        assert_eq!(
            account.credit_committed, // this includes an overcharge that needs to be refunded
            AUTO_TTL as u64 * size2 - overcharge,
        );
        credit_amount -= BigInt::from(AUTO_TTL as u64 * size2);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1 + size2));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            token_amount.atto() - (&account.credit_free + &account.credit_committed)
        );
        assert_eq!(state.capacity_used, account.capacity_used);

        // Check indexes
        assert_eq!(state.expiries.len(), 2);
        assert_eq!(state.added.len(), 2);
        assert_eq!(state.pending.len(), 0);

        // Add the first (now expired) blob again
        let add3_epoch = ChainEpoch::from(MIN_TTL + 21);
        let id1 = SubscriptionId::Default;
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add3_epoch,
            hash1,
            new_metadata_hash(),
            id1.clone(),
            size1,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add3_epoch);
        assert_eq!(
            account.credit_committed, // should not include overcharge due to refund
            BigInt::from(
                (AUTO_TTL - (add3_epoch - add2_epoch)) as u64 * size2 + AUTO_TTL as u64 * size1
            ),
        );
        credit_amount -= BigInt::from(AUTO_TTL as u64 * size1);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1 + size2));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            token_amount.atto() - (&account.credit_free + &account.credit_committed)
        );
        assert_eq!(state.capacity_used, account.capacity_used);

        // Check indexes
        assert_eq!(state.expiries.len(), 2);
        assert_eq!(state.added.len(), 2);
        assert_eq!(state.pending.len(), 0);

        // Check approval
        let account_committed = account.credit_committed.clone();
        check_approval(
            account,
            origin,
            caller,
            state.credit_debited + account_committed,
        );
    }

    #[test]
    fn test_add_blob_same_hash_same_account() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(origin, token_amount.clone(), current_epoch)
            .unwrap();
        add_blob_same_hash_same_account(state, origin, origin, origin, current_epoch, token_amount);
    }

    #[test]
    fn test_add_blob_same_hash_same_account_with_approval() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, token_amount.clone(), current_epoch)
            .unwrap();
        state
            .approve_credit(subscriber, origin, None, current_epoch, None, None)
            .unwrap();
        add_blob_same_hash_same_account(
            state,
            origin,
            origin,
            subscriber,
            current_epoch,
            token_amount,
        );
    }

    #[test]
    fn test_add_blob_same_hash_same_account_with_scoped_approval() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let caller = new_address();
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, token_amount.clone(), current_epoch)
            .unwrap();
        state
            .approve_credit(subscriber, origin, Some(caller), current_epoch, None, None)
            .unwrap();
        add_blob_same_hash_same_account(
            state,
            origin,
            caller,
            subscriber,
            current_epoch,
            token_amount,
        );
    }

    fn add_blob_same_hash_same_account(
        mut state: State,
        origin: Address,
        caller: Address,
        subscriber: Address,
        current_epoch: ChainEpoch,
        token_amount: TokenAmount,
    ) {
        let mut credit_amount = token_amount.atto() * state.credit_debit_rate;

        // Add blob with default a subscription ID
        let (hash, size) = new_hash(1024);
        let add1_epoch = current_epoch;
        let id1 = SubscriptionId::Default;
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add1_epoch,
            hash,
            new_metadata_hash(),
            id1.clone(),
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());
        let (sub, _) = res.unwrap();
        assert_eq!(sub.added, add1_epoch);
        assert_eq!(sub.expiry, add1_epoch + AUTO_TTL);
        assert!(sub.auto_renew);
        assert_eq!(sub.source, source);
        assert!(!sub.failed);
        if subscriber != origin {
            assert_eq!(sub.delegate, Some((origin, caller)));
        }

        // Check the blob status
        assert_eq!(
            state.get_blob_status(subscriber, hash, id1.clone()),
            Some(BlobStatus::Added)
        );

        // Check the blob
        let blob = state.get_blob(hash).unwrap();
        assert_eq!(blob.subscribers.len(), 1);
        assert_eq!(blob.status, BlobStatus::Added);
        assert_eq!(blob.size, size);

        // Check the subscription group
        let group = blob.subscribers.get(&subscriber).unwrap();
        assert_eq!(group.subscriptions.len(), 1);
        let got_sub = group.subscriptions.get(&id1.clone()).unwrap();
        assert_eq!(*got_sub, sub);

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add1_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(AUTO_TTL as u64 * size),
        );
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size));

        // Set to status pending
        let res = state.set_blob_pending(subscriber, hash, id1.clone(), source);
        assert!(res.is_ok());

        // Finalize as resolved
        let finalize_epoch = ChainEpoch::from(11);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            id1.clone(),
            BlobStatus::Resolved,
        );
        assert!(res.is_ok());
        assert_eq!(
            state.get_blob_status(subscriber, hash, id1.clone()),
            Some(BlobStatus::Resolved)
        );

        // Add the same blob again with a default subscription ID
        let add2_epoch = ChainEpoch::from(21);
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add2_epoch,
            hash,
            new_metadata_hash(),
            id1.clone(),
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());
        let (sub, _) = res.unwrap();
        assert_eq!(sub.added, add1_epoch); // added should not change
        assert_eq!(sub.expiry, add2_epoch + AUTO_TTL);
        assert!(sub.auto_renew);
        assert_eq!(sub.source, source);
        assert!(!sub.failed);
        if subscriber != origin {
            assert_eq!(sub.delegate, Some((origin, caller)));
        }

        // Check the blob status
        // Should already be resolved
        assert_eq!(
            state.get_blob_status(subscriber, hash, id1.clone()),
            Some(BlobStatus::Resolved)
        );

        // Check the blob
        let blob = state.get_blob(hash).unwrap();
        assert_eq!(blob.subscribers.len(), 1);
        assert_eq!(blob.status, BlobStatus::Resolved);
        assert_eq!(blob.size, size);

        // Check the subscription group
        let group = blob.subscribers.get(&subscriber).unwrap();
        assert_eq!(group.subscriptions.len(), 1); // Still only one subscription
        let got_sub = group.subscriptions.get(&id1.clone()).unwrap();
        assert_eq!(*got_sub, sub);

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch);
        assert_eq!(
            account.credit_committed, // stays the same becuase we're starting over
            BigInt::from(AUTO_TTL as u64 * size),
        );
        credit_amount -= BigInt::from((add2_epoch - add1_epoch) as u64 * size);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size)); // not changed

        // Add the same blob again but use a different subscription ID
        let add3_epoch = ChainEpoch::from(31);
        let id2 = SubscriptionId::Key(b"foo".to_vec());
        let source = new_pk();
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add3_epoch,
            hash,
            new_metadata_hash(),
            id2.clone(),
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());
        let (sub, _) = res.unwrap();
        assert_eq!(sub.added, add3_epoch);
        assert_eq!(sub.expiry, add3_epoch + AUTO_TTL);
        assert!(sub.auto_renew);
        assert_eq!(sub.source, source);
        assert!(!sub.failed);
        if subscriber != origin {
            assert_eq!(sub.delegate, Some((origin, caller)));
        }

        // Check the blob status
        // Should already be resolved
        assert_eq!(
            state.get_blob_status(subscriber, hash, id2.clone()),
            Some(BlobStatus::Resolved)
        );

        // Check the blob
        let blob = state.get_blob(hash).unwrap();
        assert_eq!(blob.subscribers.len(), 1); // still only one subscriber
        assert_eq!(blob.status, BlobStatus::Resolved);
        assert_eq!(blob.size, size);

        // Check the subscription group
        let group = blob.subscribers.get(&subscriber).unwrap();
        assert_eq!(group.subscriptions.len(), 2);
        let got_sub = group.subscriptions.get(&id2.clone()).unwrap();
        assert_eq!(*got_sub, sub);

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add3_epoch);
        assert_eq!(
            account.credit_committed, // stays the same becuase we're starting over
            BigInt::from(AUTO_TTL as u64 * size),
        );
        credit_amount -= BigInt::from((add3_epoch - add2_epoch) as u64 * size);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size)); // not changed

        // Debit all accounts
        let debit_epoch = ChainEpoch::from(41);
        let deletes_from_disc = state.debit_accounts(debit_epoch).unwrap();
        assert!(deletes_from_disc.is_empty());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, debit_epoch);
        assert_eq!(
            account.credit_committed, // debit reduces this
            BigInt::from((AUTO_TTL - (debit_epoch - add3_epoch)) as u64 * size),
        );
        assert_eq!(account.credit_free, credit_amount); // not changed
        assert_eq!(account.capacity_used, BigInt::from(size)); // not changed

        // Check indexes
        assert_eq!(state.expiries.len(), 2);
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);

        // Delete the default subscription ID
        let delete_epoch = ChainEpoch::from(51);
        let res = state.delete_blob(origin, caller, subscriber, delete_epoch, hash, id1.clone());
        assert!(res.is_ok());
        let delete_from_disk = res.unwrap();
        assert!(!delete_from_disk);

        // Check the blob
        let blob = state.get_blob(hash).unwrap();
        assert_eq!(blob.subscribers.len(), 1); // still one subscriber
        assert_eq!(blob.status, BlobStatus::Resolved);
        assert_eq!(blob.size, size);

        // Check the subscription group
        let group = blob.subscribers.get(&subscriber).unwrap();
        assert_eq!(group.subscriptions.len(), 1);
        let sub = group.subscriptions.get(&id2.clone()).unwrap();
        assert_eq!(sub.added, add3_epoch);
        assert_eq!(sub.expiry, add3_epoch + AUTO_TTL);

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, delete_epoch);
        assert_eq!(
            account.credit_committed, // debit reduces this
            BigInt::from((AUTO_TTL - (delete_epoch - add3_epoch)) as u64 * size),
        );
        assert_eq!(account.credit_free, credit_amount); // not changed
        assert_eq!(account.capacity_used, BigInt::from(size)); // not changed

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            token_amount.atto() - (&account.credit_free + &account.credit_committed)
        );
        assert_eq!(state.capacity_used, BigInt::from(size));

        // Check indexes
        assert_eq!(state.expiries.len(), 1);
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);

        // Check approval
        let account_committed = account.credit_committed.clone();
        check_approval(
            account,
            origin,
            caller,
            state.credit_debited + account_committed,
        );
    }

    #[test]
    fn test_renew_blob_success() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, amount.clone(), current_epoch)
            .unwrap();
        let mut credit_amount = amount.atto() * state.credit_debit_rate;

        // Add blob with default a subscription ID
        let (hash, size) = new_hash(1024);
        let add_epoch = current_epoch;
        let source = new_pk();
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            add_epoch,
            hash,
            new_metadata_hash(),
            SubscriptionId::Default,
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(AUTO_TTL as u64 * size),
        );
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size));

        // Set to status pending
        let res = state.set_blob_pending(subscriber, hash, SubscriptionId::Default, source);
        assert!(res.is_ok());

        // Finalize as resolved
        let finalize_epoch = ChainEpoch::from(11);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            SubscriptionId::Default,
            BlobStatus::Resolved,
        );
        assert!(res.is_ok());

        // Renew blob
        let renew_epoch = ChainEpoch::from(21);
        let res = state.renew_blob(subscriber, renew_epoch, hash, SubscriptionId::Default);
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from((AUTO_TTL + (renew_epoch - add_epoch)) as u64 * size),
        );
        credit_amount -= (renew_epoch - add_epoch) as u64 * size;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            amount.atto() - (&account.credit_free + &account.credit_committed)
        );
        assert_eq!(state.capacity_used, account.capacity_used);

        // Check indexes
        assert_eq!(state.expiries.len(), 1);
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);
    }

    #[test]
    fn test_renew_blob_refund() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, amount.clone(), current_epoch)
            .unwrap();
        let mut credit_amount = amount.atto() * state.credit_debit_rate;

        // Add blob with default a subscription ID
        let (hash1, size1) = new_hash(1024);
        let add1_epoch = current_epoch;
        let id1 = SubscriptionId::Default;
        let source = new_pk();
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            add1_epoch,
            hash1,
            new_metadata_hash(),
            id1.clone(),
            size1,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add1_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(AUTO_TTL as u64 * size1),
        );
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1));

        // Add another blob past the first blob's expiry
        let (hash2, size2) = new_hash(2048);
        let add2_epoch = ChainEpoch::from(AUTO_TTL + 11);
        let id2 = SubscriptionId::Key(b"foo".to_vec());
        let source = new_pk();
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            add2_epoch,
            hash2,
            new_metadata_hash(),
            id2.clone(),
            size2,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch);
        let blob1_expiry = ChainEpoch::from(AUTO_TTL + add1_epoch);
        let overcharge = BigInt::from((add2_epoch - blob1_expiry) as u64 * size1);
        assert_eq!(
            account.credit_committed, // this includes an overcharge that needs to be accounted for
            AUTO_TTL as u64 * size2 - overcharge,
        );
        credit_amount -= BigInt::from(AUTO_TTL as u64 * size2);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1 + size2));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            amount.atto() - (&account.credit_free + &account.credit_committed)
        );
        assert_eq!(state.capacity_used, account.capacity_used);

        // Check indexes
        assert_eq!(state.expiries.len(), 2);
        assert_eq!(state.added.len(), 2);
        assert_eq!(state.pending.len(), 0);

        // Renew the first blob
        let renew_epoch = ChainEpoch::from(AUTO_TTL + 31);
        let res = state.renew_blob(subscriber, renew_epoch, hash1, id1.clone());
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch);
        let blob1_expiry2 = ChainEpoch::from(AUTO_TTL + renew_epoch);
        let blob2_expiry = ChainEpoch::from(AUTO_TTL + add2_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(
                (blob2_expiry - add2_epoch) as u64 * size2
                    + (blob1_expiry2 - add2_epoch) as u64 * size1
            ),
        );
        credit_amount -= BigInt::from((blob1_expiry2 - add2_epoch) as u64 * size1);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1 + size2));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            amount.atto() - (&account.credit_free + &account.credit_committed)
        );
        assert_eq!(state.capacity_used, account.capacity_used);

        // Check indexes
        assert_eq!(state.expiries.len(), 2);
        assert_eq!(state.added.len(), 2);
        assert_eq!(state.pending.len(), 0);
    }

    #[test]
    fn test_finalize_blob_from_bad_state() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, amount.clone(), current_epoch)
            .unwrap();

        // Add a blob
        let (hash, size) = new_hash(1024);
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            current_epoch,
            hash,
            new_metadata_hash(),
            SubscriptionId::Default,
            size,
            None,
            new_pk(),
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Finalize as pending
        let finalize_epoch = ChainEpoch::from(11);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            SubscriptionId::Default,
            BlobStatus::Pending,
        );
        assert!(res.is_err());
        assert_eq!(
            res.err().unwrap().msg(),
            format!("cannot finalize blob {} as added or pending", hash)
        );
    }

    #[test]
    fn test_finalize_blob_resolved() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, amount.clone(), current_epoch)
            .unwrap();

        // Add a blob
        let (hash, size) = new_hash(1024);
        let source = new_pk();
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            current_epoch,
            hash,
            new_metadata_hash(),
            SubscriptionId::Default,
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Set to status pending
        let res = state.set_blob_pending(subscriber, hash, SubscriptionId::Default, source);
        assert!(res.is_ok());

        // Finalize as resolved
        let finalize_epoch = ChainEpoch::from(11);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            SubscriptionId::Default,
            BlobStatus::Resolved,
        );
        assert!(res.is_ok());

        // Check status
        let status = state
            .get_blob_status(subscriber, hash, SubscriptionId::Default)
            .unwrap();
        assert!(matches!(status, BlobStatus::Resolved));

        // Check indexes
        assert_eq!(state.expiries.len(), 1);
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);
    }

    #[test]
    fn test_finalize_blob_failed() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, amount.clone(), current_epoch)
            .unwrap();
        let credit_amount = amount.atto() * state.credit_debit_rate;

        // Add a blob
        let add_epoch = current_epoch;
        let (hash, size) = new_hash(1024);
        let source = new_pk();
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            add_epoch,
            hash,
            new_metadata_hash(),
            SubscriptionId::Default,
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Set to status pending
        let res = state.set_blob_pending(subscriber, hash, SubscriptionId::Default, source);
        assert!(res.is_ok());

        // Finalize as failed
        let finalize_epoch = ChainEpoch::from(11);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            SubscriptionId::Default,
            BlobStatus::Failed,
        );
        assert!(res.is_ok());

        // Check status
        let status = state
            .get_blob_status(subscriber, hash, SubscriptionId::Default)
            .unwrap();
        assert!(matches!(status, BlobStatus::Failed));

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add_epoch);
        assert_eq!(account.credit_committed, BigInt::from(0)); // credit was released
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(0)); // capacity was released

        // Check state
        assert_eq!(state.credit_committed, BigInt::from(0)); // credit was released
        assert_eq!(state.credit_debited, BigInt::from(0));
        assert_eq!(state.capacity_used, BigInt::from(0)); // capacity was released

        // Check indexes
        assert_eq!(state.expiries.len(), 1); // remains until the blob is explicitly deleted
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);
    }

    #[test]
    fn test_finalize_blob_failed_refund() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, amount.clone(), current_epoch)
            .unwrap();
        let mut credit_amount = amount.atto() * state.credit_debit_rate;

        // Add a blob
        let add_epoch = current_epoch;
        let (hash, size) = new_hash(1024);
        let source = new_pk();
        let res = state.add_blob(
            subscriber,
            subscriber,
            subscriber,
            add_epoch,
            hash,
            new_metadata_hash(),
            SubscriptionId::Default,
            size,
            None,
            source,
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(AUTO_TTL as u64 * size),
        );
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(state.credit_debited, BigInt::from(0));
        assert_eq!(state.capacity_used, account.capacity_used); // capacity was released

        // Debit accounts to trigger a refund when we fail below
        let debit_epoch = ChainEpoch::from(11);
        let deletes_from_disc = state.debit_accounts(debit_epoch).unwrap();
        assert!(deletes_from_disc.is_empty());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, debit_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from((AUTO_TTL - (debit_epoch - add_epoch)) as u64 * size),
        );
        assert_eq!(account.credit_free, credit_amount); // not changed
        assert_eq!(account.capacity_used, BigInt::from(size));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed);
        assert_eq!(
            state.credit_debited,
            BigInt::from((debit_epoch - add_epoch) as u64 * size)
        );
        assert_eq!(state.capacity_used, account.capacity_used);

        // Set to status pending
        let res = state.set_blob_pending(subscriber, hash, SubscriptionId::Default, source);
        assert!(res.is_ok());

        // Finalize as failed
        let finalize_epoch = ChainEpoch::from(21);
        let res = state.finalize_blob(
            subscriber,
            finalize_epoch,
            hash,
            SubscriptionId::Default,
            BlobStatus::Failed,
        );
        assert!(res.is_ok());

        // Check status
        let status = state
            .get_blob_status(subscriber, hash, SubscriptionId::Default)
            .unwrap();
        assert!(matches!(status, BlobStatus::Failed));

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, debit_epoch);
        assert_eq!(account.credit_committed, BigInt::from(0)); // credit was released
        assert_eq!(account.credit_free, amount.atto() * state.credit_debit_rate); // credit was refunded
        assert_eq!(account.capacity_used, BigInt::from(0)); // capacity was released

        // Check state
        assert_eq!(state.credit_committed, BigInt::from(0)); // credit was released
        assert_eq!(state.credit_debited, BigInt::from(0)); // credit was refunded and released
        assert_eq!(state.capacity_used, BigInt::from(0)); // capacity was released

        // Check indexes
        assert_eq!(state.expiries.len(), 1); // remains until the blob is explicitly deleted
        assert_eq!(state.added.len(), 0);
        assert_eq!(state.pending.len(), 0);
    }

    #[test]
    fn test_delete_blob_refund() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(origin, token_amount.clone(), current_epoch)
            .unwrap();
        delete_blob_refund(state, origin, origin, origin, current_epoch, token_amount);
    }

    #[test]
    fn test_delete_blob_refund_with_approval() {
        setup_logs();
        let capacity = 1024 * 1024;
        let mut state = State::new(capacity, 1);
        let origin = new_address();
        let subscriber = new_address();
        let current_epoch = ChainEpoch::from(1);
        let token_amount = TokenAmount::from_whole(10);
        state
            .buy_credit(subscriber, token_amount.clone(), current_epoch)
            .unwrap();
        state
            .approve_credit(subscriber, origin, None, current_epoch, None, None)
            .unwrap();
        delete_blob_refund(
            state,
            origin,
            origin,
            subscriber,
            current_epoch,
            token_amount,
        );
    }

    fn delete_blob_refund(
        mut state: State,
        origin: Address,
        caller: Address,
        subscriber: Address,
        current_epoch: ChainEpoch,
        token_amount: TokenAmount,
    ) {
        let mut credit_amount = token_amount.atto() * state.credit_debit_rate;

        // Add a blob
        let add1_epoch = current_epoch;
        let (hash1, size1) = new_hash(1024);
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add1_epoch,
            hash1,
            new_metadata_hash(),
            SubscriptionId::Default,
            size1,
            Some(MIN_TTL),
            new_pk(),
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add1_epoch);
        assert_eq!(
            account.credit_committed,
            BigInt::from(MIN_TTL as u64 * size1),
        );
        credit_amount -= &account.credit_committed;
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1));

        // Add another blob past the first blob expiry
        // This will trigger a debit on the account
        let add2_epoch = ChainEpoch::from(MIN_TTL + 10);
        let (hash2, size2) = new_hash(2048);
        let res = state.add_blob(
            origin,
            caller,
            subscriber,
            add2_epoch,
            hash2,
            new_metadata_hash(),
            SubscriptionId::Default,
            size2,
            Some(MIN_TTL),
            new_pk(),
            TokenAmount::zero(),
        );
        assert!(res.is_ok());

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch);
        let blob1_expiry = ChainEpoch::from(MIN_TTL + add1_epoch);
        let overcharge = BigInt::from((add2_epoch - blob1_expiry) as u64 * size1);
        assert_eq!(
            account.credit_committed, // this includes an overcharge that needs to be refunded
            MIN_TTL as u64 * size2 - overcharge,
        );
        credit_amount -= BigInt::from(MIN_TTL as u64 * size2);
        assert_eq!(account.credit_free, credit_amount);
        assert_eq!(account.capacity_used, BigInt::from(size1 + size2));

        // Delete the first blob
        let delete_epoch = ChainEpoch::from(MIN_TTL + 20);
        let delete_from_disc = state
            .delete_blob(
                origin,
                caller,
                subscriber,
                delete_epoch,
                hash1,
                SubscriptionId::Default,
            )
            .unwrap();
        assert!(delete_from_disc);

        // Check the account balance
        let account = state.get_account(subscriber).unwrap();
        assert_eq!(account.last_debit_epoch, add2_epoch); // not changed, blob is expired
        assert_eq!(
            account.credit_committed, // should not include overcharge due to refund
            BigInt::from(MIN_TTL as u64 * size2),
        );
        assert_eq!(account.credit_free, credit_amount); // not changed
        assert_eq!(account.capacity_used, BigInt::from(size2));

        // Check state
        assert_eq!(state.credit_committed, account.credit_committed); // credit was released
        assert_eq!(state.credit_debited, BigInt::from(MIN_TTL as u64 * size1));
        assert_eq!(state.capacity_used, BigInt::from(size2)); // capacity was released

        // Check indexes
        assert_eq!(state.expiries.len(), 1);
        assert_eq!(state.added.len(), 1);
        assert_eq!(state.pending.len(), 0);

        // Check approval
        let account_committed = account.credit_committed.clone();
        check_approval(
            account,
            origin,
            caller,
            state.credit_debited + account_committed,
        );
    }
}
