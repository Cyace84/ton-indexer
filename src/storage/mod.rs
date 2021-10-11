use std::io::{Read, Write};

use anyhow::Result;
use rocksdb::MergeOperands;
use smallvec::SmallVec;
use ton_types::ByteOrderRead;

pub use self::archive_manager::*;
pub use self::archive_package::*;
pub use self::background_sync_meta::*;
pub use self::block_handle::*;
pub use self::block_handle_storage::*;
pub use self::block_index_db::*;
pub use self::block_meta::*;
pub use self::node_state_storage::*;
pub use self::package_entry_id::*;
pub use self::shard_state_storage::*;
pub use self::tree::*;

mod archive_manager;
mod archive_package;
mod background_sync_meta;
mod block_handle;
mod block_handle_storage;
mod block_index_db;
mod block_meta;
mod node_state_storage;
mod package_entry_id;
mod shard_state_storage;
mod storage_cell;
mod tree;

pub mod columns {
    use rocksdb::Options;

    use super::{archive_data_merge, Column};

    pub struct ArchiveStorage;
    impl Column for ArchiveStorage {
        const NAME: &'static str = "archive_storage";

        fn options(opts: &mut Options) {
            opts.set_merge_operator_associative("archive_data_merge", archive_data_merge);
        }
    }

    /// Maps block root hash to block meta
    pub struct BlockHandles;
    impl Column for BlockHandles {
        const NAME: &'static str = "block_handles";
    }

    pub struct KeyBlocks;
    impl Column for KeyBlocks {
        const NAME: &'static str = "key_blocks";
    }

    /// Maps BlockId to data
    pub struct ArchiveManagerDb;
    impl Column for ArchiveManagerDb {
        const NAME: &'static str = "archive";

        fn options(opts: &mut Options) {
            opts.set_optimize_filters_for_hits(true);
        }
    }

    pub struct ShardStateDb;
    impl Column for ShardStateDb {
        const NAME: &'static str = "shard_state_db";
    }

    pub struct CellDb<const N: u8>;
    impl Column for CellDb<0> {
        const NAME: &'static str = "cell_db";

        fn options(opts: &mut rocksdb::Options) {
            opts.set_optimize_filters_for_hits(true);
        }
    }
    impl Column for CellDb<1> {
        const NAME: &'static str = "cell_db_additional";

        fn options(opts: &mut Options) {
            opts.set_optimize_filters_for_hits(true);
        }
    }

    pub struct NodeState;
    impl Column for NodeState {
        const NAME: &'static str = "node_state";
    }

    /// Maps shard id to last_seq_no + last_lt + last_utime
    pub struct LtDesc;
    impl Column for LtDesc {
        const NAME: &'static str = "lt_desc";
    }

    /// Maps ShardIdent to lt + utime + BlockIdExt
    pub struct Lt;
    impl Column for Lt {
        const NAME: &'static str = "lt";
    }

    pub struct Prev1;
    impl Column for Prev1 {
        const NAME: &'static str = "prev1";
    }

    pub struct Prev2;
    impl Column for Prev2 {
        const NAME: &'static str = "prev2";
    }

    pub struct Next1;
    impl Column for Next1 {
        const NAME: &'static str = "next1";
    }

    pub struct Next2;
    impl Column for Next2 {
        const NAME: &'static str = "next2";
    }

    pub struct BackgroundSyncMeta;

    impl Column for BackgroundSyncMeta {
        const NAME: &'static str = "background_sync_meta";
    }
}

fn archive_data_merge(
    _: &[u8],
    current_value: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    let total_len: usize = operands.iter().map(|data| data.len()).sum();
    let mut result = Vec::with_capacity(ARCHIVE_PREFIX.len() + total_len);

    result.extend_from_slice(current_value.unwrap_or(&ARCHIVE_PREFIX));

    for data in operands {
        let data = data.strip_prefix(&ARCHIVE_PREFIX).unwrap_or(data);
        result.extend_from_slice(data);
    }

    Some(result)
}

pub trait StoredValue {
    const SIZE_HINT: usize;

    type OnStackSlice: smallvec::Array<Item = u8>;

    fn serialize<W: Write>(&self, writer: &mut W) -> Result<()>;

    fn deserialize<R: Read>(reader: &mut R) -> Result<Self>
    where
        Self: Sized;

    fn from_slice(data: &[u8]) -> Result<Self>
    where
        Self: Sized,
    {
        Self::deserialize(&mut std::io::Cursor::new(data))
    }

    fn to_vec(&self) -> Result<SmallVec<Self::OnStackSlice>> {
        let mut result = SmallVec::with_capacity(Self::SIZE_HINT);
        self.serialize(&mut result)?;
        Ok(result)
    }
}

impl StoredValue for ton_block::BlockIdExt {
    /// 4 bytes workchain id,
    /// 8 bytes shard id,
    /// 4 bytes seqno,
    /// 32 bytes root hash,
    /// 32 bytes file hash
    const SIZE_HINT: usize = 4 + 8 + 4 + 32 + 32;

    /// 96 is minimal suitable for `smallvec::Array` and `SIZE_HINT`
    type OnStackSlice = [u8; 96];

    fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        self.shard_id.serialize(writer)?;
        writer.write_all(&self.seq_no.to_le_bytes())?;
        writer.write_all(self.root_hash.as_slice())?;
        writer.write_all(self.file_hash.as_slice())?;
        Ok(())
    }

    fn deserialize<R: Read>(reader: &mut R) -> Result<Self>
    where
        Self: Sized,
    {
        let shard_id = ton_block::ShardIdent::deserialize(reader)?;
        let seq_no = reader.read_le_u32()?;
        let root_hash = ton_types::UInt256::from(reader.read_u256()?);
        let file_hash = ton_types::UInt256::from(reader.read_u256()?);
        Ok(Self::with_params(shard_id, seq_no, root_hash, file_hash))
    }
}

impl StoredValue for ton_block::ShardIdent {
    /// 4 bytes workchain id
    /// 8 bytes shard id
    const SIZE_HINT: usize = 4 + 8;

    type OnStackSlice = [u8; Self::SIZE_HINT];

    fn serialize<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(&self.workchain_id().to_le_bytes())?;
        writer.write_all(&self.shard_prefix_with_tag().to_le_bytes())?;
        Ok(())
    }

    fn deserialize<R: Read>(reader: &mut R) -> Result<Self>
    where
        Self: Sized,
    {
        let workchain_id = reader.read_le_u32()? as i32;
        let shard_prefix_tagged = reader.read_le_u64()?;
        Self::with_tagged_prefix(workchain_id, shard_prefix_tagged)
    }
}
