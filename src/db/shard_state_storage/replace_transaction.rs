use std::io::Write;
use std::sync::Arc;

use anyhow::{Context, Result};
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use ton_types::UInt256;

use super::cell_storage::*;
use super::entries_buffer::*;
use super::files_context::*;
use super::parser::*;
use crate::db::{columns, Column, Tree};
use crate::utils::*;

pub struct ShardStateReplaceTransaction<'a> {
    shard_state_db: &'a Tree<columns::ShardStates>,
    cell_storage: &'a Arc<CellStorage>,
    min_ref_mc_state: &'a Arc<MinRefMcState>,
    marker: u8,
    reader: ShardStatePacketReader,
    header: Option<BocHeader>,
    cells_read: u64,
}

impl<'a> ShardStateReplaceTransaction<'a> {
    pub fn new(
        shard_state_db: &'a Tree<columns::ShardStates>,
        cell_storage: &'a Arc<CellStorage>,
        min_ref_mc_state: &'a Arc<MinRefMcState>,
        marker: u8,
    ) -> Self {
        Self {
            shard_state_db,
            cell_storage,
            min_ref_mc_state,
            marker,
            reader: ShardStatePacketReader::new(),
            header: None,
            cells_read: 0,
        }
    }

    pub fn header(&self) -> &Option<BocHeader> {
        &self.header
    }

    pub async fn process_packet(
        &mut self,
        ctx: &mut FilesContext,
        packet: Vec<u8>,
        progress_bar: &mut ProgressBar,
    ) -> Result<bool> {
        use tokio::io::AsyncWriteExt;

        let cells_file = ctx.cells_file()?;

        self.reader.set_next_packet(packet);

        let header = loop {
            if let Some(header) = &self.header {
                break header;
            }

            let header = match self.reader.read_header()? {
                Some(header) => header,
                None => return Ok(false),
            };

            tracing::debug!(?header);
            progress_bar.set_total(header.cell_count);

            self.header = Some(header);
        };

        let mut chunk_size = 0u32;
        let mut buffer = [0; 256]; // At most 2 + 128 + 4 * 4

        while self.cells_read < header.cell_count {
            let cell_size = match self.reader.read_cell(header.ref_size, &mut buffer)? {
                Some(cell_size) => cell_size,
                None => break,
            };

            buffer[cell_size] = cell_size as u8;
            cells_file.write_all(&buffer[..cell_size + 1]).await?;

            chunk_size += cell_size as u32 + 1;
            self.cells_read += 1;
        }

        progress_bar.set_progress(self.cells_read);

        if chunk_size > 0 {
            tracing::debug!(chunk_size, "creating chunk");
            cells_file.write_u32_le(chunk_size).await?;
        }

        if self.cells_read < header.cell_count {
            return Ok(false);
        }

        if header.has_crc && self.reader.read_crc()?.is_none() {
            return Ok(false);
        }

        progress_bar.complete();
        Ok(true)
    }

    pub async fn finalize(
        self,
        ctx: &mut FilesContext,
        block_id: ton_block::BlockIdExt,
        progress_bar: &mut ProgressBar,
    ) -> Result<Arc<ShardStateStuff>> {
        // 2^7 bits + 1 bytes
        const MAX_DATA_SIZE: usize = 128;
        const CELLS_PER_BATCH: u64 = 1_000_000;

        let header = match &self.header {
            Some(header) => header,
            None => {
                return Err(ReplaceTransactionError::InvalidShardStatePacket)
                    .context("BOC header not found")
            }
        };

        let hashes_file =
            ctx.create_mapped_hashes_file(header.cell_count as usize * HashesEntry::LEN)?;
        let cells_file = ctx.create_mapped_cells_file().await?;

        let db = self.shard_state_db.raw_db_handle();
        let mut write_options = rocksdb::WriteOptions::default();
        columns::Cells::write_options(&mut write_options);

        let mut tail = [0; 4];
        let mut ctx = FinalizationContext::new();

        // Allocate on heap to prevent big future size
        let mut chunk_buffer = Vec::with_capacity(1 << 20);
        let mut data_buffer = vec![0u8; MAX_DATA_SIZE];

        let total_size = cells_file.length();
        progress_bar.set_total(total_size as u64);

        let mut file_pos = total_size;
        let mut cell_index = header.cell_count;
        let mut batch_len = 0;
        while file_pos >= 4 {
            file_pos -= 4;
            unsafe { cells_file.read_exact_at(file_pos, &mut tail) };

            let mut chunk_size = u32::from_le_bytes(tail) as usize;
            chunk_buffer.resize(chunk_size, 0);

            file_pos -= chunk_size;
            unsafe { cells_file.read_exact_at(file_pos, &mut chunk_buffer) };

            tracing::debug!(chunk_size, "processing chunk");

            {
                // NOTE: create CF on each iteration to make this future Send+Sync
                let cells_cf = db.cf_handle(columns::Cells::NAME).expect("Shouldn't fail");

                while chunk_size > 0 {
                    cell_index -= 1;
                    batch_len += 1;
                    let cell_size = chunk_buffer[chunk_size - 1] as usize;
                    chunk_size -= cell_size + 1;

                    let cell = RawCell::from_stored_data(
                        &mut &chunk_buffer[chunk_size..chunk_size + cell_size],
                        header.ref_size,
                        header.cell_count as usize,
                        cell_index as usize,
                        &mut data_buffer,
                    )?;

                    for (&index, buffer) in cell
                        .reference_indices
                        .iter()
                        .zip(ctx.entries_buffer.iter_child_buffers())
                    {
                        // SAFETY: `buffer` is guaranteed to be in separate memory area
                        unsafe {
                            hashes_file.read_exact_at(index as usize * HashesEntry::LEN, buffer)
                        }
                    }

                    self.finalize_cell(&mut ctx, &cells_cf, cell_index as u32, cell)?;

                    // SAFETY: `entries_buffer` is guaranteed to be in separate memory area
                    unsafe {
                        hashes_file.write_all_at(
                            cell_index as usize * HashesEntry::LEN,
                            ctx.entries_buffer.current_entry_buffer(),
                        )
                    };

                    chunk_buffer.truncate(chunk_size);
                }
            }

            if batch_len > CELLS_PER_BATCH {
                db.write_opt(std::mem::take(&mut ctx.write_batch), &write_options)?;
                batch_len = 0;
            }

            progress_bar.set_progress((total_size - file_pos) as u64);
            tokio::task::yield_now().await;
        }

        if batch_len > 0 {
            db.write_opt(std::mem::take(&mut ctx.write_batch), &write_options)?;
        }

        progress_bar.complete();

        let shard_state_key = (block_id.shard_id, block_id.seq_no).to_vec();

        // Current entry contains root cell
        let current_entry = ctx.entries_buffer.split_children(&[]).0;
        self.shard_state_db
            .insert(&shard_state_key, current_entry.as_reader().hash(3))?;

        // Load stored shard state
        match self.shard_state_db.get(shard_state_key)? {
            Some(root) => {
                let cell_id = UInt256::from_be_bytes(&root);

                let cell = self.cell_storage.load_cell(cell_id)?;
                Ok(Arc::new(ShardStateStuff::new(
                    block_id,
                    ton_types::Cell::with_cell_impl_arc(cell),
                    self.min_ref_mc_state,
                )?))
            }
            None => Err(ReplaceTransactionError::NotFound.into()),
        }
    }

    fn finalize_cell(
        &self,
        ctx: &mut FinalizationContext,
        cells_cf: &Arc<rocksdb::BoundColumnFamily<'_>>,
        cell_index: u32,
        cell: RawCell<'_>,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};

        let (mut current_entry, children) =
            ctx.entries_buffer.split_children(&cell.reference_indices);

        current_entry.clear();

        // Prepare mask and counters
        let data_size = (cell.bit_len / 8) + usize::from(cell.bit_len % 8 != 0);

        let mut children_mask = ton_types::LevelMask::with_mask(0);
        let mut tree_bits_count = cell.bit_len as u64;
        let mut tree_cell_count = 1;

        for (_, child) in children.iter() {
            children_mask |= child.level_mask();
            tree_bits_count += child.tree_bits_count();
            tree_cell_count += child.tree_cell_count();
        }

        let mut is_merkle_cell = false;
        let mut is_pruned_cell = false;
        let level_mask = match cell.cell_type {
            ton_types::CellType::Ordinary => children_mask,
            ton_types::CellType::PrunedBranch => {
                is_pruned_cell = true;
                ton_types::LevelMask::with_mask(cell.level_mask)
            }
            ton_types::CellType::LibraryReference => ton_types::LevelMask::with_mask(0),
            ton_types::CellType::MerkleProof => {
                is_merkle_cell = true;
                ton_types::LevelMask::for_merkle_cell(children_mask)
            }
            ton_types::CellType::MerkleUpdate => {
                is_merkle_cell = true;
                ton_types::LevelMask::for_merkle_cell(children_mask)
            }
            ton_types::CellType::Unknown => {
                return Err(ReplaceTransactionError::InvalidCell).context("Unknown cell type")
            }
        };

        if cell.level_mask != level_mask.mask() {
            return Err(ReplaceTransactionError::InvalidCell).context("Level mask mismatch");
        }

        // Save mask and counters
        current_entry.set_level_mask(level_mask);
        current_entry.set_cell_type(cell.cell_type);
        current_entry.set_tree_bits_count(tree_bits_count);
        current_entry.set_tree_cell_count(tree_cell_count);

        // Calculate hashes
        let hash_count = if is_pruned_cell {
            1
        } else {
            level_mask.level() + 1
        };

        let mut max_depths = [0u16; 4];
        for i in 0..hash_count {
            let mut hasher = Sha256::new();

            let level_mask = if is_pruned_cell {
                level_mask
            } else {
                ton_types::LevelMask::with_level(i)
            };

            let (d1, d2) = ton_types::BagOfCells::calculate_descriptor_bytes(
                cell.bit_len,
                cell.reference_indices.len() as u8,
                level_mask.mask(),
                cell.cell_type != ton_types::CellType::Ordinary,
                false,
            );

            hasher.update([d1, d2]);

            if i == 0 {
                hasher.update(&cell.data[..data_size]);
            } else {
                hasher.update(current_entry.get_hash_slice(i - 1));
            }

            for (index, child) in children.iter() {
                let child_depth = if child.cell_type() == ton_types::CellType::PrunedBranch {
                    let child_data = ctx
                        .pruned_branches
                        .get(index)
                        .ok_or(ReplaceTransactionError::InvalidCell)
                        .context("Pruned branch data not found")?;
                    child.pruned_branch_depth(i, child_data)
                } else {
                    child.depth(if is_merkle_cell { i + 1 } else { i })
                };
                hasher.update(child_depth.to_be_bytes());

                let depth = &mut max_depths[i as usize];
                *depth = std::cmp::max(*depth, child_depth + 1);

                if *depth > ton_types::MAX_DEPTH {
                    return Err(ReplaceTransactionError::InvalidCell)
                        .context("Max tree depth exceeded");
                }

                current_entry.set_depth(i, *depth);
            }

            for (index, child) in children.iter() {
                if child.cell_type() == ton_types::CellType::PrunedBranch {
                    let child_data = ctx
                        .pruned_branches
                        .get(index)
                        .ok_or(ReplaceTransactionError::InvalidCell)
                        .context("Pruned branch data not found")?;
                    let child_hash = child.pruned_branch_hash(i, child_data);
                    hasher.update(child_hash);
                } else {
                    let child_hash = child.hash(if is_merkle_cell { i + 1 } else { i });
                    hasher.update(child_hash);
                }
            }

            current_entry.set_hash(i, hasher.finalize().as_slice());
        }

        // Update pruned branches
        if is_pruned_cell {
            ctx.pruned_branches
                .insert(cell_index, cell.data[..data_size].to_vec());
        }

        // Write cell data
        let output_buffer = &mut ctx.output_buffer;
        output_buffer.clear();

        output_buffer.write_all(&[self.marker, cell.cell_type.to_u8().unwrap()])?;
        output_buffer.write_all(&(cell.bit_len as u16).to_le_bytes())?;
        output_buffer.write_all(&cell.data[0..(cell.bit_len + 8) / 8])?;
        output_buffer.write_all(&[cell.level_mask, 0, 1, hash_count])?; // level_mask, store_hashes, has_hashes, hash_count
        for i in 0..hash_count {
            output_buffer.write_all(current_entry.get_hash_slice(i))?;
        }
        output_buffer.write_all(&[1, hash_count])?; // has_depths, depth_count(same as hash_count)
        for i in 0..hash_count {
            output_buffer.write_all(current_entry.get_depth_slice(i))?;
        }

        // Write cell references
        output_buffer.write_all(&[cell.reference_indices.len() as u8])?;
        for (_, child) in children.iter() {
            output_buffer.write_all(child.hash(3))?; // repr hash
        }

        // Write counters
        output_buffer.write_all(current_entry.get_tree_counters())?;

        // Save serialized data
        if is_pruned_cell {
            let repr_hash = current_entry
                .as_reader()
                .pruned_branch_hash(3, &cell.data[..data_size]);

            ctx.write_batch
                .put_cf(cells_cf, repr_hash, output_buffer.as_slice());
        } else {
            ctx.write_batch.put_cf(
                cells_cf,
                current_entry.as_reader().hash(3),
                output_buffer.as_slice(),
            );
        };

        // Done
        Ok(())
    }
}

struct FinalizationContext {
    pruned_branches: FxHashMap<u32, Vec<u8>>,
    entries_buffer: EntriesBuffer,
    output_buffer: Vec<u8>,
    write_batch: rocksdb::WriteBatch,
}

impl FinalizationContext {
    fn new() -> Self {
        Self {
            pruned_branches: Default::default(),
            entries_buffer: EntriesBuffer::new(),
            output_buffer: Vec::with_capacity(1 << 10),
            write_batch: rocksdb::WriteBatch::default(),
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum ReplaceTransactionError {
    #[error("Not found")]
    NotFound,
    #[error("Invalid shard state packet")]
    InvalidShardStatePacket,
    #[error("Invalid cell")]
    InvalidCell,
}
