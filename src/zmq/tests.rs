//! Tests for ZMQ notification publisher

#[cfg(all(test, feature = "zmq"))]
mod tests {
    use crate::zmq::{ZmqConfig, ZmqPublisher};
    use bllvm_protocol::{Block, Hash, Transaction};
    use std::time::Duration;
    use tokio::time::timeout;
    use zmq::{Context, SUB};

    fn create_test_block() -> Block {
        use bllvm_protocol::BlockHeader;
        Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                timestamp: 1234567890,
                bits: 0x1d00ffff,
                nonce: 0,
            },
            transactions: vec![].into_boxed_slice(),
        }
    }

    fn create_test_transaction() -> Transaction {
        use bllvm_protocol::{tx_inputs, tx_outputs};
        Transaction {
            version: 1,
            inputs: tx_inputs![],
            outputs: tx_outputs![],
            lock_time: 0,
        }
    }

    #[tokio::test]
    async fn test_zmq_publisher_creation() {
        let config = ZmqConfig {
            hashblock: Some("tcp://127.0.0.1:28332".to_string()),
            hashtx: None,
            rawblock: None,
            rawtx: None,
            sequence: None,
        };

        let publisher = ZmqPublisher::new(&config);
        assert!(publisher.is_ok());
    }

    #[tokio::test]
    async fn test_zmq_hashblock_notification() {
        let config = ZmqConfig {
            hashblock: Some("tcp://127.0.0.1:28333".to_string()),
            hashtx: None,
            rawblock: None,
            rawtx: None,
            sequence: None,
        };

        let publisher = ZmqPublisher::new(&config).unwrap();

        // Create subscriber
        let ctx = Context::new();
        let subscriber = ctx.socket(SUB).unwrap();
        subscriber.connect("tcp://127.0.0.1:28333").unwrap();
        subscriber.set_subscribe(b"hashblock").unwrap();

        // Give publisher time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Publish notification
        let block_hash: Hash = [1u8; 32];
        publisher.publish_hashblock(&block_hash).await.unwrap();

        // Receive notification
        let result = timeout(Duration::from_secs(1), async {
            let topic = subscriber.recv_msg(0).unwrap();
            let data = subscriber.recv_msg(0).unwrap();
            (topic, data)
        })
        .await;

        assert!(result.is_ok());
        let (topic, data) = result.unwrap();
        assert_eq!(topic.as_str(), Some("hashblock"));
        assert_eq!(data.len(), 32);
        assert_eq!(data.as_ref() as &[u8], block_hash.as_slice());
    }

    #[tokio::test]
    async fn test_zmq_hashtx_notification() {
        let config = ZmqConfig {
            hashblock: None,
            hashtx: Some("tcp://127.0.0.1:28334".to_string()),
            rawblock: None,
            rawtx: None,
            sequence: None,
        };

        let publisher = ZmqPublisher::new(&config).unwrap();

        // Create subscriber
        let ctx = Context::new();
        let subscriber = ctx.socket(SUB).unwrap();
        subscriber.connect("tcp://127.0.0.1:28334").unwrap();
        subscriber.set_subscribe(b"hashtx").unwrap();

        // Give publisher time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Publish notification
        let tx_hash: Hash = [2u8; 32];
        publisher.publish_hashtx(&tx_hash).await.unwrap();

        // Receive notification
        let result = timeout(Duration::from_secs(1), async {
            let topic = subscriber.recv_msg(0).unwrap();
            let data = subscriber.recv_msg(0).unwrap();
            (topic, data)
        })
        .await;

        assert!(result.is_ok());
        let (topic, data) = result.unwrap();
        assert_eq!(topic.as_str(), Some("hashtx"));
        assert_eq!(data.len(), 32);
        assert_eq!(data.as_ref() as &[u8], tx_hash.as_slice());
    }

    #[tokio::test]
    async fn test_zmq_rawblock_notification() {
        let config = ZmqConfig {
            hashblock: None,
            hashtx: None,
            rawblock: Some("tcp://127.0.0.1:28335".to_string()),
            rawtx: None,
            sequence: None,
        };

        let publisher = ZmqPublisher::new(&config).unwrap();
        let block = create_test_block();

        // Create subscriber
        let ctx = Context::new();
        let subscriber = ctx.socket(SUB).unwrap();
        subscriber.connect("tcp://127.0.0.1:28335").unwrap();
        subscriber.set_subscribe(b"rawblock").unwrap();

        // Give publisher time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Publish notification
        publisher.publish_rawblock(&block).await.unwrap();

        // Receive notification
        let result = timeout(Duration::from_secs(1), async {
            let topic = subscriber.recv_msg(0).unwrap();
            let data = subscriber.recv_msg(0).unwrap();
            (topic, data)
        })
        .await;

        assert!(result.is_ok());
        let (topic, _data) = result.unwrap();
        assert_eq!(topic.as_str(), Some("rawblock"));
        // Data should contain serialized block
        assert!(_data.len() > 0);
    }

    #[tokio::test]
    async fn test_zmq_rawtx_notification() {
        let config = ZmqConfig {
            hashblock: None,
            hashtx: None,
            rawblock: None,
            rawtx: Some("tcp://127.0.0.1:28336".to_string()),
            sequence: None,
        };

        let publisher = ZmqPublisher::new(&config).unwrap();
        let tx = create_test_transaction();

        // Create subscriber
        let ctx = Context::new();
        let subscriber = ctx.socket(SUB).unwrap();
        subscriber.connect("tcp://127.0.0.1:28336").unwrap();
        subscriber.set_subscribe(b"rawtx").unwrap();

        // Give publisher time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Publish notification
        publisher.publish_rawtx(&tx).await.unwrap();

        // Receive notification
        let result = timeout(Duration::from_secs(1), async {
            let topic = subscriber.recv_msg(0).unwrap();
            let data = subscriber.recv_msg(0).unwrap();
            (topic, data)
        })
        .await;

        assert!(result.is_ok());
        let (topic, _data) = result.unwrap();
        assert_eq!(topic.as_str(), Some("rawtx"));
        // Data should contain serialized transaction
        assert!(_data.len() > 0);
    }

    #[tokio::test]
    async fn test_zmq_sequence_notification() {
        let config = ZmqConfig {
            hashblock: None,
            hashtx: None,
            rawblock: None,
            rawtx: None,
            sequence: Some("tcp://127.0.0.1:28337".to_string()),
        };

        let publisher = ZmqPublisher::new(&config).unwrap();
        let tx_hash: Hash = [3u8; 32];

        // Create subscriber
        let ctx = Context::new();
        let subscriber = ctx.socket(SUB).unwrap();
        subscriber.connect("tcp://127.0.0.1:28337").unwrap();
        subscriber.set_subscribe(b"sequence").unwrap();

        // Give publisher time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Publish notification (mempool entry)
        publisher.publish_sequence(&tx_hash, true).await.unwrap();

        // Receive notification
        let result = timeout(Duration::from_secs(1), async {
            let topic = subscriber.recv_msg(0).unwrap();
            let data = subscriber.recv_msg(0).unwrap();
            (topic, data)
        })
        .await;

        assert!(result.is_ok());
        let (topic, data) = result.unwrap();
        assert_eq!(topic.as_str(), Some("sequence"));
        assert_eq!(data.len(), 33); // 1 byte type + 32 bytes hash
        let data_bytes: &[u8] = data.as_ref();
        assert_eq!(data_bytes[0], 0x01); // Mempool entry
        assert_eq!(&data_bytes[1..], tx_hash.as_slice());
    }

    #[tokio::test]
    async fn test_zmq_config_is_enabled() {
        let config = ZmqConfig {
            hashblock: Some("tcp://127.0.0.1:28332".to_string()),
            hashtx: None,
            rawblock: None,
            rawtx: None,
            sequence: None,
        };
        assert!(config.is_enabled());

        let config = ZmqConfig::default();
        assert!(!config.is_enabled());
    }

    #[tokio::test]
    async fn test_zmq_publish_block_convenience() {
        let config = ZmqConfig {
            hashblock: Some("tcp://127.0.0.1:28338".to_string()),
            hashtx: None,
            rawblock: Some("tcp://127.0.0.1:28339".to_string()),
            rawtx: None,
            sequence: None,
        };

        let publisher = ZmqPublisher::new(&config).unwrap();
        let block = create_test_block();
        let block_hash: Hash = [4u8; 32];

        // Create subscribers
        let ctx = Context::new();
        let hash_sub = ctx.socket(SUB).unwrap();
        hash_sub.connect("tcp://127.0.0.1:28338").unwrap();
        hash_sub.set_subscribe(b"hashblock").unwrap();

        let raw_sub = ctx.socket(SUB).unwrap();
        raw_sub.connect("tcp://127.0.0.1:28339").unwrap();
        raw_sub.set_subscribe(b"rawblock").unwrap();

        // Give publisher time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Publish block (should publish both hashblock and rawblock)
        publisher.publish_block(&block, &block_hash).await.unwrap();

        // Receive both notifications
        let hash_result = timeout(Duration::from_secs(1), async {
            hash_sub.recv_msg(0).unwrap();
            hash_sub.recv_msg(0).unwrap();
        })
        .await;

        let raw_result = timeout(Duration::from_secs(1), async {
            raw_sub.recv_msg(0).unwrap();
            raw_sub.recv_msg(0).unwrap();
        })
        .await;

        assert!(hash_result.is_ok());
        assert!(raw_result.is_ok());
    }
}
