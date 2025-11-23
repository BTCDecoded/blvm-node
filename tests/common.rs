use bllvm_node::storage::blockstore::BlockStore;
use bllvm_node::storage::chainstate::ChainState;
use bllvm_node::storage::txindex::TxIndex;
use bllvm_node::storage::utxostore::UtxoStore;
use bllvm_node::Block;
use bllvm_node::BlockHeader;
use bllvm_node::Hash;
use bllvm_node::OutPoint;
use bllvm_node::Transaction;
use bllvm_node::{ByteString, TransactionInput, TransactionOutput};
use bllvm_protocol::ProtocolVersion;
use std::collections::HashMap;
use tempfile::TempDir;

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

        use bllvm_node::storage::database::{create_database, default_backend, Database};
        let db_arc: std::sync::Arc<dyn Database> =
            std::sync::Arc::from(create_database(db_path, default_backend())?);
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
            inputs: bllvm_protocol::tx_inputs![TransactionInput {
                prevout: OutPoint {
                    hash: [0u8; 32],
                    index: 0xffffffff,
                },
                script_sig: vec![0x51], // OP_1
                sequence: 0xffffffff,
            }],
            outputs: bllvm_protocol::tx_outputs![TransactionOutput {
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
            OutPoint {
                hash,
                index: index as u64,
            },
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
    for i in 0..32 {
        hash[i] = rand::random::<u8>();
    }
    Hash::from(hash)
}

pub fn random_hash20() -> [u8; 20] {
    let mut hash = [0u8; 20];
    for i in 0..20 {
        hash[i] = rand::random::<u8>();
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
