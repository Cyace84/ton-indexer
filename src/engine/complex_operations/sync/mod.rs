/// This file is a modified copy of the file from https://github.com/tonlabs/ton-labs-node
///
/// Changes:
/// - replaced old `failure` crate with `anyhow`
/// - greatly improved sync speed
/// - added sync from old blocks
///
///
/// TODO: rewrite stream items handling 🌚
use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::engine::Engine;
use crate::storage::*;
use crate::utils::*;

use self::archive_downloader::*;
use self::block_maps::*;

mod archive_downloader;
mod block_maps;

pub async fn sync(engine: &Arc<Engine>) -> Result<()> {
    log::info!("Started sync");

    let active_peers = Arc::new(ActivePeers::default());
    let last_applied = engine.last_applied_block()?.seq_no;

    log::info!("Creating archives stream from {}", last_applied);
    let mut archives_stream = ArchiveDownloader::builder()
        .step(ARCHIVE_SLICE)
        .range(last_applied + 1..)
        .start(engine, &active_peers);

    log::info!("sync: Starting fast sync");

    let mut prev_step = std::time::Instant::now();
    while let Some(archive) = archives_stream.recv().await {
        log::info!(
            "sync: Time from prev step: {} ms",
            prev_step.elapsed().as_millis()
        );
        prev_step = std::time::Instant::now();

        let mc_block_id = engine.last_applied_block()?;

        let import_start = std::time::Instant::now();

        match import_package(engine, archive.clone(), &mc_block_id).await {
            Ok(()) => {
                log::info!(
                    "sync: Imported archive package for block {}. Took: {} ms",
                    mc_block_id.seq_no,
                    import_start.elapsed().as_millis()
                );
            }
            Err(e) => {
                log::error!(
                    "sync: Failed to apply queued archive for block {}: {:?}",
                    mc_block_id.seq_no,
                    e
                );
            }
        }

        log::info!(
            "sync: Full cycle took: {} ms",
            prev_step.elapsed().as_millis()
        );

        if engine.is_synced()? {
            break;
        } else {
            archive.accept();
        }
    }

    Ok(())
}

async fn import_package(
    engine: &Arc<Engine>,
    maps: Arc<BlockMaps>,
    last_mc_block_id: &ton_block::BlockIdExt,
) -> Result<()> {
    if maps.mc_block_ids.is_empty() {
        return Err(SyncError::EmptyArchivePackage.into());
    }
    profl::start!(t);
    import_mc_blocks(engine, maps.clone(), last_mc_block_id).await?;
    profl::tick!(t =>"import_mc_blocks");
    profl::span!(
        "import_shard_blocks",
        import_shard_blocks(engine, maps).await?
    );
    profl::tick!(t =>"import_package");
    Ok(())
}

async fn import_mc_blocks(
    engine: &Arc<Engine>,
    maps: Arc<BlockMaps>,
    mut last_mc_block_id: &ton_block::BlockIdExt,
) -> Result<()> {
    for id in maps.mc_block_ids.values() {
        if id.seq_no <= last_mc_block_id.seq_no {
            if id.seq_no == last_mc_block_id.seq_no && last_mc_block_id != id {
                return Err(SyncError::MasterchainBlockIdMismatch.into());
            }
            continue;
        }

        if id.seq_no != last_mc_block_id.seq_no + 1 {
            log::error!(
                "Seqno: {}, expected: {}",
                id.seq_no,
                last_mc_block_id.seq_no + 1
            );
            return Err(SyncError::BlocksSkippedInArchive.into());
        }

        last_mc_block_id = id;
        if let Some(handle) = engine.load_block_handle(last_mc_block_id)? {
            if handle.meta().is_applied() {
                continue;
            }
        }

        let entry = maps.blocks.get(last_mc_block_id).unwrap();

        let (block, block_proof) = entry.get_data()?;
        let handle = save_block(engine, last_mc_block_id, block, block_proof, None).await?;

        engine
            .apply_block_ext(&handle, block, last_mc_block_id.seq_no, false, 0)
            .await?;
    }

    log::info!("Last applied masterchain block id: {}", last_mc_block_id);
    Ok(())
}

async fn import_shard_blocks(engine: &Arc<Engine>, maps: Arc<BlockMaps>) -> Result<()> {
    for (id, entry) in &maps.blocks {
        if !id.shard_id.is_masterchain() {
            let (block, block_proof) = entry.get_data()?;

            profl::span!(
                "save_shard_block",
                save_block(engine, id, block, block_proof, None).await?
            );
        }
    }

    let mut last_applied_mc_block_id = engine.load_shards_client_mc_block_id()?;
    for mc_block_id in maps.mc_block_ids.values() {
        let mc_seq_no = mc_block_id.seq_no;
        if mc_seq_no <= last_applied_mc_block_id.seq_no {
            continue;
        }

        let masterchain_handle = engine
            .load_block_handle(mc_block_id)?
            .ok_or(SyncError::MasterchainBlockNotFound)?;
        let masterchain_block = engine.load_block_data(&masterchain_handle).await?;
        let shard_blocks = masterchain_block.shards_blocks()?;

        let mut tasks = Vec::with_capacity(shard_blocks.len());
        for (_, id) in shard_blocks {
            let engine = engine.clone();
            let maps = maps.clone();
            tasks.push(tokio::spawn(async move {
                let handle = engine
                    .load_block_handle(&id)?
                    .ok_or(SyncError::ShardchainBlockHandleNotFound)?;
                if handle.meta().is_applied() {
                    return Ok(());
                }

                if id.seq_no == 0 {
                    super::boot::download_zero_state(&engine, &id).await?;
                    return Ok(());
                }

                let block = match maps.blocks.get(&id) {
                    Some(entry) => match &entry.block {
                        Some(block) => Some(Cow::Borrowed(block)),
                        None => engine.load_block_data(&handle).await.ok().map(Cow::Owned),
                    },
                    None => engine.load_block_data(&handle).await.ok().map(Cow::Owned),
                };

                match block {
                    Some(block) => {
                        engine
                            .apply_block_ext(&handle, block.as_ref(), mc_seq_no, false, 0)
                            .await
                    }
                    None => {
                        log::info!("Downloading sc block for {}", mc_seq_no);
                        engine
                            .download_and_apply_block(handle.id(), mc_seq_no, false, 0)
                            .await
                    }
                }
            }));
        }

        futures::future::try_join_all(tasks)
            .await?
            .into_iter()
            .find(|item| item.is_err())
            .unwrap_or(Ok(()))?;

        engine.store_shards_client_mc_block_id(mc_block_id)?;
        last_applied_mc_block_id = mc_block_id.clone();
    }

    Ok(())
}

async fn save_block(
    engine: &Arc<Engine>,
    block_id: &ton_block::BlockIdExt,
    block: &BlockStuff,
    block_proof: &BlockProofStuff,
    prev_key_block_id: Option<&ton_block::BlockIdExt>,
) -> Result<Arc<BlockHandle>> {
    if block_proof.is_link() {
        block_proof.check_proof_link()?;
    } else {
        let (virt_block, virt_block_info) = block_proof.pre_check_block_proof()?;
        let handle = match prev_key_block_id {
            Some(block_id) => engine
                .db
                .load_block_handle(block_id)?
                .context("Prev key block not found")?,
            None => {
                let prev_key_block_seqno = virt_block_info.prev_key_block_seqno();
                engine.db.load_key_block_handle(prev_key_block_seqno)?
            }
        };

        if handle.id().seq_no == 0 {
            let zero_state = engine.load_mc_zero_state().await?;
            block_proof.check_with_master_state(&zero_state)?;
        } else {
            let prev_key_block_proof = engine.load_block_proof(&handle, false).await?;
            if let Err(e) = check_with_prev_key_block_proof(
                block_proof,
                &prev_key_block_proof,
                &virt_block,
                &virt_block_info,
            ) {
                if !engine.is_hard_fork(handle.id()) {
                    return Err(e);
                }
                log::warn!("Received hard fork block {}. Ignoring proof", handle.id());
            };
        }
    }

    let handle = engine.store_block_data(block).await?.handle;
    let handle = engine
        .store_block_proof(block_id, Some(handle), block_proof)
        .await?
        .handle;
    Ok(handle)
}

pub async fn background_sync(engine: &Arc<Engine>, from_seqno: u32) -> Result<()> {
    let store = engine.db.background_sync_store();

    log::info!("Download previous history sync since block {}", from_seqno);

    let high = store.load_high_key_block()?.seq_no;
    let low = match store.load_low_key_block()? {
        Some(low) => {
            log::warn!(
                "Ignoring `from_seqno` param, using saved seqno: {}",
                low.seq_no
            );
            low.seq_no
        }
        None => from_seqno,
    };

    let prev_key_block = engine.load_prev_key_block(low).await?;

    log::info!("Started background sync from {} to {}", low, high);
    download_archives(engine, low, high, prev_key_block)
        .await
        .context("Failed downloading archives")?;

    log::info!("Background sync complete");
    Ok(())
}

async fn download_archives(
    engine: &Arc<Engine>,
    low_seqno: u32,
    high_seqno: u32,
    mut prev_key_block_id: ton_block::BlockIdExt,
) -> Result<()> {
    let mut context = SaveContext {
        peers: Arc::new(Default::default()),
        high_seqno,
        prev_key_block_id: &mut prev_key_block_id,
    };

    let mut archive_downloader = ArchiveDownloader::builder()
        .step(ARCHIVE_SLICE)
        .range(low_seqno..=high_seqno)
        .start(engine, &context.peers);

    while let Some(archive) = archive_downloader.recv().await {
        let res = profl::span!(
            "save_background_sync_archive",
            save_archive(engine, archive.clone(), &mut context).await?
        );

        if let SyncStatus::Done = res {
            break;
        } else {
            archive.accept();
        }
    }

    Ok(())
}

enum SyncStatus {
    Done,
    InProgress,
    NoBlocksInArchive,
}

struct SaveContext<'a> {
    peers: Arc<ActivePeers>,
    high_seqno: u32,
    prev_key_block_id: &'a mut ton_block::BlockIdExt,
}

async fn save_archive(
    engine: &Arc<Engine>,
    maps: Arc<BlockMaps>,
    context: &mut SaveContext<'_>,
) -> Result<SyncStatus> {
    //todo store block maps in db
    let lowest_id = match maps.lowest_mc_id() {
        Some(a) => a,
        None => return Ok(SyncStatus::NoBlocksInArchive),
    };
    let highest_id = match maps.highest_mc_id() {
        Some(a) => a,
        None => return Ok(SyncStatus::NoBlocksInArchive),
    };
    log::debug!(
        "Saving archive. Low id: {}. High id: {}",
        lowest_id,
        highest_id.seq_no
    );

    for (id, entry) in &maps.blocks {
        let (block, proof) = entry.get_data()?;

        let handle = match engine.load_block_handle(block.id())? {
            Some(handle) => handle,
            None => profl::span!(
                "save_background_sync_block",
                save_block(engine, id, block, proof, Some(context.prev_key_block_id))
                    .await
                    .context("Failed saving block")?
            ),
        };

        engine
            .notify_subscribers_with_archive_block(&handle, block, proof)
            .await
            .context("Failed to process archive block")?;

        if handle.is_key_block() {
            *context.prev_key_block_id = id.clone();
        }
    }

    Ok({
        engine
            .db
            .background_sync_store()
            .store_low_key_block(highest_id)?;
        log::info!(
            "Background sync: Saved archive from {} to {}",
            lowest_id,
            highest_id
        );

        if highest_id.seq_no >= context.high_seqno {
            SyncStatus::Done
        } else {
            SyncStatus::InProgress
        }
    })
}

impl Engine {
    fn last_applied_block(&self) -> Result<ton_block::BlockIdExt> {
        let mc_block_id = self.load_last_applied_mc_block_id()?;
        let sc_block_id = self.load_shards_client_mc_block_id()?;
        log::info!("sync: Last applied block id: {}", mc_block_id);
        log::info!("sync: Last shards client block id: {}", sc_block_id);

        Ok(match (mc_block_id, sc_block_id) {
            (mc_block_id, sc_block_id) if mc_block_id.seq_no > sc_block_id.seq_no => sc_block_id,
            (mc_block_id, _) => mc_block_id,
        })
    }
}

#[derive(thiserror::Error, Debug)]
enum SyncError {
    #[error("Empty archive package")]
    EmptyArchivePackage,
    #[error("Masterchain block id mismatch")]
    MasterchainBlockIdMismatch,
    #[error("Some blocks are missing in archive")]
    BlocksSkippedInArchive,
    #[error("Masterchain block not found")]
    MasterchainBlockNotFound,
    #[error("Shardchain block handle not found")]
    ShardchainBlockHandleNotFound,
}
