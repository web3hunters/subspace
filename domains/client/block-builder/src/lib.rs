// This file is part of Substrate.

// Copyright (C) 2017-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Substrate block builder
//!
//! This crate provides the [`BlockBuilder`] utility and the corresponding runtime api
//! [`BlockBuilder`](sp_block_builder::BlockBuilder).
//!
//! The block builder utility is used in the node as an abstraction over the runtime api to
//! initialize a block, to push extrinsics and to finalize a block.

#![warn(missing_docs)]

use codec::Encode;
use sc_client_api::backend;
use sp_api::{
    ApiExt, ApiRef, Core, ProvideRuntimeApi, StorageChanges, StorageProof, TransactionOutcome,
};
pub use sp_block_builder::BlockBuilder as BlockBuilderApi;
use sp_blockchain::{ApplyExtrinsicFailed, Error};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{Block as BlockT, Hash, HashingFor, Header as HeaderT, NumberFor, One};
use sp_runtime::Digest;
use std::collections::VecDeque;

/// Used as parameter to [`BlockBuilderProvider`] to express if proof recording should be enabled.
///
/// When `RecordProof::Yes` is given, all accessed trie nodes should be saved. These recorded
/// trie nodes can be used by a third party to proof this proposal without having access to the
/// full storage.
#[derive(Copy, Clone, Eq, PartialEq)]
pub enum RecordProof {
    /// `Yes`, record a proof.
    Yes,
    /// `No`, don't record any proof.
    No,
}

impl RecordProof {
    /// Returns if `Self` == `Yes`.
    pub fn yes(&self) -> bool {
        matches!(self, Self::Yes)
    }
}

/// Will return [`RecordProof::No`] as default value.
impl Default for RecordProof {
    #[inline]
    fn default() -> Self {
        Self::No
    }
}

impl From<bool> for RecordProof {
    #[inline]
    fn from(val: bool) -> Self {
        if val {
            Self::Yes
        } else {
            Self::No
        }
    }
}

/// A block that was build by [`BlockBuilder`] plus some additional data.
///
/// This additional data includes the `storage_changes`, these changes can be applied to the
/// backend to get the state of the block. Furthermore an optional `proof` is included which
/// can be used to proof that the build block contains the expected data. The `proof` will
/// only be set when proof recording was activated.
pub struct BuiltBlock<Block: BlockT> {
    /// The actual block that was build.
    pub block: Block,
    /// The changes that need to be applied to the backend to get the state of the build block.
    pub storage_changes: StorageChanges<Block>,
    /// An optional proof that was recorded while building the block.
    pub proof: Option<StorageProof>,
}

impl<Block: BlockT> BuiltBlock<Block> {
    /// Convert into the inner values.
    pub fn into_inner(self) -> (Block, StorageChanges<Block>, Option<StorageProof>) {
        (self.block, self.storage_changes, self.proof)
    }
}

/// Block builder provider
pub trait BlockBuilderProvider<B, Block, RA>
where
    Block: BlockT,
    B: backend::Backend<Block>,
    Self: Sized,
    RA: ProvideRuntimeApi<Block>,
{
    /// Create a new block, built on top of `parent`.
    ///
    /// When proof recording is enabled, all accessed trie nodes are saved.
    /// These recorded trie nodes can be used by a third party to proof the
    /// output of this block builder without having access to the full storage.
    fn new_block_at<R: Into<RecordProof>>(
        &self,
        parent: &BlockId<Block>,
        inherent_digests: Digest,
        record_proof: R,
    ) -> sp_blockchain::Result<BlockBuilder<Block, RA, B>>;

    /// Create a new block, built on the head of the chain.
    fn new_block(
        &self,
        inherent_digests: Digest,
    ) -> sp_blockchain::Result<BlockBuilder<Block, RA, B>>;
}

/// Utility for building new (valid) blocks from a stream of extrinsics.
pub struct BlockBuilder<'a, Block: BlockT, A: ProvideRuntimeApi<Block>, B> {
    extrinsics: VecDeque<Block::Extrinsic>,
    api: ApiRef<'a, A::Api>,
    parent_hash: Block::Hash,
    backend: &'a B,
    /// The estimated size of the block header.
    estimated_header_size: usize,
}

impl<'a, Block, A, B> BlockBuilder<'a, Block, A, B>
where
    Block: BlockT,
    A: ProvideRuntimeApi<Block> + 'a,
    A::Api: BlockBuilderApi<Block> + ApiExt<Block>,
    B: backend::Backend<Block>,
{
    /// Create a new instance of builder based on the given `parent_hash` and `parent_number`.
    ///
    /// While proof recording is enabled, all accessed trie nodes are saved.
    /// These recorded trie nodes can be used by a third party to prove the
    /// output of this block builder without having access to the full storage.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api: &'a A,
        parent_hash: Block::Hash,
        parent_number: NumberFor<Block>,
        record_proof: RecordProof,
        inherent_digests: Digest,
        backend: &'a B,
        mut extrinsics: VecDeque<Block::Extrinsic>,
        maybe_inherent_data: Option<sp_inherents::InherentData>,
    ) -> Result<Self, Error> {
        let header = <<Block as BlockT>::Header as HeaderT>::new(
            parent_number + One::one(),
            Default::default(),
            Default::default(),
            parent_hash,
            inherent_digests,
        );

        let estimated_header_size = header.encoded_size();

        let mut api = api.runtime_api();

        if record_proof.yes() {
            api.record_proof();
        }

        api.initialize_block(parent_hash, &header)?;

        if let Some(inherent_data) = maybe_inherent_data {
            let inherent_extrinsics = Self::create_inherents(parent_hash, &api, inherent_data)?;
            for inherent_extrinsic in inherent_extrinsics {
                extrinsics.push_front(inherent_extrinsic)
            }
        }

        Ok(Self {
            parent_hash,
            extrinsics,
            api,
            backend,
            estimated_header_size,
        })
    }

    /// Execute the block's list of extrinsics.
    fn execute_extrinsics(&self) -> Result<(), Error> {
        let parent_hash = self.parent_hash;

        for (index, xt) in self.extrinsics.iter().enumerate() {
            let res = self.api.execute_in_transaction(|api| {
                match api.apply_extrinsic(parent_hash, xt.clone()) {
                    Ok(Ok(_)) => TransactionOutcome::Commit(Ok(())),
                    Ok(Err(tx_validity)) => TransactionOutcome::Rollback(Err(
                        ApplyExtrinsicFailed::Validity(tx_validity).into(),
                    )),
                    Err(e) => TransactionOutcome::Rollback(Err(Error::from(e))),
                }
            });

            if let Err(e) = res {
                tracing::debug!("Apply extrinsic at index {index} failed: {e}");
            }
        }

        Ok(())
    }

    fn collect_storage_changes(&self) -> Result<StorageChanges<Block>, Error> {
        let state = self.backend.state_at(self.parent_hash)?;
        let parent_hash = self.parent_hash;
        self.api
            .into_storage_changes(&state, parent_hash)
            .map_err(Error::StorageChanges)
    }

    /// Returns the state before executing the extrinsic at given extrinsic index.
    pub fn prepare_storage_changes_before(
        &self,
        extrinsic_index: usize,
    ) -> Result<StorageChanges<Block>, Error> {
        for (index, xt) in self.extrinsics.iter().enumerate() {
            if index == extrinsic_index {
                return self.collect_storage_changes();
            }

            // TODO: rethink what to do if an error occurs when executing the transaction.
            self.api.execute_in_transaction(|api| {
                let res = api.apply_extrinsic(self.parent_hash, xt.clone());
                match res {
                    Ok(Ok(_)) => TransactionOutcome::Commit(Ok(())),
                    Ok(Err(tx_validity)) => TransactionOutcome::Rollback(Err(
                        ApplyExtrinsicFailed::Validity(tx_validity).into(),
                    )),
                    Err(e) => TransactionOutcome::Rollback(Err(Error::from(e))),
                }
            })?;
        }

        Err(Error::Execution(Box::new(format!(
            "Invalid extrinsic index, got: {}, max: {}",
            extrinsic_index,
            self.extrinsics.len()
        ))))
    }

    /// Returns the state before finalizing the block.
    pub fn prepare_storage_changes_before_finalize_block(
        &self,
    ) -> Result<StorageChanges<Block>, Error> {
        self.execute_extrinsics()?;
        self.collect_storage_changes()
    }

    /// Consume the builder to build a valid `Block` containing all pushed extrinsics.
    ///
    /// Returns the build `Block`, the changes to the storage and an optional `StorageProof`
    /// supplied by `self.api`, combined as [`BuiltBlock`].
    /// The storage proof will be `Some(_)` when proof recording was enabled.
    pub fn build(mut self) -> Result<BuiltBlock<Block>, Error> {
        self.execute_extrinsics()?;

        let header = self.api.finalize_block(self.parent_hash)?;

        debug_assert_eq!(
            header.extrinsics_root().clone(),
            HashingFor::<Block>::ordered_trie_root(
                self.extrinsics.iter().map(Encode::encode).collect(),
                sp_core::storage::StateVersion::V1
            ),
        );

        let proof = self.api.extract_proof();

        let storage_changes = self.collect_storage_changes()?;

        Ok(BuiltBlock {
            block: <Block as BlockT>::new(header, self.extrinsics.into()),
            storage_changes,
            proof,
        })
    }

    /// Create the inherents for the block.
    ///
    /// Returns the inherents created by the runtime or an error if something failed.
    pub fn create_inherents(
        parent_hash: Block::Hash,
        api: &ApiRef<A::Api>,
        inherent_data: sp_inherents::InherentData,
    ) -> Result<VecDeque<Block::Extrinsic>, Error> {
        let exts = api
            .execute_in_transaction(move |api| {
                // `create_inherents` should not change any state, to ensure this we always rollback
                // the transaction.
                TransactionOutcome::Rollback(api.inherent_extrinsics(parent_hash, inherent_data))
            })
            .map_err(|e| Error::Application(Box::new(e)))?;
        Ok(VecDeque::from(exts))
    }

    /// Estimate the size of the block in the current state.
    ///
    /// If `include_proof` is `true`, the estimated size of the storage proof will be added
    /// to the estimation.
    pub fn estimate_block_size(&self, include_proof: bool) -> usize {
        let size = self.estimated_header_size + self.extrinsics.encoded_size();

        if include_proof {
            size + self
                .api
                .proof_recorder()
                .map(|pr| pr.estimate_encoded_size())
                .unwrap_or(0)
        } else {
            size
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_blockchain::HeaderBackend;
    use sp_core::Blake2Hasher;
    use sp_state_machine::Backend;
    // TODO: Remove `substrate_test_runtime_client` dependency for faster build time
    use std::collections::VecDeque;
    use substrate_test_runtime_client::{DefaultTestClientBuilderExt, TestClientBuilderExt};

    // TODO: Unlock this test, it got broken in https://github.com/subspace/subspace/pull/1548 and
    //  doesn't run on Windows at all
    #[test]
    #[ignore]
    fn block_building_storage_proof_does_not_include_runtime_by_default() {
        let (client, backend) =
            substrate_test_runtime_client::TestClientBuilder::new().build_with_backend();

        let block = BlockBuilder::new(
            &client,
            client.info().best_hash,
            client.info().best_number,
            RecordProof::Yes,
            Default::default(),
            &*backend,
            VecDeque::new(),
            Default::default(),
        )
        .unwrap()
        .build()
        .unwrap();

        let proof = block.proof.expect("Proof is build on request");

        let backend = sp_state_machine::create_proof_check_backend::<Blake2Hasher>(
            block.storage_changes.transaction_storage_root,
            proof,
        )
        .unwrap();

        assert!(backend
            .storage(sp_core::storage::well_known_keys::CODE)
            .unwrap_err()
            .contains("Database missing expected key"));
    }
}
