//! Block storage implementation
//!
//! Stores blocks by hash and maintains block index by height.

use crate::storage::database::{Database, Tree};
use anyhow::Result;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, BlockHeader, Hash};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[cfg(feature = "block-compression")]
use zstd;

/// Block metadata stored separately from block data for fast RPC lookups
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMetadata {
    pub n_tx: u32,
    // Could add more metadata here: size, weight, etc.
}

/// W5/N1: LMDB `blocks` value magic for original P2P `block` message payload.
/// Layout: `BLVW` ++ u32_le(len) ++ payload. Coexists with legacy bincode bodies.
pub const WIRE_BODY_MAGIC: &[u8; 4] = b"BLVW";

/// True when `data` is a W5 wire-bytes body blob (not bincode / not zstd).
#[inline]
pub fn is_wire_body_blob(data: &[u8]) -> bool {
    data.len() >= 8 && data[..4] == WIRE_BODY_MAGIC[..]
}

/// Tag a P2P block payload for LMDB storage.
pub fn encode_wire_body_blob(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(WIRE_BODY_MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Slice the P2P payload out of a W5 wire-bytes blob.
pub fn decode_wire_body_blob(data: &[u8]) -> Option<&[u8]> {
    if !is_wire_body_blob(data) {
        return None;
    }
    let len = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    if data.len() < 8 + len {
        return None;
    }
    Some(&data[8..8 + len])
}

/// Row key length when block body / header / witness / metadata are stored with a known height.
/// Prefix is big-endian height so IBD batch writes are sorted for LSM backends.
pub const BLOCK_HEIGHT_ROW_KEY_LEN: usize = 40;

/// `height_be (8) || block_hash (32)` — lexicographic order follows chain height.
#[inline]
pub fn block_height_row_key(height: u64, block_hash: &Hash) -> [u8; BLOCK_HEIGHT_ROW_KEY_LEN] {
    let mut k = [0u8; BLOCK_HEIGHT_ROW_KEY_LEN];
    k[..8].copy_from_slice(&height.to_be_bytes());
    k[8..].copy_from_slice(block_hash.as_slice());
    k
}

/// Block storage manager
pub struct BlockStore {
    #[allow(dead_code)]
    db: Arc<dyn Database>,
    blocks: Arc<dyn Tree>,
    headers: Arc<dyn Tree>,
    height_index: Arc<dyn Tree>,   // height → hash
    hash_to_height: Arc<dyn Tree>, // hash → height (reverse index for O(1) lookup)
    witnesses: Arc<dyn Tree>,
    recent_headers: Arc<dyn Tree>, // For median time-past: stores last 11+ headers by height
    block_metadata: Arc<dyn Tree>, // hash → BlockMetadata (for fast TX count lookup)
    block_undo: Arc<dyn Tree>,
    #[cfg(feature = "block-compression")]
    block_compression_enabled: bool,
    #[cfg(feature = "block-compression")]
    block_compression_level: u32,
    #[cfg(feature = "witness-compression")]
    witness_compression_enabled: bool,
    #[cfg(feature = "witness-compression")]
    witness_compression_level: u32,
    /// Optional block file reader for fallback reading
    #[cfg(feature = "rocksdb")]
    bitcoin_core_reader: Option<Arc<crate::storage::bitcoin_core_blocks::BitcoinCoreBlockReader>>,
}

impl Clone for BlockStore {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            blocks: Arc::clone(&self.blocks),
            headers: Arc::clone(&self.headers),
            height_index: Arc::clone(&self.height_index),
            hash_to_height: Arc::clone(&self.hash_to_height),
            witnesses: Arc::clone(&self.witnesses),
            recent_headers: Arc::clone(&self.recent_headers),
            block_metadata: Arc::clone(&self.block_metadata),
            block_undo: Arc::clone(&self.block_undo),
            #[cfg(feature = "block-compression")]
            block_compression_enabled: self.block_compression_enabled,
            #[cfg(feature = "block-compression")]
            block_compression_level: self.block_compression_level,
            #[cfg(feature = "witness-compression")]
            witness_compression_enabled: self.witness_compression_enabled,
            #[cfg(feature = "witness-compression")]
            witness_compression_level: self.witness_compression_level,
            #[cfg(feature = "rocksdb")]
            bitcoin_core_reader: self.bitcoin_core_reader.clone(),
        }
    }
}

impl BlockStore {
    /// Create a new block store
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        Self::new_with_compression(
            db,
            #[cfg(feature = "block-compression")]
            false, // Default: compression disabled unless explicitly enabled
            #[cfg(feature = "block-compression")]
            3, // Default compression level
            #[cfg(feature = "witness-compression")]
            false,
            #[cfg(feature = "witness-compression")]
            2,
        )
    }

    /// Create a new block store with optional Bitcoin Core block file reader (rocksdb only)
    #[cfg(feature = "rocksdb")]
    pub fn new_with_bitcoin_core_reader(
        db: Arc<dyn Database>,
        block_reader: Option<Arc<crate::storage::bitcoin_core_blocks::BitcoinCoreBlockReader>>,
    ) -> Result<Self> {
        Self::new_with_compression_and_reader(
            db,
            #[cfg(feature = "block-compression")]
            false,
            #[cfg(feature = "block-compression")]
            3,
            #[cfg(feature = "witness-compression")]
            false,
            #[cfg(feature = "witness-compression")]
            2,
            block_reader,
        )
    }

    /// Create a new block store with compression settings
    pub fn new_with_compression(
        db: Arc<dyn Database>,
        #[cfg(feature = "block-compression")] block_compression_enabled: bool,
        #[cfg(feature = "block-compression")] block_compression_level: u32,
        #[cfg(feature = "witness-compression")] witness_compression_enabled: bool,
        #[cfg(feature = "witness-compression")] witness_compression_level: u32,
    ) -> Result<Self> {
        Self::new_with_compression_and_reader(
            db,
            #[cfg(feature = "block-compression")]
            block_compression_enabled,
            #[cfg(feature = "block-compression")]
            block_compression_level,
            #[cfg(feature = "witness-compression")]
            witness_compression_enabled,
            #[cfg(feature = "witness-compression")]
            witness_compression_level,
            #[cfg(feature = "rocksdb")]
            None,
        )
    }

    /// Create a new block store with compression settings and optional block file reader
    #[cfg(feature = "rocksdb")]
    pub fn new_with_compression_and_bitcoin_core_reader(
        db: Arc<dyn Database>,
        #[cfg(feature = "block-compression")] block_compression_enabled: bool,
        #[cfg(feature = "block-compression")] block_compression_level: u32,
        #[cfg(feature = "witness-compression")] witness_compression_enabled: bool,
        #[cfg(feature = "witness-compression")] witness_compression_level: u32,
        block_reader: Option<Arc<crate::storage::bitcoin_core_blocks::BitcoinCoreBlockReader>>,
    ) -> Result<Self> {
        Self::new_with_compression_and_reader(
            db,
            #[cfg(feature = "block-compression")]
            block_compression_enabled,
            #[cfg(feature = "block-compression")]
            block_compression_level,
            #[cfg(feature = "witness-compression")]
            witness_compression_enabled,
            #[cfg(feature = "witness-compression")]
            witness_compression_level,
            block_reader,
        )
    }

    /// Create a new block store with compression settings and optional block file reader
    fn new_with_compression_and_reader(
        db: Arc<dyn Database>,
        #[cfg(feature = "block-compression")] block_compression_enabled: bool,
        #[cfg(feature = "block-compression")] block_compression_level: u32,
        #[cfg(feature = "witness-compression")] witness_compression_enabled: bool,
        #[cfg(feature = "witness-compression")] witness_compression_level: u32,
        #[cfg(feature = "rocksdb")] bitcoin_core_reader: Option<
            Arc<crate::storage::bitcoin_core_blocks::BitcoinCoreBlockReader>,
        >,
    ) -> Result<Self> {
        let blocks = Arc::from(db.open_tree("blocks")?);
        let headers = Arc::from(db.open_tree("headers")?);
        let height_index = Arc::from(db.open_tree("height_index")?);
        let hash_to_height = Arc::from(db.open_tree("hash_to_height")?);
        let witnesses = Arc::from(db.open_tree("witnesses")?);
        let recent_headers = Arc::from(db.open_tree("recent_headers")?);
        let block_metadata = Arc::from(db.open_tree("block_metadata")?);
        let block_undo = Arc::from(db.open_tree("block_undo")?);

        Ok(Self {
            db,
            blocks,
            headers,
            height_index,
            hash_to_height,
            witnesses,
            recent_headers,
            block_metadata,
            block_undo,
            #[cfg(feature = "block-compression")]
            block_compression_enabled,
            #[cfg(feature = "block-compression")]
            block_compression_level,
            #[cfg(feature = "witness-compression")]
            witness_compression_enabled,
            #[cfg(feature = "witness-compression")]
            witness_compression_level,
            #[cfg(feature = "rocksdb")]
            bitcoin_core_reader,
        })
    }

    /// Store a block
    pub fn store_block(&self, block: &Block) -> Result<()> {
        let block_hash = self.block_hash(block);
        let block_data = bincode::serialize(block)?;

        // Compress block data if compression is enabled
        #[cfg(feature = "block-compression")]
        let data_to_store = if self.block_compression_enabled {
            zstd::encode_all(&block_data[..], self.block_compression_level as i32)
                .map_err(|e| anyhow::anyhow!("Block compression failed: {}", e))?
        } else {
            block_data
        };

        #[cfg(not(feature = "block-compression"))]
        let data_to_store = block_data;

        self.blocks.insert(block_hash.as_slice(), &data_to_store)?;

        // Store header (never compressed - small and frequently accessed)
        let header_data = bincode::serialize(&block.header)?;
        self.headers.insert(block_hash.as_slice(), &header_data)?;

        // Store block metadata separately for fast RPC lookups (TX count, etc.)
        let metadata = BlockMetadata {
            n_tx: block.transactions.len() as u32,
        };
        let metadata_data = bincode::serialize(&metadata)?;
        self.block_metadata
            .insert(block_hash.as_slice(), &metadata_data)?;

        // Store header for median time-past calculation
        // We'll need height passed separately, so this will be called after store_height
        // For now, just store the header - height will be set via store_recent_header

        Ok(())
    }

    /// W5/N1: store original P2P `block` payload (includes witnesses) under height row key.
    ///
    /// Does **not** write a separate witness tree row — inject loads witnesses from the blob.
    /// Header/metadata indexes stay bincode for cheap lookups.
    pub fn store_block_wire_bytes(
        &self,
        block: &Block,
        height: u64,
        wire_payload: &[u8],
    ) -> Result<()> {
        let block_hash = self.block_hash(block);
        let row_key = block_height_row_key(height, &block_hash);
        let data_to_store = encode_wire_body_blob(wire_payload);
        self.blocks.insert(row_key.as_slice(), &data_to_store)?;

        let header_data = bincode::serialize(&block.header)?;
        self.headers.insert(row_key.as_slice(), &header_data)?;

        let metadata = BlockMetadata {
            n_tx: block.transactions.len() as u32,
        };
        let metadata_data = bincode::serialize(&metadata)?;
        self.block_metadata
            .insert(row_key.as_slice(), &metadata_data)?;

        self.store_recent_header(height, &block.header)?;
        Ok(())
    }

    /// Store a block with witness data and height
    pub fn store_block_with_witness(
        &self,
        block: &Block,
        witnesses: &[Vec<Witness>], // CRITICAL FIX: Changed from &[Witness] to &[Vec<Witness>]
        height: u64,
    ) -> Result<()> {
        let block_hash = self.block_hash(block);
        let row_key = block_height_row_key(height, &block_hash);

        let block_data = bincode::serialize(block)?;

        #[cfg(feature = "block-compression")]
        let data_to_store = if self.block_compression_enabled {
            zstd::encode_all(&block_data[..], self.block_compression_level as i32)
                .map_err(|e| anyhow::anyhow!("Block compression failed: {}", e))?
        } else {
            block_data
        };

        #[cfg(not(feature = "block-compression"))]
        let data_to_store = block_data;

        let header_data = bincode::serialize(&block.header)?;
        let metadata = BlockMetadata {
            n_tx: block.transactions.len() as u32,
        };
        let metadata_data = bincode::serialize(&metadata)?;
        let witness_blob = if !witnesses.is_empty() {
            let witness_data = bincode::serialize(witnesses)?;

            #[cfg(feature = "witness-compression")]
            let blob = if self.witness_compression_enabled {
                zstd::encode_all(&witness_data[..], self.witness_compression_level as i32)
                    .map_err(|e| anyhow::anyhow!("Witness compression failed: {}", e))?
            } else {
                witness_data
            };

            #[cfg(not(feature = "witness-compression"))]
            let blob = witness_data;

            Some(blob)
        } else {
            None
        };

        // S0: one LMDB txn (blocks+headers+meta+witness+recent) instead of 5
        // Tree::insert commits. Select-loop GAP_PERSIST was ~200ms/block on cold home LVM.
        #[cfg(feature = "heed3")]
        {
            let _ = "IBD_S0_PERSIST_ONE_TXN";
            if self.try_ibd_flush_heed3_unified(
                &[0],
                &[height],
                &[block_hash],
                &[data_to_store.clone()],
                &[Arc::new(header_data.clone())],
                &[witness_blob.clone()],
                &[metadata_data.clone()],
                &[(height, header_data.clone())],
            )? {
                return Ok(());
            }
        }

        self.blocks.insert(row_key.as_slice(), &data_to_store)?;
        self.headers.insert(row_key.as_slice(), &header_data)?;
        self.block_metadata
            .insert(row_key.as_slice(), &metadata_data)?;
        if let Some(ref witness_blob) = witness_blob {
            self.witnesses.insert(row_key.as_slice(), witness_blob)?;
        }
        self.store_recent_header(height, &block.header)?;

        Ok(())
    }

    /// True when a witness blob exists at the height row key or legacy hash-only key.
    pub fn has_witness_blob(&self, block_hash: &Hash) -> Result<bool> {
        if let Some(h) = self.get_height_by_hash(block_hash)? {
            let k = block_height_row_key(h, block_hash);
            if self.witnesses.get(&k)?.is_some() {
                return Ok(true);
            }
        }
        Ok(self.witnesses.get(block_hash.as_slice())?.is_some())
    }

    /// Store witness at the IBD row key (`height || hash`). Prefer this when height is known.
    pub fn store_witness_at_height(
        &self,
        block_hash: &Hash,
        height: u64,
        witness: &[Vec<Witness>],
    ) -> Result<()> {
        let row_key = block_height_row_key(height, block_hash);
        let witness_data = bincode::serialize(witness)?;

        #[cfg(feature = "witness-compression")]
        let data_to_store = if self.witness_compression_enabled {
            zstd::encode_all(&witness_data[..], self.witness_compression_level as i32)
                .map_err(|e| anyhow::anyhow!("Witness compression failed: {}", e))?
        } else {
            witness_data
        };

        #[cfg(not(feature = "witness-compression"))]
        let data_to_store = witness_data;

        self.witnesses.insert(row_key.as_slice(), &data_to_store)?;
        Ok(())
    }

    /// Store witness data for a block
    pub fn store_witness(&self, block_hash: &Hash, witness: &[Vec<Witness>]) -> Result<()> {
        // CRITICAL FIX: Changed from &[Witness] to &[Vec<Witness>]
        // witnesses is now Vec<Vec<Witness>> where each Vec<Witness> is for one transaction
        // and each Witness is for one input
        let witness_data = bincode::serialize(witness)?;

        // Compress witness data if compression is enabled
        #[cfg(feature = "witness-compression")]
        let data_to_store = if self.witness_compression_enabled {
            zstd::encode_all(&witness_data[..], self.witness_compression_level as i32)
                .map_err(|e| anyhow::anyhow!("Witness compression failed: {}", e))?
        } else {
            witness_data
        };

        #[cfg(not(feature = "witness-compression"))]
        let data_to_store = witness_data;

        self.witnesses
            .insert(block_hash.as_slice(), &data_to_store)?;
        Ok(())
    }

    /// Get witness data for a block
    // CRITICAL FIX: Changed return type from Option<Vec<Witness>> to Option<Vec<Vec<Witness>>>
    ///
    /// W5: when no witness tree row exists but the body is a wire blob, return witnesses
    /// parsed from that payload (single wire deser).
    pub fn get_witness(&self, block_hash: &Hash) -> Result<Option<Vec<Vec<Witness>>>> {
        if let Some(h) = self.get_height_by_hash(block_hash)? {
            let k = block_height_row_key(h, block_hash);
            if let Some(data) = self.witnesses.get(&k)? {
                #[cfg(feature = "witness-compression")]
                let witness_data = if Self::is_compressed(&data) {
                    zstd::decode_all(&data[..])
                        .map_err(|e| anyhow::anyhow!("Witness decompression failed: {}", e))?
                } else {
                    data
                };

                #[cfg(not(feature = "witness-compression"))]
                let witness_data = data;

                let witnesses: Vec<Vec<Witness>> = bincode::deserialize(&witness_data)?;
                return Ok(Some(witnesses));
            }
            // W5 wire body embeds witnesses — no separate tree row.
            if let Some(body) = self.blocks.get(&k)? {
                if let Some(payload) = decode_wire_body_blob(&body) {
                    let (_, witnesses) =
                        blvm_protocol::serialization::deserialize_block_with_witnesses(payload)
                            .map_err(|e| anyhow::anyhow!("wire witness deserialize: {e}"))?;
                    return Ok(Some(witnesses));
                }
            }
        }
        if let Some(data) = self.witnesses.get(block_hash.as_slice())? {
            // Decompress if data is compressed (auto-detect via zstd magic bytes)
            #[cfg(feature = "witness-compression")]
            let witness_data = if Self::is_compressed(&data) {
                zstd::decode_all(&data[..])
                    .map_err(|e| anyhow::anyhow!("Witness decompression failed: {}", e))?
            } else {
                data
            };

            #[cfg(not(feature = "witness-compression"))]
            let witness_data = data;

            let witnesses: Vec<Vec<Witness>> = bincode::deserialize(&witness_data)?;
            Ok(Some(witnesses))
        } else {
            Ok(None)
        }
    }

    /// Store recent headers for median time-past calculation
    /// Maintains a sliding window of the last 11+ headers by height
    pub fn store_recent_header(&self, height: u64, header: &BlockHeader) -> Result<()> {
        let height_bytes = height.to_be_bytes();
        let header_data = bincode::serialize(header)?;
        self.recent_headers.insert(&height_bytes, &header_data)?;

        // Clean up old headers (keep only last 11 for median time-past)
        // Remove headers older than height - 11
        if height > 11 {
            let remove_height = height - 12;
            let remove_bytes = remove_height.to_be_bytes();
            self.recent_headers.remove(&remove_bytes)?;
        }

        Ok(())
    }

    /// Batch-update recent headers for MTP (one `commit_no_wal` / one txn vs per-height inserts).
    /// Preserves the same put/delete semantics as repeated [`store_recent_header`](Self::store_recent_header).
    pub fn store_recent_headers_ibd_batch(&self, entries: &[(u64, &BlockHeader)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut batch = self.recent_headers.batch()?;
        for &(height, header) in entries {
            let height_bytes = height.to_be_bytes();
            let header_data = bincode::serialize(header)?;
            batch.put(&height_bytes, &header_data);
            if height > 11 {
                let remove_bytes = (height - 12).to_be_bytes();
                batch.delete(&remove_bytes);
            }
        }
        batch.commit_no_wal()?;
        Ok(())
    }

    /// RocksDB-only: one cross-CF `WriteBatch` for IBD flush. Returns `Ok(true)` if this DB is RocksDB
    /// and the write succeeded; `Ok(false)` to fall back to per-tree batches (other backends).
    #[cfg(feature = "rocksdb")]
    pub(crate) fn try_ibd_flush_rocksdb_unified(
        &self,
        flush_order: &[usize],
        heights: &[u64],
        block_hashes: &[Hash],
        block_data: &[Vec<u8>],
        header_data: &[Arc<Vec<u8>>],
        witness_blobs: &[Option<Vec<u8>>],
        metadata_blobs: &[Vec<u8>],
        recent_entries: &[(u64, Vec<u8>)],
    ) -> Result<bool> {
        use crate::storage::database::rocksdb_impl::RocksDBDatabase;

        let Some(rocks) = self.db.as_ref().as_any().downcast_ref::<RocksDBDatabase>() else {
            return Ok(false);
        };
        rocks.write_ibd_blockstore_flush_no_wal(
            flush_order,
            heights,
            block_hashes,
            block_data,
            header_data,
            witness_blobs,
            metadata_blobs,
            recent_entries,
        )?;
        Ok(true)
    }

    /// Redb-only: one write transaction for all blockstore tables + recent headers (same semantics as
    /// the per-tree path in `parallel_ibd`). Returns `Ok(true)` when `db` is Redb and the write
    /// succeeded; `Ok(false)` to use the legacy multi-batch path.
    #[cfg(feature = "redb")]
    pub(crate) fn try_ibd_flush_redb_unified(
        &self,
        flush_order: &[usize],
        heights: &[u64],
        block_hashes: &[Hash],
        block_data: &[Vec<u8>],
        header_data: &[Arc<Vec<u8>>],
        witness_blobs: &[Option<Vec<u8>>],
        metadata_blobs: &[Vec<u8>],
        recent_entries: &[(u64, Vec<u8>)],
    ) -> Result<bool> {
        use crate::storage::database::redb_impl::RedbDatabase;

        let Some(redb) = self.db.as_ref().as_any().downcast_ref::<RedbDatabase>() else {
            return Ok(false);
        };
        redb.write_ibd_blockstore_flush_no_wal(
            flush_order,
            heights,
            block_hashes,
            block_data,
            header_data,
            witness_blobs,
            metadata_blobs,
            recent_entries,
        )?;
        Ok(true)
    }

    /// TidesDB: one transaction spanning all blockstore CFs + recent headers.
    #[cfg(feature = "tidesdb")]
    pub(crate) fn try_ibd_flush_tidesdb_unified(
        &self,
        flush_order: &[usize],
        heights: &[u64],
        block_hashes: &[Hash],
        block_data: &[Vec<u8>],
        header_data: &[Arc<Vec<u8>>],
        witness_blobs: &[Option<Vec<u8>>],
        metadata_blobs: &[Vec<u8>],
        recent_entries: &[(u64, Vec<u8>)],
    ) -> Result<bool> {
        use crate::storage::database::tidesdb_impl::TidesDBDatabase;

        let Some(tdb) = self.db.as_ref().as_any().downcast_ref::<TidesDBDatabase>() else {
            return Ok(false);
        };
        tdb.write_ibd_blockstore_flush_no_wal(
            flush_order,
            heights,
            block_hashes,
            block_data,
            header_data,
            witness_blobs,
            metadata_blobs,
            recent_entries,
        )?;
        Ok(true)
    }

    /// heed3 / LMDB: one write transaction for all blockstore sub-DBs + recent headers.
    #[cfg(feature = "heed3")]
    pub(crate) fn try_ibd_flush_heed3_unified(
        &self,
        flush_order: &[usize],
        heights: &[u64],
        block_hashes: &[Hash],
        block_data: &[Vec<u8>],
        header_data: &[Arc<Vec<u8>>],
        witness_blobs: &[Option<Vec<u8>>],
        metadata_blobs: &[Vec<u8>],
        recent_entries: &[(u64, Vec<u8>)],
    ) -> Result<bool> {
        use crate::storage::database::Heed3Database;

        let Some(heed) = self.db.as_ref().as_any().downcast_ref::<Heed3Database>() else {
            return Ok(false);
        };
        heed.write_ibd_blockstore_flush_no_wal(
            flush_order,
            heights,
            block_hashes,
            block_data,
            header_data,
            witness_blobs,
            metadata_blobs,
            recent_entries,
        )?;
        Ok(true)
    }

    /// Get recent headers for median time-past calculation (BIP113)
    /// Returns up to `count` most recent headers, ordered from oldest to newest
    pub fn get_recent_headers(&self, count: usize) -> Result<Vec<BlockHeader>> {
        let mut headers = Vec::new();

        // Get current height (from height_index)
        let mut current_height: Option<u64> = None;
        let mut items: Vec<_> = self.height_index.iter().collect();
        items.reverse();
        if let Some(item) = items.into_iter().flatten().next() {
            let (height_bytes, _hash) = item;
            let mut height_bytes_array = [0u8; 8];
            height_bytes_array.copy_from_slice(&height_bytes);
            current_height = Some(u64::from_be_bytes(height_bytes_array));
        }

        if let Some(mut height) = current_height {
            // Collect headers from current_height backwards
            for _ in 0..count {
                let height_bytes = height.to_be_bytes();
                if let Some(data) = self.recent_headers.get(&height_bytes)? {
                    if let Ok(header) = bincode::deserialize::<BlockHeader>(&data) {
                        headers.push(header);
                    }
                }
                if height == 0 {
                    break;
                }
                height -= 1;
            }
        }

        // Reverse to get oldest-to-newest order (required for get_median_time_past)
        headers.reverse();
        Ok(headers)
    }

    /// Get a stored header by block height.
    ///
    /// Resolves via the durable height index → headers tree. Do **not** use the
    /// `recent_headers` sliding window here: that only retains ~12 tip heights, so
    /// gap-resume MTP seeding (e.g. start at 880001 with tip at 957k) would miss
    /// parents and fall back to tip timestamps → H05 "Invalid block header"
    /// (live 2026-07-13: Seeded 4 recent headers before 880001, then fail).
    pub fn get_header_at_height(&self, height: u64) -> Result<Option<BlockHeader>> {
        let Some(hash) = self.get_hash_by_height(height)? else {
            return Ok(None);
        };
        self.get_header(&hash)
    }

    /// Headers for BIP113 MTP immediately before `before_height` (oldest→newest, ≤11).
    ///
    /// Prefer this over [`get_recent_headers`] when validating at a height far below tip.
    pub fn headers_before_height_for_mtp(&self, before_height: u64) -> Result<Vec<BlockHeader>> {
        if before_height == 0 {
            return Ok(Vec::new());
        }
        let lo = before_height.saturating_sub(11);
        let mut headers = Vec::with_capacity(11);
        for h in lo..before_height {
            if let Some(header) = self.get_header_at_height(h)? {
                headers.push(header);
            }
        }
        Ok(headers)
    }

    /// Decode a `blocks` tree value: W5 wire blob, optional zstd, or legacy bincode.
    pub(crate) fn decode_block_blob(data: &[u8]) -> Result<Block> {
        if let Some(payload) = decode_wire_body_blob(data) {
            let (block, _) =
                blvm_protocol::serialization::deserialize_block_with_witnesses(payload)
                    .map_err(|e| anyhow::anyhow!("wire body deserialize: {e}"))?;
            return Ok(block);
        }
        #[cfg(feature = "block-compression")]
        let block_data = if Self::is_compressed(data) {
            zstd::decode_all(data)
                .map_err(|e| anyhow::anyhow!("Block decompression failed: {}", e))?
        } else {
            data.to_vec()
        };
        #[cfg(not(feature = "block-compression"))]
        let block_data = data;
        // `block-compression` yields `Vec<u8>`; otherwise `&[u8]` — both AsRef<[u8]>.
        let block: Block = bincode::deserialize(block_data)?;
        Ok(block)
    }

    /// Fetch raw `blocks` tree bytes for `hash` (height row preferred).
    /// `pub(crate)` for getdata W5 zero-copy framing (Mode T tip serve).
    pub(crate) fn load_block_blob(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        if let Some(h) = self.get_height_by_hash(hash)? {
            let k = block_height_row_key(h, hash);
            if let Some(data) = self.blocks.get(&k)? {
                return Ok(Some(data));
            }
        }
        self.blocks.get(hash.as_slice())
    }

    /// W5: one wire deser when body is a wire blob; else bincode body + witness tree.
    pub fn get_block_and_witnesses(
        &self,
        hash: &Hash,
    ) -> Result<Option<(Block, Vec<Vec<Witness>>)>> {
        let Some(data) = self.load_block_blob(hash)? else {
            #[cfg(feature = "rocksdb")]
            {
                if let Some(reader) = &self.bitcoin_core_reader {
                    if let Some(block) = reader.read_block(hash)? {
                        let w = self.get_witness(hash)?.unwrap_or_default();
                        return Ok(Some((block, w)));
                    }
                }
            }
            return Ok(None);
        };
        if let Some(payload) = decode_wire_body_blob(&data) {
            let (block, witnesses) =
                blvm_protocol::serialization::deserialize_block_with_witnesses(payload)
                    .map_err(|e| anyhow::anyhow!("wire body deserialize: {e}"))?;
            return Ok(Some((block, witnesses)));
        }
        let block = Self::decode_block_blob(&data)?;
        let witnesses = self.get_witness(hash)?.unwrap_or_default();
        Ok(Some((block, witnesses)))
    }

    /// Get a block by hash
    ///
    /// First tries to get the block from the database.
    /// If not found and block files are available, falls back to reading from files.
    pub fn get_block(&self, hash: &Hash) -> Result<Option<Block>> {
        if let Some(data) = self.load_block_blob(hash)? {
            return Ok(Some(Self::decode_block_blob(&data)?));
        }
        // Block not in database, try block files if available
        #[cfg(feature = "rocksdb")]
        {
            if let Some(reader) = &self.bitcoin_core_reader {
                return reader.read_block(hash);
            }
        }
        Ok(None)
    }

    /// Check if data is compressed (zstd magic bytes: 0x28, 0xB5, 0x2F, 0xFD)
    #[cfg(feature = "block-compression")]
    fn is_compressed(data: &[u8]) -> bool {
        data.len() >= 4 && data[0] == 0x28 && data[1] == 0xB5 && data[2] == 0x2F && data[3] == 0xFD
    }

    /// Store a block header
    pub fn store_header(&self, hash: &Hash, header: &BlockHeader) -> Result<()> {
        let header_data = bincode::serialize(header)?;
        self.headers.insert(hash.as_slice(), &header_data)?;
        Ok(())
    }

    /// Get a block header by hash
    pub fn get_header(&self, hash: &Hash) -> Result<Option<BlockHeader>> {
        if let Some(h) = self.get_height_by_hash(hash)? {
            let k = block_height_row_key(h, hash);
            if let Some(data) = self.headers.get(&k)? {
                let header: BlockHeader = bincode::deserialize(&data)?;
                return Ok(Some(header));
            }
        }
        if let Some(data) = self.headers.get(hash.as_slice())? {
            let header: BlockHeader = bincode::deserialize(&data)?;
            Ok(Some(header))
        } else {
            Ok(None)
        }
    }

    /// Store block height index
    /// Maintains both height→hash and hash→height indices for O(1) lookups
    pub fn store_height(&self, height: u64, hash: &Hash) -> Result<()> {
        let height_bytes = height.to_be_bytes();
        // Store height → hash mapping
        self.height_index.insert(&height_bytes, hash.as_slice())?;
        // Store hash → height reverse mapping for O(1) lookup
        self.hash_to_height.insert(hash.as_slice(), &height_bytes)?;
        Ok(())
    }

    /// Store multiple headers and heights in a single batch operation
    /// This is MUCH faster than individual inserts for IBD - uses atomic batch writes
    pub fn store_headers_batch(&self, entries: &[(Hash, BlockHeader, u64)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        // Create batch writers for each tree
        let mut headers_batch = self.headers.batch()?;
        let mut heights_batch = self.height_index.batch()?;
        let mut hash_to_height_batch = self.hash_to_height.batch()?;

        // Pre-serialize and add to batches
        for (hash, header, height) in entries {
            let header_data = bincode::serialize(header)?;
            let height_bytes = height.to_be_bytes();

            headers_batch.put(hash.as_slice(), &header_data);
            heights_batch.put(&height_bytes, hash.as_slice());
            hash_to_height_batch.put(hash.as_slice(), &height_bytes);
        }

        // Commit all batches atomically
        headers_batch.commit()?;
        heights_batch.commit()?;
        hash_to_height_batch.commit()?;

        Ok(())
    }

    /// Get block hash by height
    pub fn get_hash_by_height(&self, height: u64) -> Result<Option<Hash>> {
        let height_bytes = height.to_be_bytes();
        if let Some(data) = self.height_index.get(&height_bytes)? {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data);
            Ok(Some(hash))
        } else {
            Ok(None)
        }
    }

    /// Highest block height present in the height index (contiguous from genesis assumed for IBD).
    ///
    /// Used to recover `chain_info` when the `chain_info` table is missing or corrupted but
    /// block data remains. Complexity: O(log N) `get_hash_by_height` probes.
    pub fn highest_stored_height(&self) -> Result<Option<u64>> {
        if self.get_hash_by_height(0)?.is_none() {
            return Ok(None);
        }
        let mut lo = 0u64;
        let mut hi = 1u64;
        while self.get_hash_by_height(hi)?.is_some() {
            lo = hi;
            hi = hi.saturating_mul(2);
            if hi > 2_000_000_000 {
                break;
            }
        }
        if self.get_hash_by_height(hi)?.is_some() {
            return Ok(Some(hi));
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.get_hash_by_height(mid)?.is_some() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(Some(lo))
    }

    /// Get block height by hash (reverse lookup)
    /// Optimized: O(1) lookup using hash_to_height index instead of O(n) iteration
    pub fn get_height_by_hash(&self, hash: &Hash) -> Result<Option<u64>> {
        // Use reverse index for O(1) lookup instead of O(n) iteration
        if let Some(data) = self.hash_to_height.get(hash.as_slice())? {
            let mut height_bytes_array = [0u8; 8];
            height_bytes_array.copy_from_slice(&data);
            return Ok(Some(u64::from_be_bytes(height_bytes_array)));
        }
        Ok(None)
    }

    /// Build a `headers` message payload for an incoming `getheaders` (BIP130-style chain walk).
    ///
    /// Finds the first locator hash that sits on this node's contiguous height index (main chain),
    /// then returns up to `max_headers` headers starting at the **next** height. Empty vec means
    /// the peer is already at our tip (or we share no indexed ancestor).
    pub fn build_headers_response(
        &self,
        locator: &[Hash],
        hash_stop: &Hash,
        max_headers: usize,
    ) -> Result<Vec<BlockHeader>> {
        let Some(tip_h) = self.highest_stored_height()? else {
            return Ok(Vec::new());
        };

        let fork_h: Option<u64> = if locator.is_empty() {
            // Empty locator: peer wants chain from immediately after genesis.
            Some(0)
        } else {
            let mut found = None;
            for hash in locator {
                if let Some(h) = self.get_height_by_hash(hash)? {
                    if self.get_hash_by_height(h)? == Some(*hash) {
                        found = Some(h);
                        break;
                    }
                }
            }
            found
        };

        let Some(fork) = fork_h else {
            return Ok(Vec::new());
        };

        let start = fork.saturating_add(1);
        if start > tip_h {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let stop_all_zero = hash_stop.iter().all(|&b| b == 0);
        let cap = max_headers.max(1);

        for height in start..=tip_h {
            if out.len() >= cap {
                break;
            }
            let Some(hash) = self.get_hash_by_height(height)? else {
                break;
            };
            let Some(hdr) = self.get_header(&hash)? else {
                break;
            };
            out.push(hdr);
            if !stop_all_zero && hash == *hash_stop {
                break;
            }
        }

        Ok(out)
    }

    /// Get block metadata (TX count, etc.) without loading full block
    pub fn get_block_metadata(&self, hash: &Hash) -> Result<Option<BlockMetadata>> {
        if let Some(h) = self.get_height_by_hash(hash)? {
            let k = block_height_row_key(h, hash);
            if let Some(data) = self.block_metadata.get(&k)? {
                let metadata: BlockMetadata = bincode::deserialize(&data)?;
                return Ok(Some(metadata));
            }
        }
        if let Some(data) = self.block_metadata.get(hash.as_slice())? {
            let metadata: BlockMetadata = bincode::deserialize(&data)?;
            Ok(Some(metadata))
        } else {
            Ok(None)
        }
    }

    /// Get all blocks in a height range
    pub fn get_blocks_by_height_range(&self, start: u64, end: u64) -> Result<Vec<Block>> {
        let mut blocks = Vec::new();

        for height in start..=end {
            if let Some(hash) = self.get_hash_by_height(height)? {
                if let Some(block) = self.get_block(&hash)? {
                    blocks.push(block);
                }
            }
        }

        Ok(blocks)
    }

    /// Check if a block exists (body present in `blocks` tree)
    pub fn has_block(&self, hash: &Hash) -> Result<bool> {
        self.has_block_body(hash)
    }

    /// Get total number of blocks stored
    pub fn block_count(&self) -> Result<usize> {
        self.blocks.len()
    }

    /// Calculate block hash using proper Bitcoin double SHA256
    /// Get the hash of a block
    pub fn get_block_hash(&self, block: &Block) -> Hash {
        self.block_hash(block)
    }

    #[inline]
    fn block_hash(&self, block: &Block) -> Hash {
        use crate::storage::hashing::double_sha256;

        // OPTIMIZATION: Use stack-allocated array instead of heap Vec
        // Serialize block header for hashing (80 bytes total)
        // CRITICAL: Must use 4-byte types for version/timestamp/bits/nonce (Bitcoin wire format)
        let mut header_data = [0u8; 80];
        header_data[0..4].copy_from_slice(&(block.header.version as i32).to_le_bytes()); // 4 bytes
        header_data[4..36].copy_from_slice(&block.header.prev_block_hash); // 32 bytes
        header_data[36..68].copy_from_slice(&block.header.merkle_root); // 32 bytes
        header_data[68..72].copy_from_slice(&(block.header.timestamp as u32).to_le_bytes()); // 4 bytes
        header_data[72..76].copy_from_slice(&(block.header.bits as u32).to_le_bytes()); // 4 bytes
        header_data[76..80].copy_from_slice(&(block.header.nonce as u32).to_le_bytes()); // 4 bytes

        // Calculate Bitcoin double SHA256 hash
        double_sha256(&header_data)
    }

    /// Remove block body (keep header for PoW verification)
    pub fn remove_block_body(&self, hash: &Hash) -> Result<()> {
        if let Some(h) = self.get_height_by_hash(hash)? {
            let k = block_height_row_key(h, hash);
            self.blocks.remove(&k)?;
        }
        self.blocks.remove(hash.as_slice())?;
        Ok(())
    }

    /// Remove witness data for a block
    pub fn remove_witness(&self, hash: &Hash) -> Result<()> {
        if let Some(h) = self.get_height_by_hash(hash)? {
            let k = block_height_row_key(h, hash);
            self.witnesses.remove(&k)?;
        }
        self.witnesses.remove(hash.as_slice())?;
        Ok(())
    }

    /// Remove block by height (removes body, keeps header)
    pub fn remove_block_by_height(&self, height: u64) -> Result<()> {
        if let Some(hash) = self.get_hash_by_height(height)? {
            self.remove_block_body(&hash)?;
        }
        Ok(())
    }

    /// Remove blocks in a height range (removes bodies, keeps headers)
    pub fn remove_blocks_by_height_range(&self, start: u64, end: u64) -> Result<u64> {
        let mut removed = 0;
        for height in start..=end {
            if self.remove_block_by_height(height).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Check if a block body exists (not just header)
    pub fn has_block_body(&self, hash: &Hash) -> Result<bool> {
        if let Some(h) = self.get_height_by_hash(hash)? {
            let k = block_height_row_key(h, hash);
            if self.blocks.contains_key(&k)? {
                return Ok(true);
            }
        }
        self.blocks.contains_key(hash.as_slice())
    }

    // ============================================================
    // Tree accessors for batch operations (used by BufferedBlockStore)
    // ============================================================

    /// Get reference to blocks tree for batch operations
    pub fn blocks_tree(&self) -> Result<Arc<dyn Tree>> {
        Ok(Arc::clone(&self.blocks))
    }

    /// Get reference to witnesses tree for batch operations
    pub fn witnesses_tree(&self) -> Result<Arc<dyn Tree>> {
        Ok(Arc::clone(&self.witnesses))
    }

    /// Get reference to height index tree for batch operations
    pub fn height_tree(&self) -> Result<Arc<dyn Tree>> {
        Ok(Arc::clone(&self.height_index))
    }

    /// Get reference to hash-to-height tree for batch operations
    pub fn hash_to_height_tree(&self) -> Result<Arc<dyn Tree>> {
        Ok(Arc::clone(&self.hash_to_height))
    }

    /// Get reference to headers tree for batch operations
    pub fn headers_tree(&self) -> Result<Arc<dyn Tree>> {
        Ok(Arc::clone(&self.headers))
    }

    /// Get reference to block metadata tree for batch operations
    pub fn metadata_tree(&self) -> Result<Arc<dyn Tree>> {
        Ok(Arc::clone(&self.block_metadata))
    }

    /// Persist connect undo for a block hash (required for disconnect on reorg).
    #[cfg(feature = "production")]
    pub fn store_undo_log(
        &self,
        hash: &Hash,
        undo: &blvm_consensus::reorganization::BlockUndoLog,
    ) -> Result<()> {
        let data = bincode::serialize(undo)?;
        self.block_undo.insert(hash.as_slice(), &data)?;
        Ok(())
    }

    /// Write all undo logs for a chunk in a single batched write transaction.
    ///
    /// Each `Tree::insert` opens its own write transaction and calls `fdatasync` on commit.
    /// Calling `store_undo_log` in a per-block loop produces N fdatasyncs per chunk (N = chunk
    /// size, typically 50). On single-writer backends (LMDB/Heed3) this serialises with every
    /// other IBD writer and dominates block-flush latency: 50 fsyncs × 8 chunks × 2 threads
    /// ≈ 800 fsyncs per flush cycle at ~50–200 ms each = 40–160 s stall.
    ///
    /// Batching all chunk entries into one `commit_no_wal` reduces that to 1 fsync per chunk.
    #[cfg(feature = "production")]
    pub fn store_undo_logs_batch(
        &self,
        entries: &[(&Hash, &blvm_consensus::reorganization::BlockUndoLog)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut batch = self.block_undo.batch()?;
        for (hash, undo) in entries {
            let data = bincode::serialize(undo)?;
            batch.put(hash.as_slice(), &data);
        }
        batch.commit_no_wal()?;
        Ok(())
    }

    /// Load connect undo for a block hash.
    #[cfg(feature = "production")]
    pub fn get_undo_log(
        &self,
        hash: &Hash,
    ) -> Result<Option<blvm_consensus::reorganization::BlockUndoLog>> {
        match self.block_undo.get(hash.as_slice())? {
            Some(data) => Ok(Some(bincode::deserialize(&data)?)),
            None => Ok(None),
        }
    }
}
