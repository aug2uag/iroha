//! This module contains `Block` structures for each state, it's
//! transitions, implementations and related traits
//! implementations. `Block`s are organised into a linear sequence
//! over time (also known as the block chain).  A Block's life-cycle
//! starts from `PendingBlock`.
#![allow(
    clippy::module_name_repetitions,
    clippy::std_instead_of_core,
    clippy::std_instead_of_alloc,
    clippy::arithmetic
)]

use std::{error::Error, iter, marker::PhantomData};

use dashmap::{mapref::one::Ref as MapRef, DashMap};
use eyre::{bail, eyre, Context, Result};
use iroha_config::sumeragi::{DEFAULT_BLOCK_TIME_MS, DEFAULT_COMMIT_TIME_LIMIT_MS};
use iroha_crypto::{HashOf, KeyPair, MerkleTree, SignatureOf, SignaturesOf};
use iroha_data_model::{
    block_value::{BlockHeaderValue, BlockValue},
    current_time,
    events::prelude::*,
    transaction::prelude::*,
};
use iroha_schema::IntoSchema;
use iroha_version::{declare_versioned_with_scale, version_with_scale};
use parity_scale_codec::{Decode, Encode};
use serde::Serialize;

use crate::{
    prelude::*,
    sumeragi::network_topology::Topology,
    tx::{TransactionValidator, VersionedAcceptedTransaction},
};

/// Default estimation of consensus duration
#[allow(clippy::integer_division)]
pub const DEFAULT_CONSENSUS_ESTIMATION_MS: u64 =
    DEFAULT_BLOCK_TIME_MS + (DEFAULT_COMMIT_TIME_LIMIT_MS / 2);

/// The chain of the previous block hash. If there is no previous
/// block, the blockchain is empty.
#[derive(Debug, Clone, Copy)]
pub struct EmptyChainHash<T>(PhantomData<T>);

impl<T> Default for EmptyChainHash<T> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<T> From<EmptyChainHash<T>> for HashOf<T> {
    fn from(EmptyChainHash(PhantomData): EmptyChainHash<T>) -> Self {
        Hash::zeroed().typed()
    }
}

/// Blockchain.
#[derive(Debug, Default)]
pub struct Chain {
    blocks: DashMap<u64, VersionedCommittedBlock>,
}

impl Chain {
    /// Constructor.
    #[inline]
    pub fn new() -> Self {
        Chain {
            blocks: DashMap::new(),
        }
    }

    /// Push latest block.
    pub fn push(&self, block: VersionedCommittedBlock) {
        let height = block.as_v1().header.height;
        self.blocks.insert(height, block);
    }

    /// Iterator over height and block.
    pub fn iter(&self) -> ChainIterator {
        ChainIterator::new(self)
    }

    /// Latest block reference and its height.
    pub fn latest_block(&self) -> Option<MapRef<u64, VersionedCommittedBlock>> {
        self.blocks.get(&(self.blocks.len() as u64))
    }

    /// Length of the blockchain.
    #[inline]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether blockchain is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Chain iterator
pub struct ChainIterator<'itm> {
    chain: &'itm Chain,
    pos_front: u64,
    pos_back: u64,
}

impl<'itm> ChainIterator<'itm> {
    fn new(chain: &'itm Chain) -> Self {
        ChainIterator {
            chain,
            pos_front: 1,
            pos_back: chain.len() as u64,
        }
    }

    const fn is_exhausted(&self) -> bool {
        self.pos_front > self.pos_back
    }
}

impl<'itm> Iterator for ChainIterator<'itm> {
    type Item = MapRef<'itm, u64, VersionedCommittedBlock>;
    fn next(&mut self) -> Option<Self::Item> {
        if !self.is_exhausted() {
            let val = self.chain.blocks.get(&self.pos_front);
            self.pos_front += 1;
            return val;
        }
        None
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.pos_front += n as u64;
        self.next()
    }

    fn last(mut self) -> Option<Self::Item> {
        self.pos_front = self.chain.len() as u64;
        self.chain.blocks.get(&self.pos_front)
    }

    fn count(self) -> usize {
        #[allow(clippy::cast_possible_truncation)]
        let count = (self.chain.len() as u64 - (self.pos_front - 1)) as usize;
        count
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        #[allow(clippy::cast_possible_truncation)]
        let height = (self.chain.len() as u64 - (self.pos_front - 1)) as usize;
        (height, Some(height))
    }
}

impl<'itm> DoubleEndedIterator for ChainIterator<'itm> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if !self.is_exhausted() {
            let val = self.chain.blocks.get(&self.pos_back);
            self.pos_back -= 1;
            return val;
        }
        None
    }

    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.pos_back -= n as u64;
        self.next_back()
    }
}

declare_versioned_with_scale!(VersionedPendingBlock 1..2, Debug, Clone, iroha_macro::FromVariant);

/// Transaction data is permanently recorded in files called
/// blocks.  This is the first stage of a `Block`s life-cycle.
#[version_with_scale(n = 1, versioned = "VersionedPendingBlock")]
#[derive(Debug, Clone, Decode, Encode)]
pub struct PendingBlock {
    /// Unix time (in milliseconds) of block forming by a peer.
    pub timestamp: u128,
    /// array of transactions, which successfully passed validation and consensus step.
    pub transactions: Vec<VersionedAcceptedTransaction>,
    /// Event recommendations.
    pub event_recommendations: Vec<Event>,
}

// TODO: I strongly believe that we shouldn't be moving parts of a
// PendingBlock, but instead move the PendingBlock wholesale. This
// refactor could improve memory performance.

impl PendingBlock {
    /// Create a new `PendingBlock` from transactions.
    #[inline]
    pub fn new(
        transactions: Vec<VersionedAcceptedTransaction>,
        event_recommendations: Vec<Event>,
    ) -> PendingBlock {
        #[allow(clippy::expect_used)]
        let timestamp = current_time().as_millis();
        // TODO: Need to check if the `transactions` vector is empty. It shouldn't be allowed.
        PendingBlock {
            timestamp,
            transactions,
            event_recommendations,
        }
    }

    /// Chain block with the existing blockchain.
    pub fn chain(
        self,
        height: u64,
        previous_block_hash: HashOf<VersionedCommittedBlock>,
    ) -> ChainedBlock {
        ChainedBlock {
            transactions: self.transactions,
            event_recommendations: self.event_recommendations,
            header: BlockHeader {
                timestamp: self.timestamp,
                consensus_estimation: DEFAULT_CONSENSUS_ESTIMATION_MS,
                height: height + 1,
                previous_block_hash,
                transactions_hash: Hash::zeroed().typed(),
                rejected_transactions_hash: Hash::zeroed().typed(),
                genesis_topology: None,
            },
        }
    }

    /// Create a new blockchain with current block as a first block.
    pub fn chain_first_with_genesis_topology(self, genesis_topology: Topology) -> ChainedBlock {
        ChainedBlock {
            transactions: self.transactions,
            event_recommendations: self.event_recommendations,
            header: BlockHeader {
                timestamp: self.timestamp,
                consensus_estimation: DEFAULT_CONSENSUS_ESTIMATION_MS,
                height: 1,
                previous_block_hash: EmptyChainHash::default().into(),
                transactions_hash: Hash::zeroed().typed(),
                rejected_transactions_hash: Hash::zeroed().typed(),
                genesis_topology: Some(genesis_topology),
            },
        }
    }

    /// Create a new blockchain with current block as a first block.
    pub fn chain_first(self) -> ChainedBlock {
        ChainedBlock {
            transactions: self.transactions,
            event_recommendations: self.event_recommendations,
            header: BlockHeader {
                timestamp: self.timestamp,
                consensus_estimation: DEFAULT_CONSENSUS_ESTIMATION_MS,
                height: 1,
                previous_block_hash: EmptyChainHash::default().into(),
                transactions_hash: Hash::zeroed().typed(),
                rejected_transactions_hash: Hash::zeroed().typed(),
                genesis_topology: None,
            },
        }
    }
}

/// When `PendingBlock` chained with a blockchain it becomes `ChainedBlock`
#[derive(Debug, Clone, Decode, Encode)]
pub struct ChainedBlock {
    /// Block header
    pub header: BlockHeader,
    /// Array of transactions, which successfully passed validation and consensus step.
    pub transactions: Vec<VersionedAcceptedTransaction>,
    /// Event recommendations.
    pub event_recommendations: Vec<Event>,
}

/// Header of the block. The hash should be taken from its byte representation.
#[derive(Debug, Clone, Decode, Encode, IntoSchema, Serialize)]
pub struct BlockHeader {
    /// Unix time (in milliseconds) of block forming by a peer.
    pub timestamp: u128,
    /// Estimation of consensus duration in milliseconds
    pub consensus_estimation: u64,
    /// a number of blocks in the chain up to the block.
    pub height: u64,
    /// Hash of a previous block in the chain.
    /// Is an array of zeros for the first block.
    pub previous_block_hash: HashOf<VersionedCommittedBlock>,
    /// Hash of merkle tree root of the tree of valid transactions' hashes.
    pub transactions_hash: HashOf<MerkleTree<VersionedSignedTransaction>>,
    /// Hash of merkle tree root of the tree of rejected transactions' hashes.
    pub rejected_transactions_hash: HashOf<MerkleTree<VersionedSignedTransaction>>,
    /// Genesis topology
    pub genesis_topology: Option<Topology>,
}

impl BlockHeader {
    /// Checks if it's a header of a genesis block.
    #[inline]
    pub const fn is_genesis(&self) -> bool {
        self.height == 1
    }
}

impl ChainedBlock {
    /// Validate block transactions against the current state of the world.
    pub fn validate(
        self,
        transaction_validator: &TransactionValidator,
        wsv: &WorldStateView,
    ) -> ValidBlock {
        let mut txs = Vec::new();
        let mut rejected = Vec::new();

        for tx in self.transactions {
            match transaction_validator.validate(tx.into_v1(), self.header.is_genesis(), wsv) {
                Ok(tx) => txs.push(tx),
                Err(tx) => {
                    iroha_logger::warn!(
                        reason = %tx.as_v1().rejection_reason,
                        caused_by = ?tx.as_v1().rejection_reason.source(),
                        "Transaction validation failed",
                    );
                    rejected.push(tx)
                }
            }
        }
        let mut header = self.header;
        header.transactions_hash = txs
            .iter()
            .map(VersionedValidTransaction::hash)
            .collect::<MerkleTree<_>>()
            .hash()
            .unwrap_or(Hash::zeroed().typed());
        header.rejected_transactions_hash = rejected
            .iter()
            .map(VersionedRejectedTransaction::hash)
            .collect::<MerkleTree<_>>()
            .hash()
            .unwrap_or(Hash::zeroed().typed());
        let event_recommendations = self.event_recommendations;
        // TODO: Validate Event recommendations somehow?
        ValidBlock {
            header,
            rejected_transactions: rejected,
            transactions: txs,
            event_recommendations,
        }
    }

    /// Calculate the hash of the current block.
    pub fn hash(&self) -> HashOf<Self> {
        HashOf::new(&self.header).transmute()
    }
}
/// After full validation `ChainedBlock` can transform into `ValidBlock`.
#[derive(Debug, Clone)]
pub struct ValidBlock {
    /// Block header
    pub header: BlockHeader,
    /// Array of rejected transactions.
    pub rejected_transactions: Vec<VersionedRejectedTransaction>,
    /// Array of all transactions in this block.
    pub transactions: Vec<VersionedValidTransaction>,
    /// Event recommendations.
    pub event_recommendations: Vec<Event>,
}

impl ValidBlock {
    /// Calculate hash of the current block.
    #[inline]
    pub fn hash(&self) -> HashOf<Self> {
        HashOf::new(&self.header).transmute()
    }

    /// Sign this block and get `ValidSignedBlock`.
    ///
    /// # Errors
    /// Fails if signature generation fails
    pub fn sign(self, key_pair: KeyPair) -> Result<ValidSignedBlock> {
        let signature = SignatureOf::from_hash(key_pair, &self.hash().transmute())
            .wrap_err(format!("Failed to sign block with hash {}", self.hash()))?;
        let signatures = SignaturesOf::from(signature);
        Ok(ValidSignedBlock {
            header: self.header,
            rejected_transactions: self.rejected_transactions,
            transactions: self.transactions,
            event_recommendations: self.event_recommendations,
            signatures,
        })
    }
}

/// After receiving first signature, `ValidBlock` can transform into `ValidSignedBlock`.
#[derive(Debug, Clone)]
pub struct ValidSignedBlock {
    /// Block header
    pub header: BlockHeader,
    /// Array of all rejected transactions in this block.
    pub rejected_transactions: Vec<VersionedRejectedTransaction>,
    /// Array of all valid transactions in this block.
    pub transactions: Vec<VersionedValidTransaction>,
    /// Signatures of peers which approved this block.
    pub signatures: SignaturesOf<Self>,
    /// Event recommendations.
    pub event_recommendations: Vec<Event>,
}

impl ValidSignedBlock {
    /// Commit block to the store.
    #[inline]
    pub fn commit(self) -> CommittedBlock {
        let Self {
            header,
            rejected_transactions,
            transactions,
            event_recommendations,
            signatures,
        } = self;

        CommittedBlock {
            event_recommendations,
            header,
            rejected_transactions,
            transactions,
            signatures: signatures.transmute(),
        }
    }

    /// Calculate the hash of the current block.
    pub fn hash(&self) -> HashOf<Self> {
        HashOf::new(&self.header).transmute()
    }

    /// Add additional signatures for `ValidSignedBlock`.
    ///
    /// # Errors
    /// Fails if signature generation fails
    pub fn sign(mut self, key_pair: KeyPair) -> Result<Self> {
        SignatureOf::from_hash(key_pair, &self.hash())
            .wrap_err(format!("Failed to sign block with hash {}", self.hash()))
            .map(|signature| {
                self.signatures.insert(signature);
                self
            })
    }

    /// Return the signatures (as `payload`) that are verified with the `hash` of this block.
    #[inline]
    pub fn verified_signatures(&self) -> impl Iterator<Item = &SignatureOf<Self>> {
        self.signatures.verified_by_hash(self.hash())
    }

    /// Create dummy `ValidBlock`. Used in tests
    ///
    /// # Panics
    /// If generating keys or block signing fails.
    #[allow(clippy::restriction)]
    #[cfg(test)]
    pub fn new_dummy() -> Self {
        ValidBlock {
            header: BlockHeader {
                timestamp: 0,
                consensus_estimation: DEFAULT_CONSENSUS_ESTIMATION_MS,
                height: 1,
                previous_block_hash: EmptyChainHash::default().into(),
                transactions_hash: EmptyChainHash::default().into(),
                rejected_transactions_hash: EmptyChainHash::default().into(),
                genesis_topology: None,
            },
            rejected_transactions: Vec::new(),
            transactions: Vec::new(),
            event_recommendations: Vec::new(),
        }
        .sign(KeyPair::generate().unwrap())
        .unwrap()
    }
}

impl From<&ValidSignedBlock> for Vec<Event> {
    fn from(block: &ValidSignedBlock) -> Self {
        block
            .transactions
            .iter()
            .map(|transaction| -> Event {
                PipelineEvent::new(
                    PipelineEntityKind::Transaction,
                    PipelineStatus::Validating,
                    transaction.hash().into(),
                )
                .into()
            })
            .chain(block.rejected_transactions.iter().map(|transaction| {
                PipelineEvent::new(
                    PipelineEntityKind::Transaction,
                    PipelineStatus::Validating,
                    transaction.hash().into(),
                )
                .into()
            }))
            .chain([PipelineEvent::new(
                PipelineEntityKind::Block,
                PipelineStatus::Validating,
                block.hash().into(),
            )
            .into()])
            .collect()
    }
}

declare_versioned_with_scale!(VersionedCandidateBlock 1..2, Debug, Clone, iroha_macro::FromVariant, IntoSchema);

impl VersionedCandidateBlock {
    /// Convert from `&VersionedCandidateBlock` to V1 reference
    #[inline]
    pub const fn as_v1(&self) -> &CandidateBlock {
        match self {
            Self::V1(v1) => v1,
        }
    }

    /// Convert from `&mut VersionedCandidateBlock` to V1 mutable reference
    #[inline]
    pub fn as_mut_v1(&mut self) -> &mut CandidateBlock {
        match self {
            Self::V1(v1) => v1,
        }
    }

    /// Perform the conversion from `VersionedCandidateBlock` to V1
    #[inline]
    pub fn into_v1(self) -> CandidateBlock {
        match self {
            Self::V1(v1) => v1,
        }
    }

    /// Return the header of a valid block
    #[inline]
    pub const fn header(&self) -> &BlockHeader {
        &self.as_v1().header
    }

    /// Calculate the hash of the current block.
    #[inline]
    pub fn hash(&self) -> HashOf<Self> {
        self.as_v1().hash().transmute()
    }

    /// Return the signatures (as `payload`) that are verified with the `hash` of this block.
    #[inline]
    pub fn verified_signatures(&self) -> impl Iterator<Item = &SignatureOf<Self>> {
        self.as_v1()
            .verified_signatures()
            .map(SignatureOf::transmute_ref)
    }

    /// Revalidate a block against the current state of the world.
    ///
    /// # Errors
    /// Forward errors from [`CandidateBlock::revalidate`]
    #[inline]
    pub fn revalidate(
        self,
        transaction_validator: &TransactionValidator,
        wsv: &WorldStateView,
        latest_block: &HashOf<VersionedCommittedBlock>,
        block_height: u64,
    ) -> Result<ValidSignedBlock, eyre::Report> {
        self.into_v1()
            .revalidate(transaction_validator, wsv, latest_block, block_height)
    }
}

/// Revalidate the block that was sent through the network by transforming it back into [`ValidBlock`]
#[version_with_scale(n = 1, versioned = "VersionedCandidateBlock")]
#[derive(Debug, Clone, Decode, Encode, IntoSchema)]
pub struct CandidateBlock {
    /// Block header
    pub header: BlockHeader,
    /// Array of rejected transactions.
    pub rejected_transactions: Vec<VersionedSignedTransaction>,
    /// Array of all transactions in this block.
    pub transactions: Vec<VersionedSignedTransaction>,
    /// Signatures of peers which approved this block.
    pub signatures: SignaturesOf<Self>,
    /// Event recommendations.
    pub event_recommendations: Vec<Event>,
}

impl CandidateBlock {
    /// Calculate the hash of the current block.
    #[inline]
    pub fn hash(&self) -> HashOf<Self> {
        HashOf::new(&self.header).transmute()
    }

    /// Return the signatures (as `payload`) that are verified with the `hash` of this block.
    #[inline]
    pub fn verified_signatures(&self) -> impl Iterator<Item = &SignatureOf<Self>> {
        self.signatures.verified_by_hash(self.hash())
    }

    /// Check if there are no transactions in this block.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.transactions.is_empty() && self.rejected_transactions.is_empty()
    }

    /// Check if a block has transactions that are already in the blockchain.
    pub fn has_committed_transactions(&self, wsv: &WorldStateView) -> bool {
        self.transactions
            .iter()
            .any(|transaction| transaction.is_in_blockchain(wsv))
            || self
                .rejected_transactions
                .iter()
                .any(|transaction| transaction.is_in_blockchain(wsv))
    }

    /// Revalidate a block against the current state of the world.
    ///
    /// # Errors
    /// - Block is empty
    /// - Block has committed transactions
    /// - There is a mismatch between candidate block height and actual blockchain height
    /// - There is a mismatch between candidate block previous block hash and actual latest block hash
    /// - Block header transaction hashes don't match with computed transaction hashes
    /// - Error during revalidation of individual transactions
    pub fn revalidate(
        self,
        transaction_validator: &TransactionValidator,
        wsv: &WorldStateView,
        latest_block: &HashOf<VersionedCommittedBlock>,
        block_height: u64,
    ) -> Result<ValidSignedBlock, eyre::Report> {
        if self.is_empty() {
            bail!("Block is empty");
        }

        if self.has_committed_transactions(wsv) {
            bail!("Block has committed transactions");
        }

        if latest_block != &self.header.previous_block_hash {
            bail!(
                "Latest block hash mismatch. Expected: {}, actual: {}",
                latest_block,
                &self.header.previous_block_hash
            );
        }

        if block_height + 1 != self.header.height {
            bail!(
                "Block heights are in an inconsistent state. Expected: {}, actual: {}",
                block_height + 1,
                self.header.height
            );
        }

        let CandidateBlock {
            header,
            rejected_transactions,
            transactions,
            signatures,
            event_recommendations,
        } = self;

        // Validate that header transactions hashes are matched with actual hashes
        transactions
            .iter()
            .map(VersionedSignedTransaction::hash)
            .collect::<MerkleTree<_>>()
            .hash()
            .unwrap_or(Hash::zeroed().typed())
            .eq(&header.transactions_hash)
            .then_some(())
            .ok_or_else(|| {
                eyre!("Block header transactions hash does not match actual transactions hash")
            })?;

        rejected_transactions
            .iter()
            .map(VersionedSignedTransaction::hash)
            .collect::<MerkleTree<_>>()
            .hash()
            .unwrap_or(Hash::zeroed().typed())
            .eq(&header.rejected_transactions_hash)
            .then_some(())
            .ok_or_else(|| eyre!("Block header rejected transactions hash does not match actual rejected transaction hash"))?;

        // Check that valid transactions are still valid
        let transactions = transactions
            .into_iter()
            .map(VersionedSignedTransaction::into_v1)
            .map(|tx| {
                AcceptedTransaction::from_transaction(tx, &transaction_validator.transaction_limits)
            })
            .map(|accepted_tx| {
                accepted_tx.and_then(|tx| {
                    transaction_validator
                        .validate(tx, header.is_genesis(), wsv)
                        .map_err(|rejected_tx| rejected_tx.into_v1().rejection_reason)
                        .wrap_err("Failed to validate transaction")
                })
            })
            .try_fold(Vec::new(), |mut acc, tx| {
                tx.map(|valid_tx| {
                    acc.push(valid_tx);
                    acc
                })
            })
            .wrap_err("Error during transaction revalidation")?;

        // Check that rejected transactions are indeed rejected
        let rejected_transactions = rejected_transactions
            .into_iter()
            .map(VersionedSignedTransaction::into_v1)
            .map(|tx| {
                AcceptedTransaction::from_transaction(tx, &transaction_validator.transaction_limits)
            })
            .map(|accepted_tx| {
                accepted_tx.and_then(|tx| {
                    match transaction_validator.validate(tx, header.is_genesis(), wsv) {
                        Err(rejected_transaction) => Ok(rejected_transaction),
                        Ok(_) => Err(eyre!("Transactions which supposed to be rejected is valid")),
                    }
                })
            })
            .try_fold(Vec::new(), |mut acc, rejected_tx| {
                rejected_tx.map(|tx| {
                    acc.push(tx);
                    acc
                })
            })
            .wrap_err("Error during transaction revalidation")?;

        Ok(ValidSignedBlock {
            header,
            transactions,
            rejected_transactions,
            event_recommendations,
            signatures: signatures.transmute(),
        })
    }
}

impl From<ValidSignedBlock> for CandidateBlock {
    fn from(valid_block: ValidSignedBlock) -> Self {
        let ValidSignedBlock {
            header,
            rejected_transactions,
            transactions,
            signatures,
            event_recommendations,
        } = valid_block;
        Self {
            header,
            rejected_transactions: rejected_transactions
                .into_iter()
                .map(VersionedSignedTransaction::from)
                .collect(),
            transactions: transactions
                .into_iter()
                .map(VersionedSignedTransaction::from)
                .collect(),
            signatures: signatures.transmute(),
            event_recommendations,
        }
    }
}

impl From<ValidSignedBlock> for VersionedCandidateBlock {
    fn from(valid_block: ValidSignedBlock) -> Self {
        CandidateBlock::from(valid_block).into()
    }
}
declare_versioned_with_scale!(VersionedCommittedBlock 1..2, Debug, Clone, iroha_macro::FromVariant, IntoSchema, Serialize);

impl VersionedCommittedBlock {
    /// Converts from `&VersionedCommittedBlock` to V1 reference
    #[inline]
    pub const fn as_v1(&self) -> &CommittedBlock {
        match self {
            Self::V1(v1) => v1,
        }
    }

    /// Converts from `&mut VersionedCommittedBlock` to V1 mutable reference
    #[inline]
    pub fn as_mut_v1(&mut self) -> &mut CommittedBlock {
        match self {
            Self::V1(v1) => v1,
        }
    }

    /// Performs the conversion from `VersionedCommittedBlock` to V1
    #[inline]
    pub fn into_v1(self) -> CommittedBlock {
        match self {
            Self::V1(v1) => v1,
        }
    }

    /// Calculate the hash of the current block.
    /// `VersionedCommitedBlock` should have the same hash as `VersionedCommitedBlock`.
    #[inline]
    pub fn hash(&self) -> HashOf<Self> {
        self.as_v1().hash().transmute()
    }

    /// Returns the header of a valid block
    #[inline]
    pub const fn header(&self) -> &BlockHeader {
        &self.as_v1().header
    }

    /// Return the signatures (as `payload`) that are verified with the `hash` of this block.
    #[inline]
    pub fn verified_signatures(&self) -> impl Iterator<Item = &SignatureOf<Self>> {
        self.as_v1()
            .verified_signatures()
            .map(SignatureOf::transmute_ref)
    }

    /// Converts block to [`iroha_data_model`] representation for use in e.g. queries.
    pub fn into_value(self) -> BlockValue {
        let current_block_hash = self.hash();

        let CommittedBlock {
            header,
            rejected_transactions,
            transactions,
            event_recommendations,
            ..
        } = self.into_v1();
        let BlockHeader {
            timestamp,
            height,
            previous_block_hash,
            transactions_hash,
            rejected_transactions_hash,
            ..
        } = header;

        let header_value = BlockHeaderValue {
            timestamp,
            height,
            previous_block_hash: *previous_block_hash,
            transactions_hash,
            rejected_transactions_hash,
            invalidated_blocks_hashes: Vec::new(),
            current_block_hash: Hash::from(current_block_hash),
        };

        BlockValue {
            header: header_value,
            transactions,
            rejected_transactions,
            event_recommendations,
        }
    }
}

/// When Kura receives `ValidSignedBlock`, the block is stored and
/// then sent to later stage of the pipeline as `CommittedBlock`.
#[version_with_scale(n = 1, versioned = "VersionedCommittedBlock")]
#[derive(Debug, Clone, Decode, Encode, IntoSchema, Serialize)]
pub struct CommittedBlock {
    /// Block header
    pub header: BlockHeader,
    /// Array of rejected transactions.
    pub rejected_transactions: Vec<VersionedRejectedTransaction>,
    /// array of transactions, which successfully passed validation and consensus step.
    pub transactions: Vec<VersionedValidTransaction>,
    /// Event recommendations.
    pub event_recommendations: Vec<Event>,
    /// Signatures of peers which approved this block
    pub signatures: SignaturesOf<Self>,
}

impl CommittedBlock {
    /// Calculate the hash of the current block.
    /// `CommitedBlock` should have the same hash as `ValidBlock`.
    #[inline]
    pub fn hash(&self) -> HashOf<Self> {
        HashOf::new(&self.header).transmute()
    }

    /// Return the signatures (as `payload`) that are verified with the `hash` of this block.
    #[inline]
    pub fn verified_signatures(&self) -> impl Iterator<Item = &SignatureOf<Self>> {
        self.signatures.verified_by_hash(self.hash())
    }
}

impl From<CommittedBlock> for ValidSignedBlock {
    #[inline]
    fn from(
        CommittedBlock {
            header,
            rejected_transactions,
            transactions,
            signatures,
            event_recommendations,
        }: CommittedBlock,
    ) -> Self {
        Self {
            header,
            rejected_transactions,
            transactions,
            event_recommendations,
            signatures: signatures.transmute(),
        }
    }
}

impl From<CommittedBlock> for CandidateBlock {
    fn from(
        CommittedBlock {
            header,
            rejected_transactions,
            transactions,
            signatures,
            event_recommendations,
        }: CommittedBlock,
    ) -> Self {
        Self {
            header,
            rejected_transactions: rejected_transactions
                .into_iter()
                .map(VersionedSignedTransaction::from)
                .collect(),
            transactions: transactions
                .into_iter()
                .map(VersionedSignedTransaction::from)
                .collect(),
            event_recommendations,
            signatures: signatures.transmute(),
        }
    }
}

impl From<VersionedCommittedBlock> for VersionedCandidateBlock {
    #[inline]
    fn from(block: VersionedCommittedBlock) -> Self {
        CandidateBlock::from(block.into_v1()).into()
    }
}

impl From<&VersionedCommittedBlock> for Vec<Event> {
    #[inline]
    fn from(block: &VersionedCommittedBlock) -> Self {
        block.as_v1().into()
    }
}

impl From<&CommittedBlock> for Vec<Event> {
    fn from(block: &CommittedBlock) -> Self {
        let rejected_tx = block
            .rejected_transactions
            .iter()
            .cloned()
            .map(|transaction| {
                PipelineEvent::new(
                    PipelineEntityKind::Transaction,
                    PipelineStatus::Rejected(transaction.as_v1().rejection_reason.clone().into()),
                    transaction.hash().into(),
                )
                .into()
            });
        let tx = block.transactions.iter().cloned().map(|transaction| {
            PipelineEvent::new(
                PipelineEntityKind::Transaction,
                PipelineStatus::Committed,
                transaction.hash().into(),
            )
            .into()
        });
        let current_block: iter::Once<Event> = iter::once(
            PipelineEvent::new(
                PipelineEntityKind::Block,
                PipelineStatus::Committed,
                block.hash().into(),
            )
            .into(),
        );

        tx.chain(rejected_tx).chain(current_block).collect()
    }
}

// TODO: Move to data_model after release
pub mod stream {
    //! Blocks for streaming API.

    use iroha_macro::FromVariant;
    use iroha_schema::prelude::*;
    use iroha_version::prelude::*;
    use parity_scale_codec::{Decode, Encode};

    use crate::block::VersionedCommittedBlock;

    declare_versioned_with_scale!(VersionedBlockMessage 1..2, Debug, Clone, FromVariant, IntoSchema);

    impl VersionedBlockMessage {
        /// Converts from `&VersionedBlockPublisherMessage` to V1 reference
        pub const fn as_v1(&self) -> &BlockMessage {
            match self {
                Self::V1(v1) => v1,
            }
        }

        /// Converts from `&mut VersionedBlockPublisherMessage` to V1 mutable reference
        pub fn as_mut_v1(&mut self) -> &mut BlockMessage {
            match self {
                Self::V1(v1) => v1,
            }
        }

        /// Performs the conversion from `VersionedBlockPublisherMessage` to V1
        pub fn into_v1(self) -> BlockMessage {
            match self {
                Self::V1(v1) => v1,
            }
        }
    }

    /// Message sent by the stream producer
    /// Block sent by the peer.
    #[version_with_scale(n = 1, versioned = "VersionedBlockMessage")]
    #[derive(Debug, Clone, Decode, Encode, IntoSchema)]
    pub struct BlockMessage(pub VersionedCommittedBlock);

    declare_versioned_with_scale!(VersionedBlockSubscriptionRequest 1..2, Debug, Clone, FromVariant, IntoSchema);

    impl VersionedBlockSubscriptionRequest {
        /// Converts from `&VersionedBlockSubscriberMessage` to V1 reference
        pub const fn as_v1(&self) -> &BlockSubscriptionRequest {
            match self {
                Self::V1(v1) => v1,
            }
        }

        /// Converts from `&mut VersionedBlockSubscriberMessage` to V1 mutable reference
        pub fn as_mut_v1(&mut self) -> &mut BlockSubscriptionRequest {
            match self {
                Self::V1(v1) => v1,
            }
        }

        /// Performs the conversion from `VersionedBlockSubscriberMessage` to V1
        pub fn into_v1(self) -> BlockSubscriptionRequest {
            match self {
                Self::V1(v1) => v1,
            }
        }
    }

    /// Message sent by the stream consumer.
    /// Request sent to subscribe to blocks stream starting from the given height.
    #[version_with_scale(n = 1, versioned = "VersionedBlockSubscriptionRequest")]
    #[derive(Debug, Clone, Copy, Decode, Encode, IntoSchema)]
    pub struct BlockSubscriptionRequest(pub u64);

    /// Exports common structs and enums from this module.
    pub mod prelude {
        pub use super::{
            BlockMessage, BlockSubscriptionRequest, VersionedBlockMessage,
            VersionedBlockSubscriptionRequest,
        };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::restriction)]

    use super::*;

    #[test]
    pub fn committed_and_valid_block_hashes_are_equal() {
        let valid_block = ValidSignedBlock::new_dummy();
        let committed_block = valid_block.clone().commit();

        assert_eq!(*valid_block.hash(), *committed_block.hash())
    }

    #[test]
    pub fn chain_iter_returns_blocks_ordered() {
        const BLOCK_COUNT: usize = 10;
        let chain = Chain::new();

        let mut block = ValidSignedBlock::new_dummy().commit();

        for i in 1..=BLOCK_COUNT {
            block.header.height = i as u64;
            chain.push(block.clone().into());
        }

        assert_eq!(
            (BLOCK_COUNT - 5..=BLOCK_COUNT)
                .map(|i| i as u64)
                .collect::<Vec<_>>(),
            chain
                .iter()
                .skip(BLOCK_COUNT - 6)
                .map(|b| *b.key())
                .collect::<Vec<_>>()
        );

        assert_eq!(BLOCK_COUNT - 2, chain.iter().skip(2).count());
        assert_eq!(3, *chain.iter().nth(2).unwrap().key());
    }

    #[test]
    pub fn chain_rev_iter_returns_blocks_ordered() {
        const BLOCK_COUNT: usize = 10;
        let chain = Chain::new();

        let mut block = ValidSignedBlock::new_dummy().commit();

        for i in 1..=BLOCK_COUNT {
            block.header.height = i as u64;
            chain.push(block.clone().into());
        }

        assert_eq!(
            (1..=BLOCK_COUNT - 4)
                .rev()
                .map(|i| i as u64)
                .collect::<Vec<_>>(),
            chain
                .iter()
                .rev()
                .skip(BLOCK_COUNT - 6)
                .map(|b| *b.key())
                .collect::<Vec<_>>()
        );

        assert_eq!(
            (BLOCK_COUNT - 2) as u64,
            *chain.iter().nth_back(2).unwrap().key()
        );
    }
}
