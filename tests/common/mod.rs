//! Shared fixtures for `tests/*.rs` integration targets.
//!
//! Each integration test binary only uses a subset of these helpers; unused items are expected.

#![allow(dead_code)]

use blvm_node::Block;
use blvm_node::BlockHeader;
use blvm_node::Hash;
use blvm_node::OutPoint;
use blvm_node::Transaction;
use blvm_node::storage::blockstore::BlockStore;
use blvm_node::storage::chainstate::ChainState;
use blvm_node::storage::txindex::TxIndex;
use blvm_node::storage::utxostore::UtxoStore;
use blvm_node::{ByteString, TransactionInput, TransactionOutput};
use blvm_protocol::ProtocolVersion;
use std::collections::HashMap;
use tempfile::TempDir;

// ============================================================================
// Protocol/consensus fixtures (for mempool, RBF, and other node tests)
// ============================================================================

/// Create a minimal UTXO set (one UTXO) for protocol/mempool tests.
/// Uses blvm_protocol types so it can be shared by mempool_policy_tests, rbf, etc.
pub fn create_protocol_test_utxo_set() -> blvm_protocol::UtxoSet {
    use std::sync::Arc;
    let mut utxo_set = blvm_protocol::UtxoSet::default();
    utxo_set.insert(
        blvm_protocol::OutPoint {
            hash: [1; 32],
            index: 0,
        },
        Arc::new(blvm_protocol::UTXO {
            value: 100_000,
            script_pubkey: [0x76, 0xa9, 0x14, 0x00].repeat(20).into(),
            height: 0,
            is_coinbase: false,
        }),
    );
    utxo_set
}

/// Create a test transaction for protocol/mempool tests (configurable value and size).
pub fn create_protocol_test_tx(
    input_value: u64,
    output_value: u64,
    size: usize,
) -> blvm_protocol::Transaction {
    use blvm_protocol::{OutPoint, TransactionInput, TransactionOutput};
    blvm_protocol::Transaction {
        version: 1,
        inputs: blvm_protocol::tx_inputs![TransactionInput {
            prevout: OutPoint {
                hash: [1; 32],
                index: 0,
            },
            script_sig: vec![0; size / 2],
            sequence: 0xffffffff,
        }],
        outputs: blvm_protocol::tx_outputs![TransactionOutput {
            value: output_value as i64,
            script_pubkey: [0x76, 0xa9, 0x14].repeat(size / 2),
        }],
        lock_time: 0,
    }
}

pub struct TempDb {
    pub temp_dir: TempDir,
    pub utxo_store: UtxoStore,
    pub tx_index: TxIndex,
    pub block_store: BlockStore,
    pub chain_state: ChainState,
}

impl TempDb {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        // Use the directory path, not a file path - create_database handles the file creation
        let db_path = temp_dir.path();

        use blvm_node::storage::database::{Database, create_database, default_backend};
        let db_arc: std::sync::Arc<dyn Database> =
            std::sync::Arc::from(create_database(db_path, default_backend(), None)?);
        let utxo_store = UtxoStore::new(db_arc.clone())?;
        let tx_index = TxIndex::new(db_arc.clone())?;
        let block_store = BlockStore::new(db_arc.clone())?;
        let chain_state = ChainState::new(db_arc)?;

        Ok(TempDb {
            temp_dir,
            utxo_store,
            tx_index,
            block_store,
            chain_state,
        })
    }
}

pub struct TestTransactionBuilder {
    version: u64,
    inputs: Vec<TransactionInput>,
    outputs: Vec<TransactionOutput>,
    lock_time: u64,
}

impl Default for TestTransactionBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestTransactionBuilder {
    pub fn new() -> Self {
        Self {
            version: 1,
            inputs: Vec::new(),
            outputs: Vec::new(),
            lock_time: 0,
        }
    }

    pub fn add_input(mut self, prevout: OutPoint) -> Self {
        self.inputs.push(TransactionInput {
            prevout,
            script_sig: vec![0x51], // OP_1
            sequence: 0xffffffff,
        });
        self
    }

    pub fn add_output(mut self, value: u64, script_pubkey: ByteString) -> Self {
        self.outputs.push(TransactionOutput {
            value: value as i64,
            script_pubkey,
        });
        self
    }

    pub fn with_version(mut self, version: i32) -> Self {
        self.version = version as u64;
        self
    }

    pub fn with_lock_time(mut self, lock_time: u32) -> Self {
        self.lock_time = lock_time as u64;
        self
    }

    pub fn build(self) -> Transaction {
        Transaction {
            version: self.version,
            inputs: self.inputs.into(),
            outputs: self.outputs.into(),
            lock_time: self.lock_time,
        }
    }
}

pub struct TestBlockBuilder {
    header: BlockHeader,
    transactions: Vec<Transaction>,
}

impl Default for TestBlockBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestBlockBuilder {
    pub fn new() -> Self {
        Self {
            header: BlockHeader {
                version: 1,
                prev_block_hash: Hash::default(),
                merkle_root: Hash::default(),
                timestamp: 0,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: Vec::new(),
        }
    }

    pub fn set_prev_hash(mut self, hash: Hash) -> Self {
        self.header.prev_block_hash = hash;
        self
    }

    pub fn set_timestamp(mut self, timestamp: u32) -> Self {
        self.header.timestamp = timestamp as u64;
        self
    }

    pub fn with_version(mut self, version: i32) -> Self {
        self.header.version = version as i64;
        self
    }

    pub fn with_bits(mut self, bits: u32) -> Self {
        self.header.bits = bits as u64;
        self
    }

    pub fn with_nonce(mut self, nonce: u32) -> Self {
        self.header.nonce = nonce as u64;
        self
    }

    pub fn add_transaction(mut self, tx: Transaction) -> Self {
        self.transactions.push(tx);
        self
    }

    pub fn add_coinbase_transaction(mut self, script_pubkey: ByteString) -> Self {
        let coinbase_tx = Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffffffff,
                },
                script_sig: vec![0x51], // OP_1
                sequence: 0xffffffff,
            }],
            outputs: blvm_protocol::tx_outputs![TransactionOutput {
                value: 5000000000, // 50 BTC in satoshis
                script_pubkey,
            }],
            lock_time: 0,
        };
        self.transactions.push(coinbase_tx);
        self
    }

    pub fn build(self) -> Block {
        Block {
            header: self.header,
            transactions: self.transactions.into_boxed_slice(),
        }
    }

    pub fn build_header(self) -> BlockHeader {
        self.header
    }
}

pub struct TestUtxoSetBuilder {
    utxos: HashMap<OutPoint, TransactionOutput>,
}

impl Default for TestUtxoSetBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestUtxoSetBuilder {
    pub fn new() -> Self {
        Self {
            utxos: HashMap::new(),
        }
    }

    pub fn add_utxo(
        mut self,
        hash: Hash,
        index: u32,
        value: u64,
        script_pubkey: ByteString,
    ) -> Self {
        self.utxos.insert(
            OutPoint { hash, index },
            TransactionOutput {
                value: value as i64,
                script_pubkey,
            },
        );
        self
    }

    pub fn build(self) -> HashMap<OutPoint, TransactionOutput> {
        self.utxos
    }
}

pub fn random_hash() -> Hash {
    let mut hash = [0u8; 32];
    for b in &mut hash {
        *b = rand::random::<u8>();
    }
    Hash::from(hash)
}

pub fn random_hash20() -> [u8; 20] {
    let mut hash = [0u8; 20];
    for b in &mut hash {
        *b = rand::random::<u8>();
    }
    hash
}

pub fn p2pkh_script(pubkey_hash: [u8; 20]) -> ByteString {
    let mut script = Vec::new();
    script.push(0x76); // OP_DUP
    script.push(0xa9); // OP_HASH160
    script.push(0x14); // 20 bytes
    script.extend_from_slice(&pubkey_hash);
    script.push(0x88); // OP_EQUALVERIFY
    script.push(0xac); // OP_CHECKSIG
    script
}

pub fn valid_transaction() -> Transaction {
    TestTransactionBuilder::new()
        .add_input(OutPoint {
            hash: random_hash(),
            index: 0,
        })
        .add_output(1000, p2pkh_script(random_hash20()))
        .build()
}

pub fn unique_transaction() -> Transaction {
    TestTransactionBuilder::new()
        .add_input(OutPoint {
            hash: random_hash(),
            index: 0,
        })
        .add_output(1000, p2pkh_script(random_hash20()))
        .build()
}

pub fn valid_block_header() -> BlockHeader {
    BlockHeader {
        version: 1,
        prev_block_hash: random_hash(),
        merkle_root: random_hash(),
        timestamp: 1234567890,
        bits: 0x1d00ffff,
        nonce: 0,
    }
}

pub fn valid_block() -> Block {
    TestBlockBuilder::new()
        .add_transaction(valid_transaction())
        .build()
}

pub fn large_block(transaction_count: usize) -> Block {
    let mut builder = TestBlockBuilder::new();

    // Add coinbase transaction
    builder = builder.add_coinbase_transaction(p2pkh_script(random_hash20()));

    // Add many regular transactions
    for _ in 0..transaction_count {
        let tx = TestTransactionBuilder::new()
            .add_input(OutPoint {
                hash: random_hash(),
                index: 0,
            })
            .add_output(1000, p2pkh_script(random_hash20()))
            .build();
        builder = builder.add_transaction(tx);
    }

    builder.build()
}

pub fn default_protocol_version() -> ProtocolVersion {
    ProtocolVersion::Regtest
}

/// Bitcoin difficulty adjustment interval (BIP / Orange Paper).
pub const DIFFICULTY_INTERVAL: u64 = 2016;

/// Header-only chain length for mining-RPC integration tests (`getblocktemplate`, txid/weight checks).
/// Full [`DIFFICULTY_INTERVAL`] seeding is only needed when a test asserts retarget-boundary behavior.
pub const MINING_RPC_CHAIN_BLOCKS: u64 = 8;

/// Seed a chain with `total_blocks` headers (genesis + `total_blocks - 1` extensions).
///
/// Uses 10-minute header spacing and low-difficulty `bits` so `getblocktemplate` can compute
/// a valid target (only needs ≥2 recent headers for difficulty context).
pub fn setup_mining_chain(
    storage: &std::sync::Arc<blvm_node::storage::Storage>,
    total_blocks: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    setup_mining_chain_on(storage.as_ref(), total_blocks)
}

/// Like [`setup_mining_chain`] but accepts borrowed storage (e.g. from `Node::storage()`).
pub fn setup_mining_chain_on(
    storage: &blvm_node::storage::Storage,
    total_blocks: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use blvm_protocol::Block;

    assert!(
        total_blocks >= 2,
        "mining chain setup needs at least genesis + one block"
    );

    // Exponent 0x0f keeps targets representable in `mining::expand_target` (u128 path used by
    // `create_block_template`). Mainnet-style nBits (e.g. 0x1d00ffff) fail with "Target too large".
    let genesis_header = BlockHeader {
        version: 1,
        prev_block_hash: [0u8; 32],
        merkle_root: [0u8; 32],
        timestamp: 1_231_006_505,
        bits: 0x0f00ffff,
        nonce: 2_083_236_893,
    };
    storage.chain().initialize(&genesis_header)?;

    let genesis_block = Block {
        header: genesis_header.clone(),
        transactions: vec![Transaction {
            version: 1,
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![],
            lock_time: 0,
        }]
        .into_boxed_slice(),
    };

    let genesis_hash = storage.blocks().get_block_hash(&genesis_block);

    storage.blocks().store_block(&genesis_block)?;
    storage.blocks().store_height(0, &genesis_hash)?;
    storage
        .blocks()
        .store_recent_header(0, &genesis_block.header)?;

    let mut prev_hash = genesis_hash;
    let mut prev_timestamp = genesis_header.timestamp;

    for height in 1..total_blocks {
        let block_header = BlockHeader {
            version: 1,
            prev_block_hash: prev_hash,
            merkle_root: [height as u8; 32],
            timestamp: prev_timestamp + 600,
            bits: 0x0f00ffff,
            nonce: 0,
        };

        let block = Block {
            header: block_header.clone(),
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![],
                lock_time: 0,
            }]
            .into_boxed_slice(),
        };

        let block_hash = storage.blocks().get_block_hash(&block);

        storage.blocks().store_block(&block)?;
        storage.blocks().store_height(height, &block_hash)?;
        storage
            .blocks()
            .store_recent_header(height, &block.header)?;
        storage
            .chain()
            .update_tip(&block_hash, &block_header, height)?;

        prev_hash = block_hash;
        prev_timestamp = block_header.timestamp;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ECDSA helpers for integration tests (blvm-secp256k1 — no rust-secp256k1 crate).
// ---------------------------------------------------------------------------

/// Valid curve scalar **1**, big-endian 32-byte encoding.
#[inline]
pub fn test_secp256k1_scalar_one() -> [u8; 32] {
    let mut k = [0u8; 32];
    k[31] = 1;
    k
}

/// Small distinct test scalar (`n` in 1..=127, stored in the least significant byte).
#[inline]
pub fn test_secp256k1_scalar_small(n: u8) -> [u8; 32] {
    debug_assert!(n >= 1, "use non-zero scalar");
    let mut k = [0u8; 32];
    k[31] = n;
    k
}

/// RFC6979 ECDSA compact signature + compressed pubkey, both hex-encoded.
pub fn ecdsa_compact_sig_hex_and_pubkey_hex(
    seckey32: &[u8; 32],
    msg32: &[u8; 32],
) -> (String, String) {
    use blvm_secp256k1::ecdsa::{ecdsa_sign_compact_rfc6979, ge_to_compressed, pubkey_from_secret};
    use blvm_secp256k1::scalar::Scalar;
    let sig = ecdsa_sign_compact_rfc6979(msg32, seckey32).expect("ECDSA sign (test key)");
    let mut sec = Scalar::zero();
    assert!(
        !sec.set_b32(seckey32) && !sec.is_zero(),
        "invalid test seckey"
    );
    let pk = ge_to_compressed(&pubkey_from_secret(&sec));
    (hex::encode(sig), hex::encode(pk))
}

/// 33-byte compressed pubkey from raw seckey.
pub fn compressed_pubkey33_from_seckey(seckey32: &[u8; 32]) -> [u8; 33] {
    use blvm_secp256k1::ecdsa::{ge_to_compressed, pubkey_from_secret};
    use blvm_secp256k1::scalar::Scalar;
    let mut sec = Scalar::zero();
    assert!(!sec.set_b32(seckey32) && !sec.is_zero());
    ge_to_compressed(&pubkey_from_secret(&sec))
}
