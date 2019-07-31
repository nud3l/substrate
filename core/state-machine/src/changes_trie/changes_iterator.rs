// Copyright 2017-2019 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Functions + iterator that traverses changes tries and returns all
//! (block, extrinsic) pairs where given key has been changed.

use std::cell::RefCell;
use std::collections::VecDeque;
use parity_codec::{Decode, Encode};
use hash_db::{HashDB, Hasher};
use num_traits::Zero;
use trie::{Recorder, MemoryDB};
use crate::changes_trie::{AnchorBlockId, ConfigurationRange, RootsStorage, Storage, BlockNumber};
use crate::changes_trie::input::{DigestIndex, ExtrinsicIndex, DigestIndexValue, ExtrinsicIndexValue};
use crate::changes_trie::storage::{TrieBackendAdapter, InMemoryStorage};
use crate::changes_trie::surface_iterator::{surface_iterator, SurfaceIterator, SKEWED_DIGEST_LEVEL};
use crate::proving_backend::ProvingBackendEssence;
use crate::trie_backend_essence::{TrieBackendEssence};

/// Return changes of given key at given blocks range.
/// `max` is the number of best known block.
/// Changes are returned in descending order (i.e. last block comes first).
pub fn key_changes<'a, H: Hasher, Number: BlockNumber>(
	config: ConfigurationRange<'a, Number>,
	storage: &'a dyn Storage<H, Number>,
	begin: Number,
	end: &'a AnchorBlockId<H::Out, Number>,
	max: Number,
	key: &'a [u8],
) -> Result<DrilldownIterator<'a, H, Number>, String> {
	// we can't query any roots before root
	let max = ::std::cmp::min(max.clone(), end.number.clone());

	Ok(DrilldownIterator {
		essence: DrilldownIteratorEssence {
			key,
			roots_storage: storage.as_roots_storage(),
			storage,
			begin: begin.clone(),
			end,
			config: config.clone(),
			surface: surface_iterator(
				config,
				max,
				begin,
				end.number.clone(),
			)?,

			extrinsics: Default::default(),
			blocks: Default::default(),

			_hasher: ::std::marker::PhantomData::<H>::default(),
		},
	})
}

/// Returns proof of changes of given key at given blocks range.
/// `max` is the number of best known block.
pub fn key_changes_proof<'a, H: Hasher, Number: BlockNumber>(
	config: ConfigurationRange<'a, Number>,
	storage: &dyn Storage<H, Number>,
	begin: Number,
	end: &AnchorBlockId<H::Out, Number>,
	max: Number,
	key: &[u8],
) -> Result<Vec<Vec<u8>>, String> {
	// we can't query any roots before root
	let max = ::std::cmp::min(max.clone(), end.number.clone());

	let mut iter = ProvingDrilldownIterator {
		essence: DrilldownIteratorEssence {
			key,
			roots_storage: storage.as_roots_storage(),
			storage,
			begin: begin.clone(),
			end,
			config: config.clone(),
			surface: surface_iterator(
				config,
				max,
				begin,
				end.number.clone(),
			)?,

			extrinsics: Default::default(),
			blocks: Default::default(),

			_hasher: ::std::marker::PhantomData::<H>::default(),
		},
		proof_recorder: Default::default(),
	};

	// iterate to collect proof
	while let Some(item) = iter.next() {
		item?;
	}

	Ok(iter.extract_proof())
}

/// Check key changes proof and return changes of the key at given blocks range.
/// `max` is the number of best known block.
/// Changes are returned in descending order (i.e. last block comes first).
pub fn key_changes_proof_check<'a, H: Hasher, Number: BlockNumber>(
	config: ConfigurationRange<'a, Number>,
	roots_storage: &dyn RootsStorage<H, Number>,
	proof: Vec<Vec<u8>>,
	begin: Number,
	end: &AnchorBlockId<H::Out, Number>,
	max: Number,
	key: &[u8]
) -> Result<Vec<(Number, u32)>, String> {
	// we can't query any roots before root
	let max = ::std::cmp::min(max.clone(), end.number.clone());

	let mut proof_db = MemoryDB::<H>::default();
	for item in proof {
		proof_db.insert(&[], &item);
	}

	let proof_db = InMemoryStorage::with_db(proof_db);
	DrilldownIterator {
		essence: DrilldownIteratorEssence {
			key,
			roots_storage,
			storage: &proof_db,
			begin: begin.clone(),
			end,
			config: config.clone(),
			surface: surface_iterator(
				config,
				max,
				begin,
				end.number.clone(),
			)?,

			extrinsics: Default::default(),
			blocks: Default::default(),

			_hasher: ::std::marker::PhantomData::<H>::default(),
		},
	}.collect()
}

/// Drilldown iterator - receives 'digest points' from surface iterator and explores
/// every point until extrinsic is found.
pub struct DrilldownIteratorEssence<'a, H, Number>
	where
		H: Hasher,
		Number: BlockNumber,
		H::Out: 'a,
{
	key: &'a [u8],
	roots_storage: &'a dyn RootsStorage<H, Number>,
	storage: &'a dyn Storage<H, Number>,
	begin: Number,
	end: &'a AnchorBlockId<H::Out, Number>,
	config: ConfigurationRange<'a, Number>,
	surface: SurfaceIterator<'a, Number>,

	extrinsics: VecDeque<(Number, u32)>,
	blocks: VecDeque<(Number, u32)>,

	_hasher: ::std::marker::PhantomData<H>,
}

impl<'a, H, Number> DrilldownIteratorEssence<'a, H, Number>
	where
		H: Hasher,
		Number: BlockNumber,
		H::Out: 'a,
{
	pub fn next<F>(&mut self, trie_reader: F) -> Option<Result<(Number, u32), String>>
		where
			F: FnMut(&dyn Storage<H, Number>, H::Out, &[u8]) -> Result<Option<Vec<u8>>, String>,
	{
		match self.do_next(trie_reader) {
			Ok(Some(res)) => Some(Ok(res)),
			Ok(None) => None,
			Err(err) => Some(Err(err)),
		}
	}

	fn do_next<F>(&mut self, mut trie_reader: F) -> Result<Option<(Number, u32)>, String>
		where
			F: FnMut(&dyn Storage<H, Number>, H::Out, &[u8]) -> Result<Option<Vec<u8>>, String>,
	{
		loop {
			if let Some((block, extrinsic)) = self.extrinsics.pop_front() {
				return Ok(Some((block, extrinsic)));
			}

			if let Some((block, level)) = self.blocks.pop_front() {
				// not having a changes trie root is an error because:
				// we never query roots for future blocks
				// AND trie roots for old blocks are known (both on full + light node)
				let trie_root = self.roots_storage.root(&self.end, block.clone())?
					.ok_or_else(|| format!("Changes trie root for block {} is not found", block.clone()))?;

				// only return extrinsics for blocks before self.max
				// most of blocks will be filtered out before pushing to `self.blocks`
				// here we just throwing away changes at digest blocks we're processing
				debug_assert!(block >= self.begin, "We shall not touch digests earlier than a range' begin");
				if block <= self.end.number {
					let extrinsics_key = ExtrinsicIndex { block: block.clone(), key: self.key.to_vec() }.encode();
					let extrinsics = trie_reader(self.storage, trie_root, &extrinsics_key);
					if let Some(extrinsics) = extrinsics? {
						let extrinsics: Option<ExtrinsicIndexValue> = Decode::decode(&mut &extrinsics[..]);
						if let Some(extrinsics) = extrinsics {
							self.extrinsics.extend(extrinsics.into_iter().rev().map(|e| (block.clone(), e)));
						}
					}
				}

				let blocks_key = DigestIndex { block: block.clone(), key: self.key.to_vec() }.encode();
				let blocks = trie_reader(self.storage, trie_root, &blocks_key);
				if let Some(blocks) = blocks? {
					let blocks: Option<DigestIndexValue<Number>> = Decode::decode(&mut &blocks[..]);
					if let Some(blocks) = blocks {
						// filter level0 blocks here because we tend to use digest blocks,
						// AND digest block changes could also include changes for out-of-range blocks
						let begin = self.begin.clone();
						let end = self.end.number.clone();
						let is_skewed_digest = level == SKEWED_DIGEST_LEVEL;
						let config = self.config.clone();
						self.blocks.extend(blocks.into_iter()
							.rev()
							.filter(|b| level > 1 || (*b >= begin && *b <= end))
							.map(|b| {
								let prev_level = if is_skewed_digest {
									config.config.digest_level_at_block(config.zero.clone(), b.clone())
										.map(|(level, _, _)| level)
										.unwrap_or_else(|| Zero::zero())
								} else {
									level - 1
								};
								(b, prev_level)
							})
						);
					}
				}

				continue;
			}

			match self.surface.next() {
				Some(Ok(block)) => self.blocks.push_back(block),
				Some(Err(err)) => return Err(err),
				None => return Ok(None),
			}
		}
	}
}

/// Exploring drilldown operator.
pub struct DrilldownIterator<'a, H, Number>
	where
		Number: BlockNumber,
		H: Hasher,
		H::Out: 'a,
{
	essence: DrilldownIteratorEssence<'a, H, Number>,
}

impl<'a, H: Hasher, Number: BlockNumber> Iterator
	for DrilldownIterator<'a, H, Number>
{
	type Item = Result<(Number, u32), String>;

	fn next(&mut self) -> Option<Self::Item> {
		self.essence.next(|storage, root, key|
			TrieBackendEssence::<_, H>::new(TrieBackendAdapter::new(storage), root).storage(key))
	}
}

/// Proving drilldown iterator.
struct ProvingDrilldownIterator<'a, H, Number>
	where
		Number: BlockNumber,
		H: Hasher,
		H::Out: 'a,
{
	essence: DrilldownIteratorEssence<'a, H, Number>,
	proof_recorder: RefCell<Recorder<H::Out>>,
}

impl<'a, H, Number> ProvingDrilldownIterator<'a, H, Number>
	where
		Number: BlockNumber,
		H: Hasher,
		H::Out: 'a,
{
	/// Consume the iterator, extracting the gathered proof in lexicographical order
	/// by value.
	pub fn extract_proof(self) -> Vec<Vec<u8>> {
		self.proof_recorder.into_inner().drain()
			.into_iter()
			.map(|n| n.data.to_vec())
			.collect()
	}
}

impl<'a, H, Number> Iterator for ProvingDrilldownIterator<'a, H, Number>
	where
		Number: BlockNumber,
		H: Hasher,
		H::Out: 'a,
{
	type Item = Result<(Number, u32), String>;

	fn next(&mut self) -> Option<Self::Item> {
		let proof_recorder = &mut *self.proof_recorder.try_borrow_mut()
			.expect("only fails when already borrowed; storage() is non-reentrant; qed");
		self.essence.next(|storage, root, key|
			ProvingBackendEssence::<_, H> {
				backend: &TrieBackendEssence::new(TrieBackendAdapter::new(storage), root),
				proof_recorder,
			}.storage(key))
	}
}

#[cfg(test)]
mod tests {
	use std::iter::FromIterator;
	use primitives::Blake2Hasher;
	use crate::changes_trie::Configuration;
	use crate::changes_trie::input::InputPair;
	use crate::changes_trie::storage::InMemoryStorage;
	use super::*;

	fn prepare_for_drilldown() -> (Configuration, InMemoryStorage<Blake2Hasher, u64>) {
		let config = Configuration { digest_interval: 4, digest_levels: 2 };
		let backend = InMemoryStorage::with_inputs(vec![
			// digest: 1..4 => [(3, 0)]
			(1, vec![]),
			(2, vec![]),
			(3, vec![
				InputPair::ExtrinsicIndex(ExtrinsicIndex { block: 3, key: vec![42] }, vec![0]),
			]),
			(4, vec![
				InputPair::DigestIndex(DigestIndex { block: 4, key: vec![42] }, vec![3]),
			]),
			// digest: 5..8 => [(6, 3), (8, 1+2)]
			(5, vec![]),
			(6, vec![
				InputPair::ExtrinsicIndex(ExtrinsicIndex { block: 6, key: vec![42] }, vec![3]),
			]),
			(7, vec![]),
			(8, vec![
				InputPair::ExtrinsicIndex(ExtrinsicIndex { block: 8, key: vec![42] }, vec![1, 2]),
				InputPair::DigestIndex(DigestIndex { block: 8, key: vec![42] }, vec![6]),
			]),
			// digest: 9..12 => []
			(9, vec![]),
			(10, vec![]),
			(11, vec![]),
			(12, vec![]),
			// digest: 0..16 => [4, 8]
			(13, vec![]),
			(14, vec![]),
			(15, vec![]),
			(16, vec![
				InputPair::DigestIndex(DigestIndex { block: 16, key: vec![42] }, vec![4, 8]),
			]),
		]);

		(config, backend)
	}

	fn configuration_range<'a>(config: &'a Configuration, zero: u64) -> ConfigurationRange<'a, u64> {
		ConfigurationRange {
			config,
			zero,
			end: None,
		}
	}

	#[test]
	fn drilldown_iterator_works() {
		let (config, storage) = prepare_for_drilldown();
		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 1, &AnchorBlockId { hash: Default::default(), number: 16 }, 16, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![(8, 2), (8, 1), (6, 3), (3, 0)]));

		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 1, &AnchorBlockId { hash: Default::default(), number: 2 }, 4, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![]));

		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 1, &AnchorBlockId { hash: Default::default(), number: 3 }, 4, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![(3, 0)]));

		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 1, &AnchorBlockId { hash: Default::default(), number: 7 }, 7, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![(6, 3), (3, 0)]));

		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 7, &AnchorBlockId { hash: Default::default(), number: 8 }, 8, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![(8, 2), (8, 1)]));

		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 5, &AnchorBlockId { hash: Default::default(), number: 7 }, 8, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![(6, 3)]));
	}

	#[test]
	fn drilldown_iterator_fails_when_storage_fails() {
		let (config, storage) = prepare_for_drilldown();
		storage.clear_storage();

		assert!(key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 1, &AnchorBlockId { hash: Default::default(), number: 100 }, 1000, &[42])
			.and_then(|i| i.collect::<Result<Vec<_>, _>>()).is_err());
	}

	#[test]
	fn drilldown_iterator_fails_when_range_is_invalid() {
		let (config, storage) = prepare_for_drilldown();
		assert!(key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 1, &AnchorBlockId { hash: Default::default(), number: 100 }, 50, &[42]).is_err());
		assert!(key_changes::<Blake2Hasher, u64>(
			configuration_range(&config, 0), &storage, 20, &AnchorBlockId { hash: Default::default(), number: 10 }, 100, &[42]).is_err());
	}


	#[test]
	fn proving_drilldown_iterator_works() {
		// happens on remote full node:

		// create drilldown iterator that records all trie nodes during drilldown
		let (remote_config, remote_storage) = prepare_for_drilldown();
		let remote_proof = key_changes_proof::<Blake2Hasher, u64>(
			configuration_range(&remote_config, 0), &remote_storage,
			1, &AnchorBlockId { hash: Default::default(), number: 16 }, 16, &[42]).unwrap();

		// happens on local light node:

		// create drilldown iterator that works the same, but only depends on trie
		let (local_config, local_storage) = prepare_for_drilldown();
		local_storage.clear_storage();
		let local_result = key_changes_proof_check::<Blake2Hasher, u64>(
			configuration_range(&local_config, 0), &local_storage, remote_proof,
			1, &AnchorBlockId { hash: Default::default(), number: 16 }, 16, &[42]);

		// check that drilldown result is the same as if it was happening at the full node
		assert_eq!(local_result, Ok(vec![(8, 2), (8, 1), (6, 3), (3, 0)]));
	}

	#[test]
	fn drilldown_iterator_works_with_skewed_digest() {
		let config = Configuration { digest_interval: 4, digest_levels: 3 };
		let mut config_range = configuration_range(&config, 0);
		config_range.end = Some(91);

		// when 4^3 deactivates at block 91:
		// last L3 digest has been created at block#64
		// skewed digest covers:
		// L2 digests at blocks: 80
		// L1 digests at blocks: 84, 88
		// regular blocks: 89, 90, 91
		let mut input = (1u64..92u64).map(|b| (b, vec![])).collect::<Vec<_>>();
		// changed at block#63 and covered by L3 digest at block#64
		input[63 - 1].1.push(InputPair::ExtrinsicIndex(ExtrinsicIndex { block: 63, key: vec![42] }, vec![0]));
		input[64 - 1].1.push(InputPair::DigestIndex(DigestIndex { block: 64, key: vec![42] }, vec![63]));
		// changed at block#79 and covered by L2 digest at block#80 + skewed digest at block#91
		input[79 - 1].1.push(InputPair::ExtrinsicIndex(ExtrinsicIndex { block: 79, key: vec![42] }, vec![1]));
		input[80 - 1].1.push(InputPair::DigestIndex(DigestIndex { block: 80, key: vec![42] }, vec![79]));
		input[91 - 1].1.push(InputPair::DigestIndex(DigestIndex { block: 91, key: vec![42] }, vec![80]));
		let storage = InMemoryStorage::with_inputs(input);

		let drilldown_result = key_changes::<Blake2Hasher, u64>(
			config_range, &storage, 1, &AnchorBlockId { hash: Default::default(), number: 91 }, 100_000u64, &[42])
			.and_then(Result::from_iter);
		assert_eq!(drilldown_result, Ok(vec![(79, 1), (63, 0)]));
	}
}
