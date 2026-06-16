//! heed3 default-backend smoke: fresh datadir, chain + UTXO writes, LMDB persistence on reopen.
//!
//! Requires liblmdb. Build with `--features heed3`.

#[cfg(feature = "heed3")]
mod common;

#[cfg(feature = "heed3")]
mod heed3_ibd_smoke {
    use super::common::setup_mining_chain;
    use blvm_node::storage::database::{default_backend, DatabaseBackend};
    use blvm_node::storage::Storage;
    use blvm_node::{OutPoint, UTXO};
    use std::sync::Arc;
    use tempfile::TempDir;

    const BLOCKS: u64 = 50;

    fn heed3_env_exists(datadir: &std::path::Path) -> bool {
        let heed3_dir = datadir.join("heed3");
        if !heed3_dir.is_dir() {
            return false;
        }
        // LMDB data file name varies by heed3/lmdb version; any non-empty env dir is enough.
        std::fs::read_dir(&heed3_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    }

    #[test]
    fn heed3_auto_backend_fresh_datadir_connect_and_restart() {
        assert_eq!(
            default_backend(),
            DatabaseBackend::Heed3,
            "standard build must default to heed3"
        );

        let temp_dir = TempDir::new().unwrap();
        let datadir = temp_dir.path();

        let outpoint = OutPoint {
            hash: [0xab; 32],
            index: 7,
        };
        let utxo = UTXO {
            value: 50_000,
            script_pubkey: vec![0x51].into(),
            height: BLOCKS - 1,
            is_coinbase: false,
        };

        let expected_height = {
            let storage = Arc::new(Storage::new(datadir).expect("open heed3 via Storage::new"));
            assert_eq!(
                storage.utxo_value_codec(),
                blvm_node::storage::utxo_value_codec::ValueCodec::Rkyv
            );

            setup_mining_chain(&storage, BLOCKS).expect("seed chain");

            storage
                .utxos()
                .add_utxo(&outpoint, &utxo)
                .expect("persist UTXO on heed3");

            let height = storage.chain().get_height().expect("height").unwrap_or(0);
            assert!(
                height >= BLOCKS - 1,
                "expected chain height >= {} got {height}",
                BLOCKS - 1
            );

            storage
                .utxos()
                .get_utxo(&outpoint)
                .expect("read UTXO")
                .expect("UTXO must exist before drop");

            height
        };

        assert!(
            heed3_env_exists(datadir),
            "default backend must create heed3/ LMDB env under datadir"
        );

        // Restart: reopen via auto path (validates heed3 persistence after process-level drop).
        let storage2 = Storage::new(datadir).expect("reopen after drop");
        let height2 = storage2.chain().get_height().expect("height").unwrap_or(0);
        assert_eq!(
            height2, expected_height,
            "chain height must persist across reopen"
        );

        let got = storage2
            .utxos()
            .get_utxo(&outpoint)
            .expect("read UTXO after reopen")
            .expect("UTXO must persist across reopen");
        assert_eq!(got, utxo);

        // LMDB allows only one env handle per process; drop before a second open.
        drop(storage2);
        let storage3 =
            Storage::with_backend(datadir, DatabaseBackend::Heed3).expect("explicit heed3 reopen");
        assert_eq!(
            storage3.chain().get_height().expect("height").unwrap_or(0),
            expected_height
        );
        assert_eq!(
            storage3
                .utxos()
                .get_utxo(&outpoint)
                .expect("read")
                .expect("UTXO"),
            utxo
        );
    }
}
