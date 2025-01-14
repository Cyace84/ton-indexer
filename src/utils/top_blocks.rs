use anyhow::Result;
use rustc_hash::FxHashMap;
use ton_types::ByteOrderRead;

use super::{BlockStuff, StoredValue, StoredValueBuffer};

/// Stores last blocks for each workchain and shard
#[derive(Debug, Clone)]
pub struct TopBlocks {
    pub mc_block: ton_block::BlockIdExt,
    pub shard_heights: FxHashMap<ton_block::ShardIdent, u32>,
}

impl TopBlocks {
    /// Extracts last blocks for each workchain and shard from the given masterchain block
    pub fn from_mc_block(mc_block_data: &BlockStuff) -> Result<Self> {
        debug_assert!(mc_block_data.id().shard_id.is_masterchain());
        Ok(Self {
            mc_block: mc_block_data.id().clone(),
            shard_heights: mc_block_data.shard_blocks_seq_no()?,
        })
    }

    /// Checks whether the given block is equal to or greater than
    /// the last block for the given shard
    pub fn contains(&self, block_id: &ton_block::BlockIdExt) -> bool {
        self.contains_shard_seq_no(&block_id.shard_id, block_id.seq_no)
    }

    /// Checks whether the given pair of [`ton_block::ShardIdent`] and seqno
    /// is equal to or greater than the last block for the given shard.
    ///
    /// NOTE: Specified shard could be split or merged
    pub fn contains_shard_seq_no(&self, shard_ident: &ton_block::ShardIdent, seq_no: u32) -> bool {
        if shard_ident.is_masterchain() {
            seq_no >= self.mc_block.seq_no
        } else {
            match self.shard_heights.get(shard_ident) {
                Some(&top_seq_no) => seq_no >= top_seq_no,
                None => self
                    .shard_heights
                    .iter()
                    .find(|&(shard, _)| shard_ident.intersect_with(shard))
                    .map(|(_, &top_seq_no)| seq_no >= top_seq_no)
                    .unwrap_or_default(),
            }
        }
    }
}

impl StoredValue for TopBlocks {
    const SIZE_HINT: usize = 512;

    type OnStackSlice = [u8; Self::SIZE_HINT];

    fn serialize<T: StoredValueBuffer>(&self, buffer: &mut T) {
        self.mc_block.serialize(buffer);

        buffer.write_raw_slice(&(self.shard_heights.len() as u32).to_le_bytes());
        for (shard, top_block) in &self.shard_heights {
            shard.serialize(buffer);
            buffer.write_raw_slice(&top_block.to_le_bytes());
        }
    }

    fn deserialize(reader: &mut &[u8]) -> Result<Self>
    where
        Self: Sized,
    {
        let target_mc_block = ton_block::BlockIdExt::deserialize(reader)?;

        let top_blocks_len = reader.read_le_u32()? as usize;
        let mut top_blocks =
            FxHashMap::with_capacity_and_hasher(top_blocks_len, Default::default());

        for _ in 0..top_blocks_len {
            let shard = ton_block::ShardIdent::deserialize(reader)?;
            let top_block = reader.read_le_u32()?;
            top_blocks.insert(shard, top_block);
        }

        Ok(Self {
            mc_block: target_mc_block,
            shard_heights: top_blocks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_shards() {
        let mut shard_heights = FxHashMap::default();

        let main_shard =
            ton_block::ShardIdent::with_tagged_prefix(0, ton_block::SHARD_FULL).unwrap();

        let (left_shard, right_shard) = main_shard.split().unwrap();
        shard_heights.insert(left_shard, 1000);
        shard_heights.insert(right_shard, 1001);

        let top_blocks = TopBlocks {
            mc_block: ton_block::BlockIdExt {
                shard_id: ton_block::ShardIdent::masterchain(),
                seq_no: 100,
                root_hash: Default::default(),
                file_hash: Default::default(),
            },
            shard_heights,
        };

        assert!(!top_blocks.contains(&ton_block::BlockIdExt {
            shard_id: right_shard,
            seq_no: 100,
            ..Default::default()
        }));

        // Merged shard test
        assert!(!top_blocks.contains(&ton_block::BlockIdExt {
            shard_id: main_shard,
            seq_no: 100,
            ..Default::default()
        }));
        assert!(top_blocks.contains(&ton_block::BlockIdExt {
            shard_id: main_shard,
            seq_no: 10000,
            ..Default::default()
        }));

        // Split shard test
        let (right_left_shard, _) = right_shard.split().unwrap();
        assert!(!top_blocks.contains(&ton_block::BlockIdExt {
            shard_id: right_left_shard,
            seq_no: 100,
            ..Default::default()
        }));
        assert!(top_blocks.contains(&ton_block::BlockIdExt {
            shard_id: right_left_shard,
            seq_no: 10000,
            ..Default::default()
        }));
    }
}
