use std::collections::HashMap;
use std::{fmt, error as stderror};

use crate::traits::{
	HashOf, BlockOf, ExternalitiesOf, AsExternalities, BaseContext, Backend,
	NullExternalities, StorageExternalities, Block,
};
use crate::chain::Operation;
use super::tree_route;

#[derive(Debug)]
pub enum Error {
	IO,
	InvalidOperation,
	ImportingGenesis,
	NotExist,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Error::IO => "IO failure".fmt(f)?,
			Error::InvalidOperation => "The operation provided is invalid".fmt(f)?,
			Error::NotExist => "Block does not exist".fmt(f)?,
			Error::ImportingGenesis => "Trying to import another genesis".fmt(f)?,
		}

		Ok(())
	}
}

impl stderror::Error for Error { }

#[derive(Clone)]
pub struct MemoryState {
	storage: HashMap<Vec<u8>, Vec<u8>>,
}

impl NullExternalities for MemoryState { }

impl AsExternalities<dyn NullExternalities> for MemoryState {
	fn as_externalities(&mut self) -> &mut (dyn NullExternalities + 'static) {
		self
	}
}

impl StorageExternalities for MemoryState {
	fn read_storage(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Box<std::error::Error>> {
		Ok(self.storage.get(key).map(|value| value.to_vec()))
	}

	fn write_storage(&mut self, key: Vec<u8>, value: Vec<u8>) {
		self.storage.insert(key, value);
	}

	fn remove_storage(&mut self, key: &[u8]) {
		self.storage.remove(key);
	}
}

impl AsExternalities<dyn StorageExternalities> for MemoryState {
	fn as_externalities(&mut self) -> &mut (dyn StorageExternalities + 'static) {
		self
	}
}

struct BlockData<C: BaseContext> {
	block: BlockOf<C>,
	state: MemoryState,
	depth: usize,
	children: Vec<HashOf<C>>,
	is_canon: bool,
}

pub struct MemoryBackend<C: BaseContext> {
	blocks_and_states: HashMap<HashOf<C>, BlockData<C>>,
	head: HashOf<C>,
	genesis: HashOf<C>,
	canon_depth_mappings: HashMap<usize, HashOf<C>>,
}

impl<C: BaseContext> Backend<C> for MemoryBackend<C> where
	MemoryState: AsExternalities<ExternalitiesOf<C>>
{
	type State = MemoryState;
	type Operation = Operation<C, Self>;
	type Error = Error;

	fn head(&self) -> HashOf<C> {
		self.head
	}

	fn genesis(&self) -> HashOf<C> {
		self.genesis
	}

	fn contains(
		&self,
		hash: &HashOf<C>
	) -> Result<bool, Error> {
		Ok(self.blocks_and_states.contains_key(hash))
	}

	fn is_canon(
		&self,
		hash: &HashOf<C>
	) -> Result<bool, Error> {
		self.blocks_and_states.get(hash)
			.map(|data| data.is_canon)
			.ok_or(Error::NotExist)
	}

	fn lookup_canon_depth(
		&self,
		depth: usize,
	) -> Result<Option<HashOf<C>>, Error> {
		Ok(self.canon_depth_mappings.get(&depth)
		   .map(|h| h.clone()))
	}

	fn children_at(
		&self,
		hash: &HashOf<C>,
	) -> Result<Vec<HashOf<C>>, Error> {
		self.blocks_and_states.get(hash)
			.map(|data| data.children.clone())
			.ok_or(Error::NotExist)
	}

	fn depth_at(
		&self,
		hash: &HashOf<C>
	) -> Result<usize, Error> {
		self.blocks_and_states.get(hash)
		   .map(|data| data.depth)
		   .ok_or(Error::NotExist)
	}

	fn block_at(
		&self,
		hash: &HashOf<C>,
	) -> Result<BlockOf<C>, Error> {
		self.blocks_and_states.get(hash)
			.map(|data| data.block.clone())
			.ok_or(Error::NotExist)
	}

	fn state_at(
		&self,
		hash: &HashOf<C>,
	) -> Result<MemoryState, Error> {
		self.blocks_and_states.get(hash)
			.map(|data| data.state.clone())
			.ok_or(Error::NotExist)
	}

	fn commit(
		&mut self,
		operation: Operation<C, Self>,
	) -> Result<(), Error> {
		let mut parent_hashes = HashMap::new();
		let mut importing: HashMap<HashOf<C>, BlockData<C>> = HashMap::new();
		let mut verifying = operation.import_block;

		// Do precheck to make sure the import operation is valid.
		loop {
			let mut progress = false;
			let mut next_verifying = Vec::new();

			for op in verifying {
				let parent_depth = match op.block.parent_hash() {
					Some(parent_hash) => {
						if self.contains(parent_hash)? {
							Some(self.depth_at(parent_hash)?)
						} else if importing.contains_key(parent_hash) {
							importing.get(parent_hash)
								.map(|data| data.depth)
						} else {
							None
						}
					},
					None => return Err(Error::ImportingGenesis),
				};
				let depth = parent_depth.map(|d| d + 1);

				if let Some(depth) = depth {
					progress = true;
					if let Some(parent_hash) = op.block.parent_hash() {
						parent_hashes.insert(*op.block.hash(), *parent_hash);
					}
					importing.insert(*op.block.hash(), BlockData {
						block: op.block,
						state: op.state,
						depth,
						children: Vec::new(),
						is_canon: false,
					});
				} else {
					next_verifying.push(op)
				}
			}

			if next_verifying.len() == 0 {
				break;
			}

			if !progress {
				return Err(Error::InvalidOperation);
			}

			verifying = next_verifying;
		}

		// Do precheck to make sure the head going to set exists.
		if let Some(new_head) = &operation.set_head {
			let head_exists = self.contains(new_head)? ||
				importing.contains_key(new_head);

			if !head_exists {
				return Err(Error::InvalidOperation);
			}
		}

		self.blocks_and_states.extend(importing);

		// Fix children at hashes.
		for (hash, parent_hash) in parent_hashes {
			self.blocks_and_states.get_mut(&parent_hash)
				.expect("Parent hash are checked to exist or has been just imported; qed")
				.children.push(hash);
		}

		if let Some(new_head) = operation.set_head {
			let route = tree_route(self, &self.head, &new_head)
				.expect("Blocks are checked to exist or importing; qed");

			for hash in route.retracted() {
				let mut block = self.blocks_and_states.get_mut(hash)
					.expect("Block is fetched from tree_route; it must exist; qed");
				block.is_canon = false;
				self.canon_depth_mappings.remove(&block.depth);
			}

			for hash in route.enacted() {
				let mut block = self.blocks_and_states.get_mut(hash)
					.expect("Block is fetched from tree_route; it must exist; qed");
				block.is_canon = true;
				self.canon_depth_mappings.insert(block.depth, *hash);
			}

			self.head = new_head;
		}

		Ok(())
	}
}

impl<C: BaseContext> MemoryBackend<C> where
	MemoryState: AsExternalities<ExternalitiesOf<C>>
{
	pub fn with_genesis(block: BlockOf<C>, genesis_storage: HashMap<Vec<u8>, Vec<u8>>) -> Self {
		assert!(block.parent_hash().is_none(), "with_genesis must be provided with a genesis block");

		let genesis_hash = *block.hash();
		let genesis_state = MemoryState {
			storage: genesis_storage,
		};
		let mut blocks_and_states = HashMap::new();
		blocks_and_states.insert(
			*block.hash(),
			BlockData {
				block,
				state: genesis_state,
				depth: 0,
				children: Vec::new(),
				is_canon: true,
			}
		);
		let mut canon_depth_mappings = HashMap::new();
		canon_depth_mappings.insert(0, genesis_hash);

		MemoryBackend {
			blocks_and_states,
			canon_depth_mappings,
			genesis: genesis_hash,
			head: genesis_hash,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::traits::*;
	use crate::chain::SharedBackend;

	#[derive(Clone)]
	pub struct DummyBlock {
		hash: usize,
		parent_hash: usize,
	}

	impl Block for DummyBlock {
		type Hash = usize;

		fn hash(&self) -> &usize { &self.hash }
		fn parent_hash(&self) -> Option<&usize> { if self.parent_hash == 0 { None } else { Some(&self.parent_hash) } }
	}

	pub trait CombinedExternalities: NullExternalities + StorageExternalities { }

	impl<T: NullExternalities + StorageExternalities> CombinedExternalities for T { }

	impl<T: CombinedExternalities + 'static> AsExternalities<dyn CombinedExternalities> for T {
		fn as_externalities(&mut self) -> &mut (dyn CombinedExternalities + 'static) {
			self
		}
	}

	#[allow(dead_code)]
	pub struct DummyContext;

	impl BaseContext for DummyContext {
		type Block = DummyBlock;
		type Externalities = dyn CombinedExternalities + 'static;
	}

	pub struct DummyExecutor;

	impl BlockExecutor<DummyContext> for DummyExecutor {
		type Error = Error;

		fn execute_block(
			&self,
			_block: &DummyBlock,
			_state: &mut (dyn CombinedExternalities + 'static),
		) -> Result<(), Error> {
			Ok(())
		}
	}

	#[test]
	fn all_traits_for_importer_are_satisfied() {
		let backend = MemoryBackend::with_genesis(
			DummyBlock {
				hash: 1,
				parent_hash: 0,
			},
			Default::default()
		);
		let executor = DummyExecutor;
		let shared = SharedBackend::new(backend);
		let _ = shared.begin_import(&executor);
	}
}
