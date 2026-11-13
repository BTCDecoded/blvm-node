# Changelog

All notable changes to `blvm-node` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Every section below is reconstructed from git history. Conventional Commits
were not enforced when those commits were written. Merge commits are omitted.

## [Unreleased]

## [v0.1.1] - 2026-05-06

### Reconstructed

- Complete reference-node implementation ([f081ce506542](https://github.com/BTCDecoded/blvm-node/commit/f081ce506542c43123c841cef2d02d71882fdfab))
- Complete reference-node implementation ([75067c0bd31a](https://github.com/BTCDecoded/blvm-node/commit/75067c0bd31a0f209d6bc0e72adc13151244ea16))
- Update reference node implementation and documentation ([43f82f5cda5f](https://github.com/BTCDecoded/blvm-node/commit/43f82f5cda5f7813ee8388c185c3d0c10ad6d407))
- docs: Organize documentation into structured directories ([66c7ecbd2421](https://github.com/BTCDecoded/blvm-node/commit/66c7ecbd2421b5778242191c6e3d0db26d5d2e68))
- chore: Update codebase with various improvements ([00d2edacf925](https://github.com/BTCDecoded/blvm-node/commit/00d2edacf9250a001eb6e433d9e0800ae8c38610))
- Add benchmark comparison infrastructure and production features benchmarks ([c6e6adc31561](https://github.com/BTCDecoded/blvm-node/commit/c6e6adc31561de847ed68ffa9ddcf89412555722))
- Fix YAML syntax errors in CI workflow ([0a403af4e102](https://github.com/BTCDecoded/blvm-node/commit/0a403af4e1026762bb170bc4ab0e3bfe2c310e08))
- Simplify CI to Linux-only self-hosted runners ([add1b2755630](https://github.com/BTCDecoded/blvm-node/commit/add1b27556309dcf08201b165a1da6320fb060bb))
- Fix runner label case sensitivity ([6c523bc6acc7](https://github.com/BTCDecoded/blvm-node/commit/6c523bc6acc76ebc86a2533b266cb66ad3f7fdb9))
- Update CI workflow to use array format for runner labels ([7add92ec785c](https://github.com/BTCDecoded/blvm-node/commit/7add92ec785cb51abb6b1efdae4f1c58901aeb26))
- Update Rust toolchain to 1.82.0 ([4851352e544b](https://github.com/BTCDecoded/blvm-node/commit/4851352e544b52d8e807cb989f59949014903c9f))
- chore: trigger CI workflow ([1f9ac5b64ff4](https://github.com/BTCDecoded/blvm-node/commit/1f9ac5b64ff47cebbda8af36a368763db25d1714))
- refactor: remove direct consensus-proof dependency ([0a303f5d7cc8](https://github.com/BTCDecoded/blvm-node/commit/0a303f5d7cc8eee2fd3967194c7142c56c032682))
- Fix GitHub Actions path issue: move dependencies to parent directory after checkout ([f91bd40ea59e](https://github.com/BTCDecoded/blvm-node/commit/f91bd40ea59e9910346b39a008ea36c76f47cba9))
- Fix move step: handle existing directories in parent ([53326643b5a1](https://github.com/BTCDecoded/blvm-node/commit/53326643b5a1b8a9eb981a374a50d12784c6961c))
- feat(mining): add comprehensive test suite for mining implementation ([b1c1233dc57e](https://github.com/BTCDecoded/blvm-node/commit/b1c1233dc57e5b1364bbddf056e75ed67d6461cc))
- feat(mining): complete mining integration with consensus-proof functions ([c98a9bce1c92](https://github.com/BTCDecoded/blvm-node/commit/c98a9bce1c92bde3a204d14f7639c98e87d3414d))
- feat: Integrate BLLVM optimizations for Stratum V2 and mining ([20ca0e4fa17e](https://github.com/BTCDecoded/blvm-node/commit/20ca0e4fa17e1a35110a22db28d1b0d11da9840e))
- Fix: Update Rust toolchain to 1.88.0 and fix cargo-audit/tarpaulin ([ca7a960a3baa](https://github.com/BTCDecoded/blvm-node/commit/ca7a960a3baa73093aa1c7386778499198fd0c26))
- Fix: Update rust-toolchain.toml to 1.88.0 ([e46d5152d3f7](https://github.com/BTCDecoded/blvm-node/commit/e46d5152d3f72b3580b1d64c742b42d772a95b57))
- Fix: Update format! strings and remove unused imports ([e4ea664c3392](https://github.com/BTCDecoded/blvm-node/commit/e4ea664c33926f92683200cb21f37d2fd03d3fad))
- Fix: Critical clippy errors (deprecated function, unreachable code, etc.) ([429188c729f6](https://github.com/BTCDecoded/blvm-node/commit/429188c729f6d6160bc7ac727bc259cf1077e7d6))
- Fix: Use static cache keys for dependency caches (hashFiles can't access files outside workspace) ([0ce2ea4de382](https://github.com/BTCDecoded/blvm-node/commit/0ce2ea4de382e291b79a7da7fc8629f967bf058d))
- Fix: Update cargo-audit to 0.21.0 to support Cargo.lock format version 4 ([bde26803efe7](https://github.com/BTCDecoded/blvm-node/commit/bde26803efe7443516366bb55c1ffc317367f5a2))
- Fix: Remove unreachable Ok(()) in monitor_module loop ([7e7f2d07cde2](https://github.com/BTCDecoded/blvm-node/commit/7e7f2d07cde2d70b6a2ff52acbbb404657acf000))
- Fix: Use cargo-audit 0.20.0 (0.21.0 requires Rust 1.89, we use 1.88.0) ([aea56522b158](https://github.com/BTCDecoded/blvm-node/commit/aea56522b1580f1ed86f9eca0bde7d2337ec5d9b))
- fix: use protocol-engine features and fix imports ([5cafd024b177](https://github.com/BTCDecoded/blvm-node/commit/5cafd024b177bd7d3b7568e2ae273ad0a3262124))
- feat(rpc): Replace manual HTTP parsing with hyper server ([838228f53049](https://github.com/BTCDecoded/blvm-node/commit/838228f53049c62806a869ec7d62381a379627d7))
- perf(rpc): Cache RPC handler instances to reduce allocations ([429e2851ef48](https://github.com/BTCDecoded/blvm-node/commit/429e2851ef48f3c87eb01c992c526a29f9f13be8))
- Update repository references to new names (bllvm-*) ([bd1961a1c696](https://github.com/BTCDecoded/blvm-node/commit/bd1961a1c696d61f467d2196bd9e7fffd1d48daf))
- docs: Add reference to master status document ([1cbd19360556](https://github.com/BTCDecoded/blvm-node/commit/1cbd19360556883bb3537b6e8f92fd4272bbe797))
- docs: Update bllvm-sdk description to 'developer toolkit' ([cbf2181a09d9](https://github.com/BTCDecoded/blvm-node/commit/cbf2181a09d964823be78dbd2352c836856eaed8))
- feat: Complete pruning system, RPC parity, and core improvements ([1e92841ed3d7](https://github.com/BTCDecoded/blvm-node/commit/1e92841ed3d764ff00abc802147f9dbc1e1f2de9))
- Implement Phase 3 mainnet hardening and critical improvements ([0b77cb7d292b](https://github.com/BTCDecoded/blvm-node/commit/0b77cb7d292b391735a9780c11a01c2187eb7e0d))
- Fix compilation errors after separation of concerns refactoring ([2d3437a870c4](https://github.com/BTCDecoded/blvm-node/commit/2d3437a870c4fb5727bbd5b225865bb698b224b5))
- feat: Implement address relay and peer discovery ([58226fb7c0aa](https://github.com/BTCDecoded/blvm-node/commit/58226fb7c0aa76da88fbad42bfc148a55259e529))
- feat: Integrate Iroh peer discovery with address database ([3b893fdc7d2b](https://github.com/BTCDecoded/blvm-node/commit/3b893fdc7d2b4c84bce787214060b199f7ac4aa1))
- test: Add Iroh integration tests and integration status doc ([671475f52087](https://github.com/BTCDecoded/blvm-node/commit/671475f520878bde66ecc8b41547b08934919a29))
- feat: Automatic peer connection initialization on startup ([239554cb99f4](https://github.com/BTCDecoded/blvm-node/commit/239554cb99f49ab7da8559dd9143bfc1806b7e74))
- chore: Add trailing newlines to test files ([09e90dad35c2](https://github.com/BTCDecoded/blvm-node/commit/09e90dad35c24830e749d6227ae06983a1ef1ace))
- fix: Update workflow to use correct repository names (bllvm-protocol, bllvm-consensus) ([d94bb9ff1208](https://github.com/BTCDecoded/blvm-node/commit/d94bb9ff1208d8aca9cb1387e049d947d9e40327))
- fix: Add bllvm-consensus checkout to all jobs that need it ([28f69a7eb218](https://github.com/BTCDecoded/blvm-node/commit/28f69a7eb218715a95635b47ee4af3853ae6d8f1))
- Fix compilation errors: resolve 45+ errors in bllvm-node ([8cad52c0cab0](https://github.com/BTCDecoded/blvm-node/commit/8cad52c0cab01ab249933984950464820a66bddf))
- Add missing debug macro import to fix compilation errors ([49940d271c83](https://github.com/BTCDecoded/blvm-node/commit/49940d271c83a864f967afcc38702231de1e723f))
- Fix remaining compilation errors: borrow checker, integer literals, and missing imports ([31e669cd2474](https://github.com/BTCDecoded/blvm-node/commit/31e669cd247411a8db5f92e92ed35e5c0ca52600))
- Fix all remaining compilation errors - build now succeeds ([a76c8ed46779](https://github.com/BTCDecoded/blvm-node/commit/a76c8ed467797ab55da6df2cc5e7a4abaf69f1e0))
- Fix Send trait errors in async code - all compilation errors resolved ([f8d442d66b06](https://github.com/BTCDecoded/blvm-node/commit/f8d442d66b0656997d962094534459e57db19404))
- Fix async Send errors - all compilation errors now resolved ([f70050fdc3d4](https://github.com/BTCDecoded/blvm-node/commit/f70050fdc3d44dda0f71f1d355aeee396df40c67))
- Fix all remaining Send trait errors - build now succeeds ([702186ac290d](https://github.com/BTCDecoded/blvm-node/commit/702186ac290d2f6beac8952fe5778e99f1bc3e75))
- Fix await in sync closure - all compilation errors resolved ([dfb7cbf6e5e0](https://github.com/BTCDecoded/blvm-node/commit/dfb7cbf6e5e04ada1498448482f9ff7a32942d4b))
- Fix final Send trait error in server.rs - all compilation errors resolved ([cc8650abc2b2](https://github.com/BTCDecoded/blvm-node/commit/cc8650abc2b214bb0b166ed437fe19fc070402ec))
- Fix MutexGuard held across await in send_to_peer_by_transport - all errors resolved ([4f394794a6a7](https://github.com/BTCDecoded/blvm-node/commit/4f394794a6a71d1e7b549e82d06dfc7bee2adace))
- Fix remaining compilation errors: TcpConnection fields and U256 type ([4c557bab4f29](https://github.com/BTCDecoded/blvm-node/commit/4c557bab4f29e71c750975281b16bd6785b692f3))
- Fix syntax error in blockchain.rs - all compilation errors resolved ([f550f27212af](https://github.com/BTCDecoded/blvm-node/commit/f550f27212afe73456c795fa57f641b647041f37))
- cd /home/user/src/BTCDecoded/bllvm-node && git add src/network/address_db.rs && git rebase --continue fix(address_db): fix code duplication, IPv6 detection, eviction bugs, and optimize sorting ([78d5b5113b24](https://github.com/BTCDecoded/blvm-node/commit/78d5b5113b24aa21dc9af37360ae74fa165eccc3))
- fix: remove trailing whitespace in storage/mod.rs ([9e521e5ac0f5](https://github.com/BTCDecoded/blvm-node/commit/9e521e5ac0f597afef0aadac2f08fb193c103852))
- fix(network): use correct IPv6 methods for local address detection ([9aa528117da3](https://github.com/BTCDecoded/blvm-node/commit/9aa528117da39ecb1b1be9a0c64d6aa213182076))
- fix(tests): fix async await and TransportAddr type issues in tests ([9de1b479fadf](https://github.com/BTCDecoded/blvm-node/commit/9de1b479fadf26befdb0e0fbbbebd9f24e444835))
- fix(tests): fix remaining test errors - RpcServer instance methods, Arc mutability, and peer_manager access ([6e4a83b22698](https://github.com/BTCDecoded/blvm-node/commit/6e4a83b226986ceea7c7d6e893b3c677b8ae9aff))
- fix: add bip158 feature flag and format code ([e406a520a9b4](https://github.com/BTCDecoded/blvm-node/commit/e406a520a9b4186fe819c025b8755de98cd5bf81))
- fix(tests): fix test_active_connection_limit logic and test_network_manager_send_to_peer expectations ([ac9979abe69a](https://github.com/BTCDecoded/blvm-node/commit/ac9979abe69a672aa9bc7894fffae9559b8c9f6c))
- fix: add missing std::net::IpAddr qualifier ([d86eea578474](https://github.com/BTCDecoded/blvm-node/commit/d86eea57847490f10296ac5334b7327a45bbfc5d))
- fix: replace deprecated calculate_tx_id, add sigop feature, format code ([bbb2a30df544](https://github.com/BTCDecoded/blvm-node/commit/bbb2a30df544148aa6dcbe9ab4d905028efcaf18))
- fix: fix all clippy warnings - unused variables, unreachable patterns, unused imports ([4e235668fff5](https://github.com/BTCDecoded/blvm-node/commit/4e235668fff5725f806f2c17388dbac24cb78fe9))
- fix: fix remaining unused variables ([c74b96714d32](https://github.com/BTCDecoded/blvm-node/commit/c74b96714d3247e5af3feca3b955e612bca45c6b))
- fix: fix all remaining verbose unused variables ([884c0f26e73e](https://github.com/BTCDecoded/blvm-node/commit/884c0f26e73e28ac04302ba6be8bcfeab7abb307))
- Fix compilation errors: align quinn versions and update API for quinn 0.10 ([c97395c6cdf0](https://github.com/BTCDecoded/blvm-node/commit/c97395c6cdf008590b8614bfc2f0f014b742aab4))
- Fix all compilation errors: BIP70 imports, node tests, property tests ([4917851339de](https://github.com/BTCDecoded/blvm-node/commit/4917851339dec69e83dcf778294faeffee3bca87))
- Fix CI issues: unused variables, stratum-v2 test module structure, rustfmt ([a08f0ba60f40](https://github.com/BTCDecoded/blvm-node/commit/a08f0ba60f40d2ffaeacff4e9bdafd839cdc0f97))
- Fix rustfmt: move common module declaration outside cfg block ([86bfb11e6de2](https://github.com/BTCDecoded/blvm-node/commit/86bfb11e6de24a0092bde93fb0defa32f816782d))
- Fix failing tests: use valid Ed25519 keys, remove block_on, handle RPC errors gracefully ([0941724e34c7](https://github.com/BTCDecoded/blvm-node/commit/0941724e34c798aedc9d05d4c775df792c8daf23))
- Fix clippy warnings (use {e} format) and test_timestamp_cleanup (wait 1 second) ([eeb90bc6b05b](https://github.com/BTCDecoded/blvm-node/commit/eeb90bc6b05b5ac0209dd261879e06dc141754cc))
- Fix clippy warnings: use {e} format strings, fix unnecessary casts, fix dandelion test ([ff058b9fd346](https://github.com/BTCDecoded/blvm-node/commit/ff058b9fd34658d35cc335e4cd831f1b8cf0cefd))
- Fix dandelion test (enable dandelion), fix comparison errors (remove >= 0 on u64) ([80c3235cc682](https://github.com/BTCDecoded/blvm-node/commit/80c3235cc682dabde9554cf0c4372926c6822a60))
- Fix dandelion test: use with_policies to enable dandelion ([da8549f7edb8](https://github.com/BTCDecoded/blvm-node/commit/da8549f7edb8c0a97fbbc7a7bee650799836dc9e))
- Fix dandelion test: use max_stem_hops=0 to fluff on second call ([77a781fd2cb0](https://github.com/BTCDecoded/blvm-node/commit/77a781fd2cb0db7d54ed70e2f4aaa27c3205f354))
- perf(rpc): Optimize RPC performance with safe caching ([8e7a0fcf7be4](https://github.com/BTCDecoded/blvm-node/commit/8e7a0fcf7be4c9a5784845e875ef636d5024fd06))
- perf(rpc): Additional RPC optimizations for competitive performance ([41f25901dff7](https://github.com/BTCDecoded/blvm-node/commit/41f25901dff763bd8820b9f60126b1f017937345))
- feat: Add formal verification for Bitcoin network protocol ([d767623b308d](https://github.com/BTCDecoded/blvm-node/commit/d767623b308d8da6531ace11267c327867a9cc3b))
- Add comprehensive formal verification: 66 new Kani proofs ([a9f66364cd17](https://github.com/BTCDecoded/blvm-node/commit/a9f66364cd17f85a6204062ed94bb107e61bfd4d))
- Optimize RPC performance: eliminate double JSON serialization, use direct Value constructors, optimize request parsing ([d7ed8338f0b9](https://github.com/BTCDecoded/blvm-node/commit/d7ed8338f0b9c50f91702a66e936c9ec9bc01475))
- Additional low-risk RPC optimizations: params cloning, storage lookup batching, hex caching ([50fc77bd22d1](https://github.com/BTCDecoded/blvm-node/commit/50fc77bd22d19c24735765aaf3e050c475423773))
- Fix: remove non-existent compute_merkle_root_from_hashes and cached_hash field ([4ee0e8fb0d6b](https://github.com/BTCDecoded/blvm-node/commit/4ee0e8fb0d6b7d5ad7a2e476c0a3af53a2696d4d))
- Fix clippy: change target >= u64::MAX to target == u64::MAX ([d68a6660d8af](https://github.com/BTCDecoded/blvm-node/commit/d68a6660d8af7a4fc133d205cc3926345bd13ba0))
- Additional RPC optimizations: getrawmempool, get_chain_tx_stats, get_block_header ([8bb06f935a3b](https://github.com/BTCDecoded/blvm-node/commit/8bb06f935a3bd02813c97b5831f20dbcd1313735))
- Additional safe RPC optimizations: static constants, hex helper, Vec pre-allocation, caching ([0349eca84c88](https://github.com/BTCDecoded/blvm-node/commit/0349eca84c88299ae8dd45a25ae14d5eb3ce0243))
- Merge: resolve conflicts in network module ([842f70f77f0d](https://github.com/BTCDecoded/blvm-node/commit/842f70f77f0d1029f8e34fcca830d9e04cb37082))
- Fix compilation errors: API changes, borrow checker, Send trait issues ([a9e137d6c3ce](https://github.com/BTCDecoded/blvm-node/commit/a9e137d6c3ce2bce145908207468416000f4f62a))
- Fix clippy warnings ([d33c95e6c720](https://github.com/BTCDecoded/blvm-node/commit/d33c95e6c720ac15d8cf20630f5e57f8626c20ac))
- Fix compilation errors from workflow logs ([5b7c806c8fa5](https://github.com/BTCDecoded/blvm-node/commit/5b7c806c8fa504e7b632e62352ea68f77007e473))
- Fix duplicate Default implementation for ParallelBlockValidator ([089b4b33a66c](https://github.com/BTCDecoded/blvm-node/commit/089b4b33a66c93791ec70e6af8083383afe4fe08))
- Upgrade cargo-audit to 0.22.0 for Cargo.lock format version 4 support ([2852e3a40d77](https://github.com/BTCDecoded/blvm-node/commit/2852e3a40d77cf83aabb7c4fe4f6e06cd5919f5f))
- Fix RPC server tests: parse String response to Value ([20669d99def6](https://github.com/BTCDecoded/blvm-node/commit/20669d99def6c87fbd04aaeca272f10a687b1fd7))
- Add --locked flag to cargo-audit installation to use exact dependency versions ([d34bd247a529](https://github.com/BTCDecoded/blvm-node/commit/d34bd247a529e4026aff562b080c355fe6806b9f))
- Upgrade Rust to 1.89.0 for Security Audit job (required by cargo-audit 0.22.0) ([44dd4d641774](https://github.com/BTCDecoded/blvm-node/commit/44dd4d6417748ad58891944e0edd70d18b258e64))
- Migrate from iroh-net 0.12 to iroh 0.95.1 ([02822ad422f8](https://github.com/BTCDecoded/blvm-node/commit/02822ad422f8844b9348494bcc94b021a023645c))
- Fix iroh 0.95 API changes: SecretKey::generate() and borrow checker issues ([47c39287251c](https://github.com/BTCDecoded/blvm-node/commit/47c39287251cadee9d9e4e6f5a6243cc9fa1b4d4))
- Fix remaining iroh migration issues: use OsRng for SecretKey::generate and fix quinn_addr borrow error ([c3a08fafa54a](https://github.com/BTCDecoded/blvm-node/commit/c3a08fafa54acf714bbdcd94295d878b3eefc290))
- Fix iroh SecretKey generation: use getrandom instead of RNG to avoid version conflicts ([2f6b12053051](https://github.com/BTCDecoded/blvm-node/commit/2f6b12053051449c21527416025797cd7e134753))
- Fix rpc_routing_tests: parse JSON string response before indexing ([edd6549258e1](https://github.com/BTCDecoded/blvm-node/commit/edd6549258e1827a01b3709498b5e8a2250b488a))
- Fix feature_flags_reflection_tests: parse JSON string response before indexing ([c3b3237da928](https://github.com/BTCDecoded/blvm-node/commit/c3b3237da928bab76547b4bcf1ae5523e3d410cd))
- Fix hanging test: change async test to sync test ([39bcce8273d2](https://github.com/BTCDecoded/blvm-node/commit/39bcce8273d25a74f9311949bf96282fbd39a204))
- Fix hanging test: remove nested runtime in async test ([7cd5920448be](https://github.com/BTCDecoded/blvm-node/commit/7cd5920448be94fd7401bad079a53f6cf87057fc))
- Fix hanging test: add timeout and use dynamic port ([9e156f54ed96](https://github.com/BTCDecoded/blvm-node/commit/9e156f54ed9617271c83f260a466ebb477066dc3))
- Temporarily ignore hanging test until mutex issue is fixed ([5ff6bb3d5072](https://github.com/BTCDecoded/blvm-node/commit/5ff6bb3d5072edf17d6bd389f910bf6ff6371ce5))
- Remove hanging test that blocks test suite ([100e5e8f5f9f](https://github.com/BTCDecoded/blvm-node/commit/100e5e8f5f9f8f55154e17310ecef3963ebbff6c))
- Fix send_to_peer to return Ok for non-existent peers ([c06e84e61dc9](https://github.com/BTCDecoded/blvm-node/commit/c06e84e61dc9f01ec8be2f9eebaee08d1318ca83))
- Add configurable network timing, request timeouts, and module resource limits ([a36cf322960c](https://github.com/BTCDecoded/blvm-node/commit/a36cf322960cbfa8a1327af3dc70ffbe66f37e08))
- Add user-operated node signaling infrastructure ([ac7c63b4bacb](https://github.com/BTCDecoded/blvm-node/commit/ac7c63b4bacb47fc6654b17771bb0f276b8bf393))
- Add governance module with user signaling ([8c68c607954f](https://github.com/BTCDecoded/blvm-node/commit/8c68c607954fe0db3060b74da3c730d2dde60e57))
- fix: resolve critical MutexGuard-across-await issues and convert to tokio::sync::Mutex ([7deb22ddc5c6](https://github.com/BTCDecoded/blvm-node/commit/7deb22ddc5c6ebb267be6217af4b31a260b77292))
- feat: implement critical security and code quality improvements ([b1df335bf078](https://github.com/BTCDecoded/blvm-node/commit/b1df335bf07801796d436803190a23ff36ad5133))
- feat: Concurrency improvements and bllvm-consensus integration ([e8990a72c065](https://github.com/BTCDecoded/blvm-node/commit/e8990a72c0652b5bf9358c9c99525adce0672657))
- fix: Resolve remaining compilation errors ([ac2620c0762a](https://github.com/BTCDecoded/blvm-node/commit/ac2620c0762a3e83ec5da54b917b58e27e19eaf2))
- fix: Resolve hyper::Error conversion issue ([609d1705bc28](https://github.com/BTCDecoded/blvm-node/commit/609d1705bc28bfbd83550bd7fec81c6b22f6b8a1))
- fix: Use expect() for Response::builder() error handling ([aef7155d6480](https://github.com/BTCDecoded/blvm-node/commit/aef7155d6480e1f3eb9d5605bb2ad0f4fc33d512))
- feat: Implement better indexing and update Kani proof count ([9b23aa398b21](https://github.com/BTCDecoded/blvm-node/commit/9b23aa398b218bbfc1ef4be15515dae949254be3))
- refactor: Remove bench definitions - moved to bllvm-bench ([9113445281ee](https://github.com/BTCDecoded/blvm-node/commit/9113445281ee19f6cd96895d13ebddc8e2b5c189))
- Fix compilation errors: Update for production feature type changes ([60879dbdab41](https://github.com/BTCDecoded/blvm-node/commit/60879dbdab416793fa3229c70c8f91c29ec4541c))
- fix: add ctv feature pass-through from bllvm-protocol ([f7f34e4a8ef1](https://github.com/BTCDecoded/blvm-node/commit/f7f34e4a8ef125d878af15660ecffc7278a3fcc3))
- fix: use write() instead of lock() for RwLock in iroh address handling ([587ea5ce66c7](https://github.com/BTCDecoded/blvm-node/commit/587ea5ce66c71de92bc26115f839c5fc28e2da31))
- feat(ha): Add Phase 2 & 3 high availability features ([fb412ae9af61](https://github.com/BTCDecoded/blvm-node/commit/fb412ae9af61bec6b586d71ce016016648404451))
- Fix network manager tests for multi-threaded runtime ([97c9ea462a26](https://github.com/BTCDecoded/blvm-node/commit/97c9ea462a26218cc322a0fa68d5adab2b328a73))
- Fix all compilation warnings and complete TODOs ([214d2c41605a](https://github.com/BTCDecoded/blvm-node/commit/214d2c41605a0675b746fd73ace6082ad02c089b))
- fix: add missing tracing::info import in relay.rs ([c79786191de7](https://github.com/BTCDecoded/blvm-node/commit/c79786191de762e5bf6d50b822092fa2e298ea39))
- Implement TODOs: auto-ban periodic check, prlimit support, block header extraction, secp256k1 signing, spam summary conversion ([6da570fc001c](https://github.com/BTCDecoded/blvm-node/commit/6da570fc001ca54c542f88772eb37049f5b9f982))
- Organize documentation: update README structure ([48172fe8c24b](https://github.com/BTCDecoded/blvm-node/commit/48172fe8c24b7cbba70e2d5d9d434d2efd35c70c))
- feat(governance): Add governance webhook client for block notifications ([99bcaf26b233](https://github.com/BTCDecoded/blvm-node/commit/99bcaf26b233820f7754afa6dd2b0294adf23f2c))
- Fix test isolation issues for Redb database ([f5f484eedc0b](https://github.com/BTCDecoded/blvm-node/commit/f5f484eedc0b94f1fb671c918697e55e59e456e5))
- Fix remaining test failures in bllvm-node ([11e705c56bf8](https://github.com/BTCDecoded/blvm-node/commit/11e705c56bf8a4e023402be0c58a72a5a08786f6))
- Fix mempool test to match actual implementation behavior ([920fcc83df3d](https://github.com/BTCDecoded/blvm-node/commit/920fcc83df3dbb455a8fb7b9dad1dad984b89a5d))
- Fix overflow bug in calculate_work_from_bits ([29829b321b4b](https://github.com/BTCDecoded/blvm-node/commit/29829b321b4bf37561dd8cb833e1bce456431250))
- Fix all remaining test failures in bllvm-node ([aac168fe8ef2](https://github.com/BTCDecoded/blvm-node/commit/aac168fe8ef2013fb2e5aa01157db1faee6d7349))
- Fix rpc_tests: Initialize chain state for getblocktemplate test ([e469bb812396](https://github.com/BTCDecoded/blvm-node/commit/e469bb8123967d6b7b14e73b1e67395be762f60b))
- Fix security_tests: Use multi-threaded tokio runtime for blocking operations ([1aa9bfe0a080](https://github.com/BTCDecoded/blvm-node/commit/1aa9bfe0a0809b96bfddc35ad02efa4deb975e3d))
- Fix storage_tests: Use multi-threaded tokio runtime for all tests ([6aaa06d9dac2](https://github.com/BTCDecoded/blvm-node/commit/6aaa06d9dac2b754eab8983f91c036f5c324e00e))
- Fix test failures: RedbTree iteration, TxIndex hash calculation, Node storage Arc handling ([6cd01228e85e](https://github.com/BTCDecoded/blvm-node/commit/6cd01228e85e58d067231bb3a5c047f7c9f5f48a))
- fix: make IPC module Unix-only to support Windows cross-compilation ([c30f1634825b](https://github.com/BTCDecoded/blvm-node/commit/c30f1634825b5c7fcb1ca2fc73cc8892938e7ad6))
- fix: remove non-existent governance config field access ([1e57b2e05364](https://github.com/BTCDecoded/blvm-node/commit/1e57b2e053644197e489366e85370d9adf28eda4))
- fix: make IPC protocol available on all platforms, conditionally compile client usage ([d8bcd05cefbf](https://github.com/BTCDecoded/blvm-node/commit/d8bcd05cefbf0e85258bd40fde719ab2fc6364a3))
- fix: disable mimalloc for Windows cross-compilation ([6a401b96e063](https://github.com/BTCDecoded/blvm-node/commit/6a401b96e0632281012099201106efae8b86ded8))
- fix: make all optional dependencies conditional for Windows cross-compilation ([ef44ff5b081a](https://github.com/BTCDecoded/blvm-node/commit/ef44ff5b081a9b0b7ae3fd86cf1360b9407b26f4))
- Update .gitignore: remove fuzz/target/ from tracking ([1fe77b7a4613](https://github.com/BTCDecoded/blvm-node/commit/1fe77b7a461377c441f0c288069545564edcef80))
- Archive historical documentation to bllvm-docs ([04e567e84dc9](https://github.com/BTCDecoded/blvm-node/commit/04e567e84dc95f99432b2f7bfcca9125f0042b42))
- fix: pin dependencies to exact versions for release ([fcf244c2ae27](https://github.com/BTCDecoded/blvm-node/commit/fcf244c2ae27e32a371fe307e460e1d99dc4bfde))
- Improve logging system: add utilities, config integration, and standards compliance ([3de89e412968](https://github.com/BTCDecoded/blvm-node/commit/3de89e412968bf14ead4065c1954ade480a3b9bc))
- fix: resolve merge conflict markers in config/mod.rs ([41af2d1bf525](https://github.com/BTCDecoded/blvm-node/commit/41af2d1bf5259a491afbd9bd1994705fe6271bd0))
- fix: resolve remaining merge conflict marker in NodeConfig Default ([18ec0190588e](https://github.com/BTCDecoded/blvm-node/commit/18ec0190588e5970964c041a3ea47e494c4c8d31))
- Fix compilation errors and test failures ([ea53aeba8d56](https://github.com/BTCDecoded/blvm-node/commit/ea53aeba8d565e3a8e2eea9d8020248da7d12d47))
- fix: remove underscore prefix from keep_filters parameter ([16f0792f7fde](https://github.com/BTCDecoded/blvm-node/commit/16f0792f7fdeb87a8073edbc08a6e16ad5e4f9fc))
- security: Update tracing-subscriber to 0.3.20 and quinn to 0.11 to fix vulnerabilities ([d5a6411be397](https://github.com/BTCDecoded/blvm-node/commit/d5a6411be3975a794b0c1cad9627daca8c0ba670))
- fix: Add missing info import in network/fibre.rs ([fb60115460df](https://github.com/BTCDecoded/blvm-node/commit/fb60115460df9f2e90244f3454737d892bd555c3))
- Remove verbose workflow name ([925fa54bdeda](https://github.com/BTCDecoded/blvm-node/commit/925fa54bdeda2d76f130eff5d57c9bc2af54dc3e))
- chore: Update dependencies and formatting ([510471cc2420](https://github.com/BTCDecoded/blvm-node/commit/510471cc242078c51a37a067be469994e8446861))
- Clean up verbose workflow names and echo statements ([441f4bf994cb](https://github.com/BTCDecoded/blvm-node/commit/441f4bf994cbe81a91f2165dcda935ecc568912e))
- Fix formatting: add trailing newlines ([5d2195d6dc38](https://github.com/BTCDecoded/blvm-node/commit/5d2195d6dc380f98391a13503b968a784eaede77))
- Resolve merge conflicts and fix formatting ([47818357a9c1](https://github.com/BTCDecoded/blvm-node/commit/47818357a9c1a41f876df6ce9fed583004516dbb))
- Add orchestrator trigger: trigger release orchestrator on push to main ([ec0f14c0a857](https://github.com/BTCDecoded/blvm-node/commit/ec0f14c0a857915e25eb87023743b851536d27e4))
- Standardize CI workflow: security depends on setup, build depends on all checks ([08e90b09f28d](https://github.com/BTCDecoded/blvm-node/commit/08e90b09f28dab302d08fc9d5dbea084af108bb0))
- Add independent release workflow for bllvm-node ([30ca656f9ac1](https://github.com/BTCDecoded/blvm-node/commit/30ca656f9ac14d7c825eb23055428f0a8dfa920b))
- Add release job to CI workflow ([118e5c9501f1](https://github.com/BTCDecoded/blvm-node/commit/118e5c9501f19b8267ab010c3586720b7780c7fa))
- Fix version determination to only match package version ([179bcfd76501](https://github.com/BTCDecoded/blvm-node/commit/179bcfd7650142710ec71ccd72090afeffcf934e))
- Fix version extraction: use grep instead of awk ([63529a6007bc](https://github.com/BTCDecoded/blvm-node/commit/63529a6007bc69ac87e19173e9901c13e90c87e7))
- Commit Cargo.toml changes before publish ([a0a34dacbff1](https://github.com/BTCDecoded/blvm-node/commit/a0a34dacbff14badbac760ef369e75e360a3da68))
- Fix clippy, fmt, and audit issues; add Kani verify step to CI ([07a17a83c788](https://github.com/BTCDecoded/blvm-node/commit/07a17a83c788cd937f40e4d883cf8f56813ee9d5))
- Fix cargo-audit version compatibility with Rust 1.88.0 ([d059be22da1e](https://github.com/BTCDecoded/blvm-node/commit/d059be22da1e4a76e0c642530415b2d53d130cc4))
- Optimize Kani verification and test execution ([4fc241346773](https://github.com/BTCDecoded/blvm-node/commit/4fc241346773316d97b88370f77417e9746ba558))
- Fix Kani jobs flag in bllvm-node CI ([5a89c03c3402](https://github.com/BTCDecoded/blvm-node/commit/5a89c03c340207bd313f16106210564afdd1b846))
- Change non-verification jobs to use 'builds' runners ([ade912a22caf](https://github.com/BTCDecoded/blvm-node/commit/ade912a22caf8c8ccf06e4d0cf0b5614abaf9b26))
- Fix build job to use 'builds' runner ([0a6746d53f4c](https://github.com/BTCDecoded/blvm-node/commit/0a6746d53f4cde5ab14b5f3768fbc009d8be9dec))
- Add GitHub attestations for releases ([4fc135a9159f](https://github.com/BTCDecoded/blvm-node/commit/4fc135a9159fd6ac3e7d978e799b61e877065854))
- Remove attestations from bllvm-node ([241eed99b8fc](https://github.com/BTCDecoded/blvm-node/commit/241eed99b8fc334a218a752a021dbe92eab34dd6))
- Add simple attestation step to bllvm-node release ([75bf164a3fc8](https://github.com/BTCDecoded/blvm-node/commit/75bf164a3fc8f500ec19cd236a34d200ba092e09))
- Add skip_verification workflow input ([66c745ad9f56](https://github.com/BTCDecoded/blvm-node/commit/66c745ad9f56f92897af6261f7ec1158d53ad7a8))
- Fix: Add workflow_dispatch to build and release job conditions ([45f02204e8be](https://github.com/BTCDecoded/blvm-node/commit/45f02204e8beb8f1a64b8e159baa83e85186e1b8))
- Fix: Handle 'crate already exists' error gracefully ([74cfd389db0a](https://github.com/BTCDecoded/blvm-node/commit/74cfd389db0a92ae53dfe8827f099c74ffb5d7b4))
- Add skip_tests workflow input option ([2862e49c4668](https://github.com/BTCDecoded/blvm-node/commit/2862e49c466860e0da515ce1af5a6034e94a7028))
- Simplify version tag: untested counts as unverified ([b28eb9461c3b](https://github.com/BTCDecoded/blvm-node/commit/b28eb9461c3b11e20ecf4bda94cd71df1c9b634e))
- Fix: Restructure conditional expressions to avoid shell syntax errors ([48dea04b877b](https://github.com/BTCDecoded/blvm-node/commit/48dea04b877be3a4125f5c7c09a3447db36b4c4b))
- Fix: Update Collect build metadata condition for skip_tests ([f33fd40874d6](https://github.com/BTCDecoded/blvm-node/commit/f33fd40874d6d8078eebc4e7059436bda2e650a9))
- Make verify job conditional when skip_verification is true ([3b7257166afb](https://github.com/BTCDecoded/blvm-node/commit/3b7257166afb9f04e4824afadd8d915c33b3855d))
- Make test job conditional when skip_tests is true ([9a6a2841791c](https://github.com/BTCDecoded/blvm-node/commit/9a6a2841791caffa49a6d775183b47f8f0bc2d43))
- Fix: Avoid shell syntax errors in version determination ([4d74cade701c](https://github.com/BTCDecoded/blvm-node/commit/4d74cade701cc8a4dfda30c32b506467916a97bc))
- Add [skip ci] support and fix attestation ([9e77a073a9a2](https://github.com/BTCDecoded/blvm-node/commit/9e77a073a9a24441f4a78f1a24473e213072b79d))
- Fix: Add [skip ci] and REPO_ACCESS_TOKEN to version commit ([535057e521f6](https://github.com/BTCDecoded/blvm-node/commit/535057e521f6fbd5043546839501c1a4d4611f43))
- Add ZMQ notification support and fix feature placement ([818f84277ab1](https://github.com/BTCDecoded/blvm-node/commit/818f84277ab1d7088ace0c07a0a394c8e63644eb))
- Add skip_tests to clippy, fmt, docs, security jobs and update build job ([090a5c88728b](https://github.com/BTCDecoded/blvm-node/commit/090a5c88728b6c04ac3da94e79a2f9e2be3f34e0))
- Fix release workflow: don't commit crates.io dependency changes ([42148366b5ba](https://github.com/BTCDecoded/blvm-node/commit/42148366b5ba802a3ec20e864bcface7788549d3))
- Fix YAML syntax: remove emoji causing parsing error ([84dec260d51d](https://github.com/BTCDecoded/blvm-node/commit/84dec260d51dce2695761216e048ea495917731b))
- Fix YAML: quote step name with colon ([24349a0a4a5d](https://github.com/BTCDecoded/blvm-node/commit/24349a0a4a5dc7dd1fe45476bbc8d6d6aec810bd))
- Fix CI workflow PATH issues and update dependencies ([35118c906174](https://github.com/BTCDecoded/blvm-node/commit/35118c9061741e5935eb69a002d4725b0365cf7a))
- Enforce test/verification requirements for build and release ([fd0fb24b82fe](https://github.com/BTCDecoded/blvm-node/commit/fd0fb24b82fe6dacf9ff7c0769b89d39e98c5ce1))
- Update bllvm-node to use published bllvm-protocol crate ([80c6e213549f](https://github.com/BTCDecoded/blvm-node/commit/80c6e213549f96b0c161fb65afd1455fbb3a0a67))
- Fix compilation errors and test failures ([3bc8fcf6ea12](https://github.com/BTCDecoded/blvm-node/commit/3bc8fcf6ea12ade02b8744d6d81918b9cd451efd))
- Fix test and benchmark compilation errors ([7c2f1aea8897](https://github.com/BTCDecoded/blvm-node/commit/7c2f1aea8897458c5f8744b340c108d56514df0b))
- Remove duplicate benchmarks - they belong in bllvm-bench ([212c238ea941](https://github.com/BTCDecoded/blvm-node/commit/212c238ea941c423bbfd12a5d1b4f7f2818cadac))
- Fix compilation errors in bllvm-node ([ed1397c5c574](https://github.com/BTCDecoded/blvm-node/commit/ed1397c5c57445ef8ce9c87d7c2319bcf3303790))
- Fix total_supply variable name ([533bf675975b](https://github.com/BTCDecoded/blvm-node/commit/533bf675975b2da28239b4e9cc4e822dec157e4e))
- Fix remaining compilation errors ([7d7646d08907](https://github.com/BTCDecoded/blvm-node/commit/7d7646d08907f3c277b40afc0306ec6c5163c0ea))
- Fix clippy warnings and formatting ([b5920f633389](https://github.com/BTCDecoded/blvm-node/commit/b5920f63338999b99fc27ebc2e52d95dc5ef7ff7))
- Fix attribute syntax for dead_code allow ([3efdcc8d37c1](https://github.com/BTCDecoded/blvm-node/commit/3efdcc8d37c13b7521c604e6cf3e9e93f3c69e78))
- Add crate-level allow(dead_code) attribute ([3d92e2e1e5e1](https://github.com/BTCDecoded/blvm-node/commit/3d92e2e1e5e1b7b2efc3a39240a06a58586b3ac4))
- Fix refutable pattern error and unused variable warnings ([869f01f6d706](https://github.com/BTCDecoded/blvm-node/commit/869f01f6d706a148c3c299e240086a1f6da91f29))
- Restore mut on tcp_listener - accept() requires mutable reference ([7d849f8e6a50](https://github.com/BTCDecoded/blvm-node/commit/7d849f8e6a50fc61bc4b9dbd94995e81dd4d6a6f))
- Fix verify job skip condition - remove ${{ }} wrapper ([8c42fe5451c9](https://github.com/BTCDecoded/blvm-node/commit/8c42fe5451c9b323bbed2c9f3ceec8519d0d8ce1))
- Fix all clippy errors and compilation issues ([18ab586b1882](https://github.com/BTCDecoded/blvm-node/commit/18ab586b1882b52d7c4e4034ed839cacfc2f6621))
- Fix remaining clippy errors: while let loop and filter_map ([e8752a73ca56](https://github.com/BTCDecoded/blvm-node/commit/e8752a73ca56590cb6f9f16a535406e9a9d00ec4))
- Fix final clippy errors: to_string allow, while let loop, loop never loops ([067753c79a48](https://github.com/BTCDecoded/blvm-node/commit/067753c79a481d03e3f2ab7ca38d5659fe92b8ca))
- Fix to_string allow attribute ([2471366d9484](https://github.com/BTCDecoded/blvm-node/commit/2471366d948439bb26247a00a0f37dc67dba9900))
- Fix final clippy error: use &Path instead of &PathBuf ([176479957432](https://github.com/BTCDecoded/blvm-node/commit/1764799574321dca5c3bcd663ff325054825bc9f))
- Fix compilation errors and clippy format string warnings ([63cda44868ea](https://github.com/BTCDecoded/blvm-node/commit/63cda44868ea76f49b0361a074faa136740c3622))
- Remove build-chain trigger from repository_dispatch ([b59e7561e65a](https://github.com/BTCDecoded/blvm-node/commit/b59e7561e65af000b90fd081df7328ddbb3bbb4b))
- Delete build-chain.yml workflow ([b9a5d839d8e3](https://github.com/BTCDecoded/blvm-node/commit/b9a5d839d8e3181e49694cb68963dc06163e9819))
- Fix formatting in dns_seeds.rs ([153fc8f5b23e](https://github.com/BTCDecoded/blvm-node/commit/153fc8f5b23ee87e0680889720ac7b106bcdeae2))
- Fix remaining clippy format string warnings ([5cdbae0e95dc](https://github.com/BTCDecoded/blvm-node/commit/5cdbae0e95dc1c925cb659a894353d1890cdfd1a))
- Fix all remaining clippy format string warnings ([84ac690517d8](https://github.com/BTCDecoded/blvm-node/commit/84ac690517d8da1e1373c03c3918f93114e6a907))
- Fix remaining format strings in network and tests ([ecc6bb55fb7b](https://github.com/BTCDecoded/blvm-node/commit/ecc6bb55fb7b119b90774d0a481970f76caf615f))
- Fix write! macros in transport.rs ([999623ad0128](https://github.com/BTCDecoded/blvm-node/commit/999623ad01287edb373ace83ae83e18d1efa1883))
- Fix version auto-increment to check crates.io and find available version ([b80cc6b77aa8](https://github.com/BTCDecoded/blvm-node/commit/b80cc6b77aa849cb525349873daa817f401cfd5d))
- Remove redundant release.yml workflow ([10add8166b06](https://github.com/BTCDecoded/blvm-node/commit/10add8166b069038ef97be2edc781822dcb2b308))
- Fix format strings in test files by extracting expressions ([b08beac7602e](https://github.com/BTCDecoded/blvm-node/commit/b08beac7602e8a68d5fba48e1ef56debd50570e7))
- Fix final format string warnings ([9b527b98a97d](https://github.com/BTCDecoded/blvm-node/commit/9b527b98a97d7bc7a0fa122df1f6355ed2285a46))
- Fix all remaining clippy errors ([ed04e5b09ff2](https://github.com/BTCDecoded/blvm-node/commit/ed04e5b09ff28d8a0cf2c2fdd95406077e378b03))
- Fix remaining clippy issues ([3e68fcb5a889](https://github.com/BTCDecoded/blvm-node/commit/3e68fcb5a889effba6617a4156986f1f8eea3cbf))
- Fix doc indentation and test unused Result ([c03a03ca02a2](https://github.com/BTCDecoded/blvm-node/commit/c03a03ca02a2c8fbe4cd50e4661488eb224cb122))
- Fix version check to use cargo index in Determine version step ([7513fd2cb6e5](https://github.com/BTCDecoded/blvm-node/commit/7513fd2cb6e57fbed10fb31a137a1ea545e6d26c))
- Make Build step depend on Verify step ([9ff336261217](https://github.com/BTCDecoded/blvm-node/commit/9ff336261217d03cde96ad0ef484bd802660e5b6))
- Fix compilation errors ([d0b5058c90f8](https://github.com/BTCDecoded/blvm-node/commit/d0b5058c90f8fbb39da4127349024ae81671af57))
- Fix clippy uninlined_format_args in convert-bitcoin-core-config ([0662f3407ab3](https://github.com/BTCDecoded/blvm-node/commit/0662f3407ab3ff9559cc314d3d7ad17476ad5c8c))
- Add Kani verification tier system to bllvm-node ([fda5efba41a9](https://github.com/BTCDecoded/blvm-node/commit/fda5efba41a96e8f5da9c3359765870e9eadd657))
- Improve proof tier categorization to handle unwind_bounds constants ([eb45120a160c](https://github.com/BTCDecoded/blvm-node/commit/eb45120a160cedc43d2d91d99f84190fd48d765d))
- Fix proof tier categorization for unwind_bounds constants ([2304fee7fb71](https://github.com/BTCDecoded/blvm-node/commit/2304fee7fb7165a21655f87cb44df23035931325))
- Limit proptest cases for expensive mining property tests ([706d35036241](https://github.com/BTCDecoded/blvm-node/commit/706d350362410b5e023893e9407446e0b1af60c5))
- Optimize test execution: run tests once with coverage ([217da1eafd1c](https://github.com/BTCDecoded/blvm-node/commit/217da1eafd1c33452ad109843d861304f2198fca))
- Skip test job entirely when skip_tests is true ([6ec5f6362634](https://github.com/BTCDecoded/blvm-node/commit/6ec5f636263457f1357e6ef534501fa54fbb818d))
- Enable local path dependencies for Kani verification testing ([37ad593ba397](https://github.com/BTCDecoded/blvm-node/commit/37ad593ba397dd2219d39977d1a57ce256d8d93f))
- Fix Kani proof compilation errors in bllvm-node ([afa414f8578d](https://github.com/BTCDecoded/blvm-node/commit/afa414f8578db7eefa6d8abca28faa5ffbfe7883))
- Fix remaining Kani proof compilation errors ([02fee48d998f](https://github.com/BTCDecoded/blvm-node/commit/02fee48d998f5798509c5f51d95d94dd724e16ec))
- feat: Complete FIBRE implementation with optimizations ([432d22a58850](https://github.com/BTCDecoded/blvm-node/commit/432d22a588503c238eff486a152afdff23945e00))
- fix: Use local path dependency for bllvm-protocol to access fibre module ([ea4c821779bf](https://github.com/BTCDecoded/blvm-node/commit/ea4c821779bf7c1844b5933fe76526ba20a64be1))
- fix: Restore published crate dependency with patch override for local dev ([b0b18a63eb17](https://github.com/BTCDecoded/blvm-node/commit/b0b18a63eb1766d5c586fedb17dd362629639980))
- Fix clippy warnings and use inexact version for bllvm-protocol ([9b3a4cb098ea](https://github.com/BTCDecoded/blvm-node/commit/9b3a4cb098ead8b0f59792a38d0c89100a53ef66))
- Fix clippy to exclude tests (standard practice) ([50c3ec2a8d0b](https://github.com/BTCDecoded/blvm-node/commit/50c3ec2a8d0bd6eb52c6f1a395251c454f0041a4))
- Release pipeline: Use published crates instead of checking out repos ([b3b249808a27](https://github.com/BTCDecoded/blvm-node/commit/b3b249808a27fdc9525c25cb352a8cbcac8333d9))
- Fix release pipeline: Comment out patch section instead of removing ([672b2e2ee1c1](https://github.com/BTCDecoded/blvm-node/commit/672b2e2ee1c191f8fd93fc6a8d0f762c987135c8))
- Use temporary Cargo.toml for release instead of modifying source ([1dd7f32cac61](https://github.com/BTCDecoded/blvm-node/commit/1dd7f32cac61c23604e96489ab2e97d7c951eff2))
- Refactor: Use .cargo/config.toml for local dependencies instead of [patch.crates-io] ([3c804d57eefc](https://github.com/BTCDecoded/blvm-node/commit/3c804d57eefc5c7b13d06ce7f84a2314533a3270))
- Make FIBRE module optional behind feature flag ([3bebf5cf40c9](https://github.com/BTCDecoded/blvm-node/commit/3bebf5cf40c92b0f093b67c03f8494b41d77ce68))
- Fix type annotation error when zmq feature is disabled ([9f77aea38bb8](https://github.com/BTCDecoded/blvm-node/commit/9f77aea38bb802847a06a4b782965dd27826b777))
- Fix formatting and clippy warnings ([cf99a0672a1e](https://github.com/BTCDecoded/blvm-node/commit/cf99a0672a1e23b067775ba3841b5debc56cc1f1))
- Apply cargo fmt formatting fix ([43d458318dd3](https://github.com/BTCDecoded/blvm-node/commit/43d458318dd33ae17faa8cbaf567906ea3bbc76a))
- Fix fibre feature compilation issues ([5e9a616c4a66](https://github.com/BTCDecoded/blvm-node/commit/5e9a616c4a66d473ac3b156234c3a557397748ef))
- Fix duplicate FibreConfig import ([059f436cf19f](https://github.com/BTCDecoded/blvm-node/commit/059f436cf19fcc38ac1ae8f8a4b1da4862e55222))
- Gate entire fibre.rs module behind feature flag at file level ([5d08605dbdb3](https://github.com/BTCDecoded/blvm-node/commit/5d08605dbdb331e9ab28df7c6d893b76f88fd22b))
- Remove local dependency checkouts from test job ([269bbb7b0497](https://github.com/BTCDecoded/blvm-node/commit/269bbb7b0497f99ee08caf63ed631aa23d380508))
- Remove local dependency checkouts from clippy and build jobs ([de294e0522a3](https://github.com/BTCDecoded/blvm-node/commit/de294e0522a3cf6cd84aa37ac69b801b3f1d9bbf))
- Fix setup job to not leave dependency directories behind ([65abe2edf0db](https://github.com/BTCDecoded/blvm-node/commit/65abe2edf0dbb54f90b4522c11c22326d28e3087))
- Remove leftover dependency checkouts from test job ([fdc78f258c30](https://github.com/BTCDecoded/blvm-node/commit/fdc78f258c30e0b65e6ba222ede07eeb280e7b82))
- Limit tarpaulin parallelism to prevent OOM ([78a1f1fb8142](https://github.com/BTCDecoded/blvm-node/commit/78a1f1fb8142d141c13cda41d5919b37345d1efa))
- Implement coverage strategy and add comprehensive test coverage ([13fd42f816a7](https://github.com/BTCDecoded/blvm-node/commit/13fd42f816a7a40b1a0c3e1c1f00e1aa88f61928))
- Mark timing-dependent rate limiter test as ignored ([b5e55e07e487](https://github.com/BTCDecoded/blvm-node/commit/b5e55e07e4878da4d94121869b2208aea39dbfec))
- Complete getblock RPC method implementation ([63d8bae842e0](https://github.com/BTCDecoded/blvm-node/commit/63d8bae842e064ce45a271205eab3844dac29b3b))
- Implement advanced indexing: address and value range indexing ([18040e83c7f2](https://github.com/BTCDecoded/blvm-node/commit/18040e83c7f282bd9b6450a61fe9161804f600a4))
- Implement Fibre statistics tracking ([9167478488d5](https://github.com/BTCDecoded/blvm-node/commit/9167478488d5bca65128b3d0298ae02a43bf6288))
- Implement RBF and Mempool Policies ([e26e0a7f0d32](https://github.com/BTCDecoded/blvm-node/commit/e26e0a7f0d32328b050d6bb3a9ada5bc59676af7))
- docs: remove working documents and improve timelessness ([0cd9ab167162](https://github.com/BTCDecoded/blvm-node/commit/0cd9ab167162b2aee07b62e51a3262089902993e))
- feat: integrate protocol validation and update error handling ([975892c78063](https://github.com/BTCDecoded/blvm-node/commit/975892c78063b3d3c3249b15d7225a4690aa360e))
- Remove arc_clone and arc_new helper functions ([607cb45ce2db](https://github.com/BTCDecoded/blvm-node/commit/607cb45ce2dbf5294c7b53c99ee380cab0abc843))
- Add Current Implementation Status section to README ([b1cfc532bd7f](https://github.com/BTCDecoded/blvm-node/commit/b1cfc532bd7fc444091ed0fdc808ad722763c1bd))
- Add comprehensive test coverage for remaining gaps ([117e16051f53](https://github.com/BTCDecoded/blvm-node/commit/117e16051f53c8521dbc495d1fd21003ab136fad))
- Add comprehensive test coverage for integration tests, config, monitoring, and feature-gated components ([34f6e3e1726b](https://github.com/BTCDecoded/blvm-node/commit/34f6e3e1726b78eed77f3af60adec451dd3fbf05))
- Fix edge case test in bip21_comprehensive_tests ([caa0c9c3df90](https://github.com/BTCDecoded/blvm-node/commit/caa0c9c3df90b7251ce42e06b6ca89b322b1dda2))
- Fix test warnings, clippy issues, and ensure all tests pass per file/subsystem ([551f6c4b91ce](https://github.com/BTCDecoded/blvm-node/commit/551f6c4b91ce04d4189a3777d4c934051e7a5192))
- Add comprehensive test coverage for utility modules (arc, async, env, lock, signal, time, option, error) ([62ece82c9a2e](https://github.com/BTCDecoded/blvm-node/commit/62ece82c9a2e2492d6871349f8b1a098a98af4a5))
- Add comprehensive test coverage for protocol extensions, logging, and module API edge cases ([1270a1a03fb7](https://github.com/BTCDecoded/blvm-node/commit/1270a1a03fb7f5f8400d3b1dc574ab68562c2c83))
- Fix test compilation errors and update documentation ([7c88492b51b2](https://github.com/BTCDecoded/blvm-node/commit/7c88492b51b2115e725f96ed7aab76efe2add2e6))
- docs: apply documentation standards - imperative mood, timeless language ([745934a7dd38](https://github.com/BTCDecoded/blvm-node/commit/745934a7dd381fd63aa21b8c38390ea66b4a4724))
- docs: add architecture and module system documentation ([646ebf056018](https://github.com/BTCDecoded/blvm-node/commit/646ebf056018a085c5c2ecfe9bfe49844e45f57e))
- docs: clarify security boundaries in SECURITY.md ([b38a0d6c63e7](https://github.com/BTCDecoded/blvm-node/commit/b38a0d6c63e779e6b2e4650105d172c966cd67a3))
- feat: Add complete payment processing system with CTV support ([00f77f2e76d5](https://github.com/BTCDecoded/blvm-node/commit/00f77f2e76d52dac0f22b591b9d17ffca2a0c897))
- feat(node): add P2P governance message relay to bllvm-node ([a1dc8da3c7e2](https://github.com/BTCDecoded/blvm-node/commit/a1dc8da3c7e25313c627f73b3a2a559def2bdd90))
- Improve Bitcoin Core RPC compatibility: SegWit support and comprehensive testing ([4f21b166cd99](https://github.com/BTCDecoded/blvm-node/commit/4f21b166cd99ef8a30ee6e828ae41aea0b097533))
- feat: Enhance sendrawtransaction with maxfeerate/allowhighfees and add createrawtransaction ([7efe9378f138](https://github.com/BTCDecoded/blvm-node/commit/7efe9378f13886e2f992b57a70b4b4cc5fd9510b))
- feat(node): major module system overhaul - core infrastructure ([58811bf0156b](https://github.com/BTCDecoded/blvm-node/commit/58811bf0156b64562a45d46af7d8add93b7724a1))
- refactor: Rename from bllvm-node to blvm-node ([21c4864fc558](https://github.com/BTCDecoded/blvm-node/commit/21c4864fc5583384a3f12fe37c03960680f1a1f7))
- docs: consolidate CONTRIBUTING.md, link to blvm-docs ([8c63f87ba188](https://github.com/BTCDecoded/blvm-node/commit/8c63f87ba188e67a0b72eb9cd28019b57d012875))
- refactor(spam-filtering): update imports to use top-level spam_filter module ([d094d48a2530](https://github.com/BTCDecoded/blvm-node/commit/d094d48a253017a1b8125243801001d4d95b4b9f))
- feat(ci): implement strong tier for critical node proofs ([f145f5ac5890](https://github.com/BTCDecoded/blvm-node/commit/f145f5ac589029e5b8746e7443ea221279d8891e))
- perf(ci): use jobs=3 for strong tier proofs ([dc7eb94e4632](https://github.com/BTCDecoded/blvm-node/commit/dc7eb94e4632e646e809adaa0c8591261118cfdd))
- feat(ci): Move medium/slow Kani proofs to AWS spot runners ([dc0cae4d239c](https://github.com/BTCDecoded/blvm-node/commit/dc0cae4d239cfe7525a96bca3f1060d8e8f86a02))
- feat: Enhance module system, network integration, and RPC improvements ([74f576b3f6e9](https://github.com/BTCDecoded/blvm-node/commit/74f576b3f6e9c9edf92b7ead15823b65d7cf78b5))
- Resolve merge conflicts after pulling remote changes ([32b6248789d5](https://github.com/BTCDecoded/blvm-node/commit/32b6248789d5c2a9c9a9d3b5e82208860ee55f51))
- Update CI and module system improvements ([80fbcee97cd3](https://github.com/BTCDecoded/blvm-node/commit/80fbcee97cd38a0c8ecb7c905a2e59f21c50fd41))
- fix: Pass AMI ID through job output instead of using secrets in with section ([11af8a2d8bea](https://github.com/BTCDecoded/blvm-node/commit/11af8a2d8beae429df452667833f327b823f2f65))
- Update connect_block calls to provide network_time parameter ([710abfaef5ee](https://github.com/BTCDecoded/blvm-node/commit/710abfaef5ee897f7f6caf8fac2fba6efc15b9d5))
- Remove historical documentation summaries ([41ce95046096](https://github.com/BTCDecoded/blvm-node/commit/41ce95046096899f30a950e5710ff6e41d67dade))
- docs: fix typos (bllvm -> blvm) and improve feature descriptions ([6531409f21c7](https://github.com/BTCDecoded/blvm-node/commit/6531409f21c74c342933d00321267062a8c4079f))
- Update module marketplace payment split and manifest fields ([df444aa9f413](https://github.com/BTCDecoded/blvm-node/commit/df444aa9f413d444924d72933e2c0feaa1500d5a))
- Remove fee forwarding configuration ([c36e848d1ff0](https://github.com/BTCDecoded/blvm-node/commit/c36e848d1ff000f79c440f2ba78a93fd0c88bf6e))
- feat(utxo-commitments): Add UTXO proof network protocol ([2a30377c8418](https://github.com/BTCDecoded/blvm-node/commit/2a30377c84183ad53c36013a17f220f58f221885))
- Implement request_full_block() for UTXO commitments ([28be930c7747](https://github.com/BTCDecoded/blvm-node/commit/28be930c7747e1b0ea1b108fb278e0fe425a0576))
- refactor(network): Restructure NetworkManager to use Arc for Parallel IBD ([a1de27c87544](https://github.com/BTCDecoded/blvm-node/commit/a1de27c87544493ef2010098c4bcc7b8bd925474))
- Add RocksDB backend with Bitcoin Core compatibility ([7d93533f330b](https://github.com/BTCDecoded/blvm-node/commit/7d93533f330b88c9c1b3ad467b64ce015f15974e))
- Documentation cleanup: Remove outdated status docs and fix README ([652e6ad8201e](https://github.com/BTCDecoded/blvm-node/commit/652e6ad8201e1327a697237e43a35c5f19600d21))
- Fix IBD header sync and implement iterative header download ([ac05d0f0305b](https://github.com/BTCDecoded/blvm-node/commit/ac05d0f0305b24690735fe2c615c616d8fadf7fe))
- docs: extract caching analysis and remove temporary file ([0486c76c5041](https://github.com/BTCDecoded/blvm-node/commit/0486c76c50415ed00eeaa88d4abe2e5c54e86975))
- Implement peer disconnection logic with message size limits and timeouts ([36d0ebaae9b6](https://github.com/BTCDecoded/blvm-node/commit/36d0ebaae9b6d54d841c7bf3ae1dd646e6d4c241))
- Optimize header sync with parallel pipelining ([8519c3801340](https://github.com/BTCDecoded/blvm-node/commit/8519c38013400c77b17408f9fea5d2fd9e764781))
- Fix height 0 storage and peer index bounds in header sync ([0cc61bda7e14](https://github.com/BTCDecoded/blvm-node/commit/0cc61bda7e142434c5fa9d4c12ca7ac007632d99))
- feat(node): implement IBD optimizations with LAN peer support ([24a30c91fa7b](https://github.com/BTCDecoded/blvm-node/commit/24a30c91fa7b8916ac435d34ec744a6f0ab216fa))
- feat(module): add NodeApiIpc and ModuleIntegration helpers ([d214f67ed26b](https://github.com/BTCDecoded/blvm-node/commit/d214f67ed26b30e46540e05d80537c1c0efd5ee4))
- feat(ibd): increase priority chunks and add buffer overflow protection ([babaee9f97b0](https://github.com/BTCDecoded/blvm-node/commit/babaee9f97b00a7256c6fa0ce4cc67a4049d3ff6))
- feat(ibd): dynamic memory-based buffer limits ([1504b7b81a01](https://github.com/BTCDecoded/blvm-node/commit/1504b7b81a01048d67d053a557f30bf8b66e2fcc))
- feat(bitcoin-rust): integrate miniscript, bitcoinconsensus, standardize bitcoin_hashes ([9d9ceff2544b](https://github.com/BTCDecoded/blvm-node/commit/9d9ceff2544b85e8db49e903bd0af0acb10b1954))
- feat: remove sequential sync fallback, require parallel IBD only ([2fc176986b50](https://github.com/BTCDecoded/blvm-node/commit/2fc176986b50644f69134a1148cb0b789bcb350d))
- fix: pruning, connect_block, UTXO commitments client, bllvm typos ([70c0890e01b6](https://github.com/BTCDecoded/blvm-node/commit/70c0890e01b6f799c8a60e50da90c071ecedc591))
- perf: IBD Phase A/B/C, flush caps, memory off path, gap-fill profiling ([a73b14ce7312](https://github.com/BTCDecoded/blvm-node/commit/a73b14ce7312dda9bcffd4f89d9eac94d8aab689))
- perf: IBD prefetch pipeline, RocksDB default, disk_utxo batch, FxHash ([7c4ab1605d1c](https://github.com/BTCDecoded/blvm-node/commit/7c4ab1605d1c238742ad10d2d5ef98ba12007fd8))
- perf: parallel_ibd pipeline, prefetch, disk_utxo, feeder lock ([76b6c2efc505](https://github.com/BTCDecoded/blvm-node/commit/76b6c2efc505a3b84a8a448f1e13cbf1642ba02e))
- refactor: secp256k1 crates.io, blvm-muhash, utxo-commitments default, IBD/storage ([3624be2aa026](https://github.com/BTCDecoded/blvm-node/commit/3624be2aa0267074f5c85b179fac18900702b6d2))
- refactor: remove Kani proofs, update docs and module system ([84edc5a268d1](https://github.com/BTCDecoded/blvm-node/commit/84edc5a268d118dad18bf68970e3436d40876627))
- refactor(node): modular IBD and storage; expanded network, RPC, config, and CLI ([a0ad44aca3c1](https://github.com/BTCDecoded/blvm-node/commit/a0ad44aca3c13628fadfe18cd0ee0aaf19d83fdf))
- feat(node): parallel IBD, storage, RPC, regtest mining; doc link fixes ([f7d676ef6974](https://github.com/BTCDecoded/blvm-node/commit/f7d676ef6974dfd1fb998e69893f7e611dfdee78))
- style: rustfmt; deps: crates.io blvm-muhash (>=0.1, <1) ([9d2981bd5f07](https://github.com/BTCDecoded/blvm-node/commit/9d2981bd5f07265d73c8fb4e98cc2a38f59b5ada))
- chore: CI cache keys from Cargo.toml (libraries do not commit Cargo.lock) ([99d592a551ec](https://github.com/BTCDecoded/blvm-node/commit/99d592a551ec4a15488893e83043d35fb9f187c9))
- ci: BTCDecoded/rust-ci composites and install-rust-toolchain ([500b952d27b5](https://github.com/BTCDecoded/blvm-node/commit/500b952d27b5f62271c6fda433f2a69c2b1e41b6))
- add RPC method examples ([e5130a0cc168](https://github.com/BTCDecoded/blvm-node/commit/e5130a0cc1685113eeb998ed6d364d2223b70aa8))
- check spacing ([d38cec7996c3](https://github.com/BTCDecoded/blvm-node/commit/d38cec7996c39928f1bbe241ca9dbc638409ffd5))
- minor spacing updates ([4db23f81284b](https://github.com/BTCDecoded/blvm-node/commit/4db23f81284b601fbf23f7681315b9543b4810ec))
- add additional RPC method examples ([73db79f15137](https://github.com/BTCDecoded/blvm-node/commit/73db79f1513714a711457c675cf607240a8596d0))
- pre PR review cleanup ([d95088255767](https://github.com/BTCDecoded/blvm-node/commit/d950882557670b62188b4d649e3e7060b21f2280))
- feat(node): P2P getdata, selective withhold, and module IPC serve policy ([8c5c48384a63](https://github.com/BTCDecoded/blvm-node/commit/8c5c48384a63d6559d6a318704b3f3dfc2fcbbee))
- test(utxo-commitments): integration tests, Redb storage in tests, stable test allocator ([b7eb40a58560](https://github.com/BTCDecoded/blvm-node/commit/b7eb40a585608164e078119de13fe1b979c7b01f))
- ci: crates.io-only workflow; tighten Cargo.toml policy ([beae43a4a494](https://github.com/BTCDecoded/blvm-node/commit/beae43a4a494ead28ae9f89cd8f700fb521b081a))
- deps: align zeroize and smallvec with iroh/rustls and hyper ([e096765f3df4](https://github.com/BTCDecoded/blvm-node/commit/e096765f3df40d9d3e444c69dced22b3744b5ed4))
- docs, fuzz: soften Core-specific claims; monorepo fuzz patches ([61012a360791](https://github.com/BTCDecoded/blvm-node/commit/61012a360791f1d2a31ba7bc2e1dfd6f488fa028))
- ci(verify): inline blvm-spec clone (setup-blvm-spec action missing on rust-ci) ([5747d3ceb24e](https://github.com/BTCDecoded/blvm-node/commit/5747d3ceb24e31a273f32b479080a673c8ed2994))
- Revert "ci(verify): inline blvm-spec clone (setup-blvm-spec action missing on rust-ci)" ([61fcccfc8412](https://github.com/BTCDecoded/blvm-node/commit/61fcccfc8412e515eb3f5ac1452b3f884dfd4407))
- fuzz: extend node fuzz crate and CI ([b30578be782b](https://github.com/BTCDecoded/blvm-node/commit/b30578be782b5d9f11a3e094f8f359d2e268b28e))
- feat(network)!: remove economic-node governance handlers and IPC hooks ([e0b42cabd56e](https://github.com/BTCDecoded/blvm-node/commit/e0b42cabd56e3d7e060e4a61608e07bcfcf38afd))
- docs(rpc,zmq): QUIC JSON-RPC vs mandatory auth; ZMQ wire payload format ([0f9445ffd6d6](https://github.com/BTCDecoded/blvm-node/commit/0f9445ffd6d6af4708da9d9167f0dbc3441d42ec))
- fix: pin rustc-hash for bindgen compatibility ([e9cc71fc7caf](https://github.com/BTCDecoded/blvm-node/commit/e9cc71fc7caf14290e6ecadc3ecf1c6af2b7c9c5))
- fix(node): RocksDB 0.24, P2P parity, IPC length codec ([0ad590a22ca2](https://github.com/BTCDecoded/blvm-node/commit/0ad590a22ca23caa6b783c28efcd9c97782dc4b5))
- chore(node): consensus import gate tooling and rustfmt protocol parser ([43fb15110b07](https://github.com/BTCDecoded/blvm-node/commit/43fb15110b073347291542bcdfd5e681de788c40))
- ci(fuzz): shard node fuzz jobs, one build per shard ([b08e9848b2f3](https://github.com/BTCDecoded/blvm-node/commit/b08e9848b2f3c55c9aa5e1e205f6bcb17d621085))
- fix(blvm-node): build with rest-api and iroh; relax regex for toolchains ([e4912c4d3b63](https://github.com/BTCDecoded/blvm-node/commit/e4912c4d3b633701ee2ca74443c2390eb61d38cb))
- docs: point readers at docs.thebitcoincommons.org for BLVM book links [skip ci] ([b2d6687ae5f1](https://github.com/BTCDecoded/blvm-node/commit/b2d6687ae5f1607b5b97844ffdb75f3ff5a0044a))
- ci: align workflow, release manifest check, and clippy -D clean ([89dd5982b2e3](https://github.com/BTCDecoded/blvm-node/commit/89dd5982b2e396bd60e78eaee0964e60b75b409a))
- Fix compilation errors for all-features build ([e69ce34bbe8e](https://github.com/BTCDecoded/blvm-node/commit/e69ce34bbe8ef8e17eaecc82c9c4017bb662b10f))
- ci: drop perf label from verify job runs-on ([b94a39a920d9](https://github.com/BTCDecoded/blvm-node/commit/b94a39a920d91412a29576f24ac079a8d58eafc2))
- style: rustfmt miniscript_tests policy bindings ([9edf178eb1ea](https://github.com/BTCDecoded/blvm-node/commit/9edf178eb1ea8b6502916e85a525d327a8cdf81b))
- ci(fuzz): checkout blvm-primitives sibling for fuzz [patch] ([c4d71ccb4364](https://github.com/BTCDecoded/blvm-node/commit/c4d71ccb4364d3d4d0de77167d97cbbbce35688f))
- fuzz(blvm-node): resolve blvm-protocol from crates.io; drop sibling CI checkouts ([bb3b6bfc024e](https://github.com/BTCDecoded/blvm-node/commit/bb3b6bfc024e6e32e21a7ac2dbb748935547f1ae))
- fuzz: restore [patch.crates-io] for local monorepo (CI strips) [skip ci] ([75938595d098](https://github.com/BTCDecoded/blvm-node/commit/75938595d098eb3ea9cd07ce876c37f82fdf2a37))
- ci: quieter tests; fix tidesdb_tests Arc<dyn Database> inference ([fe3615c29adc](https://github.com/BTCDecoded/blvm-node/commit/fe3615c29adc6e75ed542c297645ab71cab14834))
- ci: rustfmt tidesdb tests; crates.io UA + retries for verify fetch ([9ce0beb3a35e](https://github.com/BTCDecoded/blvm-node/commit/9ce0beb3a35e17613ef5041f05a3f4ef371b817b))
- fix(test): read Full<Bytes> REST bodies via BodyExt::collect ([47e8805c1a8d](https://github.com/BTCDecoded/blvm-node/commit/47e8805c1a8d329a77a34bd4a6f0a09d70a0764a))
- ci(verify): clone blvm-spec and pass --spec-path to spec-lock ([8e6fd21954a0](https://github.com/BTCDecoded/blvm-node/commit/8e6fd21954a0f04d8b411c77367fc09f05e3c09b))
- tests: fix get_mempool arity and silence unused config warnings ([6ab19b6d39d8](https://github.com/BTCDecoded/blvm-node/commit/6ab19b6d39d84138eac990817067723ea0612985))
- tests(iroh): import TransportListener for local_addr() ([2fcbde796ddd](https://github.com/BTCDecoded/blvm-node/commit/2fcbde796dddd55619e7d2f118cbeb2d87459863))
- docs(contributing): warn about RAM when building all-features tests ([d86c72aea43a](https://github.com/BTCDecoded/blvm-node/commit/d86c72aea43a1b31d3448a450defb52960b2dc5c))
- ci: use default crate features for test and coverage jobs ([50a956c1377f](https://github.com/BTCDecoded/blvm-node/commit/50a956c1377f0a824f57f6c5562a5f3cd672e1a6))
- chore(tests): fix miniscript empty check and silence default-feature warnings ([6c6bf6500a77](https://github.com/BTCDecoded/blvm-node/commit/6c6bf6500a774f8168f74e8e1f07f8c076df7545))
- fix(network): AddrV2 TCP adapter + correct mainnet magic for wire ([c97edd086b19](https://github.com/BTCDecoded/blvm-node/commit/c97edd086b19b757060c52cdc193ad5b3420990d))
- fix(storage): pre-register RocksDB CF names used by rocksdb_tests [skip_fuzz] ([5aa30e3157b1](https://github.com/BTCDecoded/blvm-node/commit/5aa30e3157b1d72830bb2037a3a9cf9cf7beab0c))
- fix(utxo-commitments): safe get_peer_ids sync bridge + multi_thread tests [skip_fuzz] ([0d1c393809d4](https://github.com/BTCDecoded/blvm-node/commit/0d1c393809d4aca5b308306d6f180476d5e8756a))
- fix(ibd): parallel download liveness and feeder ordering deadlocks ([f684b3967ac4](https://github.com/BTCDecoded/blvm-node/commit/f684b3967ac49763a33516c3d87a2e31524b1057))
- docs(security): use security@thebitcoincommons.org for disclosures ([c32371046530](https://github.com/BTCDecoded/blvm-node/commit/c323710465301b07d83494872a60afa211f5eb87))
- feat(ibd): parallel IBD throughput, flush/watermark correctness, and hygiene ([19b272f90765](https://github.com/BTCDecoded/blvm-node/commit/19b272f90765799597093e0c8df97cc52a842a93))
- fix(storage): remove forced 4-thread floor for RocksDB parallelism on weak hardware ([23852eb35422](https://github.com/BTCDecoded/blvm-node/commit/23852eb354226068ca3cd1b23e782617726c7b37))
- fix(ibd): add retire-lag backpressure to prevent pending_log OOM on 16 GiB hosts ([94ba74980ba6](https://github.com/BTCDecoded/blvm-node/commit/94ba74980ba65649730656b5da90db68d0b501a0))
- fix(ibd): switch retire backpressure from block-lag to pending-ops, fix BPS regression ([d50aebc86039](https://github.com/BTCDecoded/blvm-node/commit/d50aebc86039e274d354d00e8bd573571df3d32f))
- ci: add libssl-dev, --locked, update audit ignores and docs ([5c8eb530fa72](https://github.com/BTCDecoded/blvm-node/commit/5c8eb530fa72b4270b774c6127c41ba1a2cd64d3))
- refactor: apply rustfmt formatting across IBD, storage, and test files ([2e7f73cbca59](https://github.com/BTCDecoded/blvm-node/commit/2e7f73cbca59571381f3aa8d34e20177c8b45474))
- fix(ibd): restore peer binding in ChunkAssigner to fix 10x BPS regression ([05af974a2901](https://github.com/BTCDecoded/blvm-node/commit/05af974a2901ee1006d6ce3f9f5772de6f364403))
- fix(ibd): stop retire from scanning huge pending logs each block ([3e2a99be13e9](https://github.com/BTCDecoded/blvm-node/commit/3e2a99be13e92a892334fe30c6571447d4dd7aad))
- feat(ibd): soft UTXO autorepair; blvm-muhash 0.1 semver like siblings ([538b1636bfd2](https://github.com/BTCDecoded/blvm-node/commit/538b1636bfd27d019b4f7e434498f40eda57cb15))
- ci: drop --locked from blvm-node builds (library-style, no Cargo.lock) ([83092ca3cde1](https://github.com/BTCDecoded/blvm-node/commit/83092ca3cde10431493a2da0e1e6549537ab5975))
- ci: use DPkg::Lock::Timeout=120 instead of apt lock polling script ([f6965ba3bb2e](https://github.com/BTCDecoded/blvm-node/commit/f6965ba3bb2edc07c2c97cbd5d964640ab8fbdd2))
- ci: wipe registry/src after cache restore to fix parallel-job corruption ([d20e03adc866](https://github.com/BTCDecoded/blvm-node/commit/d20e03adc866cd998f67a5cad07de5bd3fc9b1e0))
- ci: isolate CARGO_HOME per runner to prevent registry extraction races ([601a6d69c1d0](https://github.com/BTCDecoded/blvm-node/commit/601a6d69c1d0644f7ef434dd79cac522ec32e4d3))
- ci: fix CARGO_HOME isolation — use RUNNER_TEMP in a step, not workflow env ([57e316d6b33b](https://github.com/BTCDecoded/blvm-node/commit/57e316d6b33ba16a19b23f6695aba2bd79b822bc))
- ci: remove --locked from cargo doc (no Cargo.lock committed) ([e4b7d420ad6b](https://github.com/BTCDecoded/blvm-node/commit/e4b7d420ad6b5c45e8d81b1ee039268c22c3aa65))
- ci: clean fuzz/target before build to avoid stale dep-info errors ([9e53e03b0af4](https://github.com/BTCDecoded/blvm-node/commit/9e53e03b0af4f98e6d47240882478419b76e3dc6))
- ci: shared dev build cache; fix clippy -D warnings [skip docs] ([91099d5db5c4](https://github.com/BTCDecoded/blvm-node/commit/91099d5db5c494b44ce3cb983037eff4cbec2ccd))
- chore: optional fibre/reed-solomon crc32; reqwest behind governance; drop redb default ([19829dfd3e60](https://github.com/BTCDecoded/blvm-node/commit/19829dfd3e605cb411c8e91d157239ee49dcf488))
- fix: expose maybe_evict for IBD bench; narrow tokio features; tidy mempool tests ([0ba5bb532ac7](https://github.com/BTCDecoded/blvm-node/commit/0ba5bb532ac747ea98ca9c54cc691d4a1b4352d6))
- fix(tests): pruning + backend interchangeability after default redb removed ([5e178bbc3db1](https://github.com/BTCDecoded/blvm-node/commit/5e178bbc3db1b74a218efb84f16efac8d7bd467c))
- ci: add [skip_docs_fuzz] to skip docs and fuzz on push ([a20aaa04372a](https://github.com/BTCDecoded/blvm-node/commit/a20aaa04372afa3dbd6437d9931b4b647c52dff7))
- ci: retry apt-get on lock contention; keep paired skip tags [skip_docs] [skip_fuzz] ([3794273a6c6d](https://github.com/BTCDecoded/blvm-node/commit/3794273a6c6d16e207ed47272a297b1f704e4c41))
- ci: disable incremental builds, add test binary cleanup and weekly disk sweep [skip_docs] [skip_fuzz] ([c628256313d1](https://github.com/BTCDecoded/blvm-node/commit/c628256313d1541f35fcabced6823043db97ff7c))
- ci: add disk usage dump steps for space debugging [skip_docs] [skip_fuzz] ([c4d1d3866de3](https://github.com/BTCDecoded/blvm-node/commit/c4d1d3866de33f8b5d74e9d0fdd9a365ee398c59))
- ci: teardown job deletes run dir after all jobs; drop mtime guard on stale dir cleanup [skip_docs] [skip_fuzz] ([3a5ba2bede7f](https://github.com/BTCDecoded/blvm-node/commit/3a5ba2bede7f447624212a264846140967ba1986))
- ci: emergency pre-flight cleanup in setup; cleanup workflow deletes all stale dirs [skip_docs] [skip_fuzz] ([115482fa2649](https://github.com/BTCDecoded/blvm-node/commit/115482fa26492eb17352a8b3aeb027102edb2a3f))
- fix: add version = "0.6.7" to tidesdb git dep for cargo publish [skip_tests] [skip_docs] [skip_fuzz] [skip_verify] ([40b4ac8006fe](https://github.com/BTCDecoded/blvm-node/commit/40b4ac8006fed968d53239e10e78db553f3c0e98))
- ci: replace slow version-check loop with single crates.io API call [skip_tests] [skip_docs] [skip_fuzz] [skip_verify] ([0d81f90dc15f](https://github.com/BTCDecoded/blvm-node/commit/0d81f90dc15feb6891eed1f2464fa078e4320c4f))
- ci: skip build-dev when [skip_tests]; build job accepts build-dev skipped [skip_docs] [skip_fuzz] [skip_tests] [skip_verify] ([997ee225a44a](https://github.com/BTCDecoded/blvm-node/commit/997ee225a44a83af025253a22e1124652fd5bdfb))
- ci: drop release-mode test build; allow-dirty on publish dry-run [skip_docs] [skip_fuzz] [skip_tests] [skip_verify] ([195c0ba52556](https://github.com/BTCDecoded/blvm-node/commit/195c0ba52556dc19fc9ca960a65b89eb8de60ac4))
- ci: remove useless cache save from build job [skip_docs] [skip_fuzz] [skip_tests] [skip_verify] ([e7be74c0111a](https://github.com/BTCDecoded/blvm-node/commit/e7be74c0111a56a2f608eb5d7369dde4737cd789))
- fix: add --allow-dirty to cargo publish and clean up check-version step ([d6a190505bf4](https://github.com/BTCDecoded/blvm-node/commit/d6a190505bf4edbb7ae5fe9b42b6d72d30d257b3))
- fix: skip cargo publish verification compile with --no-verify ([d1820e60748d](https://github.com/BTCDecoded/blvm-node/commit/d1820e60748dc8cb2cb47dc54b7a02dd904a5440))
- fix: add attestations/id-token write perms to release job ([c6e539a64e9d](https://github.com/BTCDecoded/blvm-node/commit/c6e539a64e9da4ca6836dfb30a4dffa598644284))
- fix: remove duplicate permissions block in release job ([98ec18facca7](https://github.com/BTCDecoded/blvm-node/commit/98ec18facca75a0e27fda15221629c6754117153))

## [v0.1.2] - 2026-05-08

### Reconstructed

- fix: treat already-published version as failure in publish step ([e2fdf19c0d74](https://github.com/BTCDecoded/blvm-node/commit/e2fdf19c0d74c9e13d0acda8219e5e7d81c200bf))
- fix: bump version directly in Cargo.toml, add hard verification before publish ([6dd21f1b22f4](https://github.com/BTCDecoded/blvm-node/commit/6dd21f1b22f4c264381edbe3a0235b9dbdea477e))
- fix(windows): resolve Windows cross-compile errors in blvm-node ([194ed09dfcbd](https://github.com/BTCDecoded/blvm-node/commit/194ed09dfcbd733d66f1a949babc0e5ffeaede04))
- ci: trigger fast publish for Windows fixes [skip_tests] [skip_docs] [skip_fuzz] [skip_security] ([eb5c3bfe9ea8](https://github.com/BTCDecoded/blvm-node/commit/eb5c3bfe9ea806187812df56114512b0c0876dd2))

## [v0.1.3] - 2026-05-08

### Reconstructed

- ci: skip verify to fast-path publish [skip_tests] [skip_docs] [skip_fuzz] [skip_security] [skip_verify] ([0a4990ac127a](https://github.com/BTCDecoded/blvm-node/commit/0a4990ac127a967704c6493cdb3d947d681c0c95))

## [v0.1.4] - 2026-05-08

### Reconstructed

- fix(windows): gate unix-only IPC code; move dashmap to universal deps ([93449dcae727](https://github.com/BTCDecoded/blvm-node/commit/93449dcae7270cf5869478edf98b22c9023ce853))
- ci: fast publish [skip_tests] [skip_docs] [skip_fuzz] [skip_security] [skip_verify] ([c58001cca95d](https://github.com/BTCDecoded/blvm-node/commit/c58001cca95d283b165fe5f055fb9593d41097ef))
- fix: restore sysinfo dep accidentally dropped in dashmap move ([74c8d979ff4e](https://github.com/BTCDecoded/blvm-node/commit/74c8d979ff4ee85eeb7cacbcbc3dacc1046d8dba))

## [v0.1.5] - 2026-05-12

### Reconstructed

- fix: make sysinfo an optional dep; activate via feature ([0c8cb110af19](https://github.com/BTCDecoded/blvm-node/commit/0c8cb110af193f522edf498681fcbceb36676ee3))
- fix: make zeromq optional dep; activate via zmq feature ([856636a6427d](https://github.com/BTCDecoded/blvm-node/commit/856636a6427df03a5bf144e0c889c8e8ad97fdf5))
- migrate secp256k1 usages to blvm-secp256k1; drop C FFI dep ([23e30eca8459](https://github.com/BTCDecoded/blvm-node/commit/23e30eca845971e0631140e5543c8587cd0948fe))
- deps: make mimalloc/libmimalloc-sys optional; move getrandom to dev-deps ([fe2168a7549e](https://github.com/BTCDecoded/blvm-node/commit/fe2168a7549e402a167e49f0e1dc056ed765477c))
- security: harden mempool policy and P2P peer handling ([bd6f005cea1c](https://github.com/BTCDecoded/blvm-node/commit/bd6f005cea1cae739739157a2ceb9a52bc8b307e))
- refactor: move ge_from_pubkey_bytes to top-level imports in ban_list_signing ([4b72e99394a5](https://github.com/BTCDecoded/blvm-node/commit/4b72e99394a5b69d35e6bfbd30f57c9eb8c3a487))
- security: second-pass mempool and P2P hardening ([043e8cb53248](https://github.com/BTCDecoded/blvm-node/commit/043e8cb5324871c0e7a72111d7de7560bcc33419))
- docs: align CONTRIBUTING lockfile guidance with .gitignore ([771290306af1](https://github.com/BTCDecoded/blvm-node/commit/771290306af1c8152d77ccc3cf6be0bed6349194))
- security: third-pass mempool and P2P hardening ([613a4fc14742](https://github.com/BTCDecoded/blvm-node/commit/613a4fc14742f3b905aaffd10cdec5447315ca0e))
- net: fix four minor hardening gaps ([550dd0f1cfcf](https://github.com/BTCDecoded/blvm-node/commit/550dd0f1cfcf5df20b2f98fd095efc12898a88b9))
- fix: clippy and fmt ([7f28f735135e](https://github.com/BTCDecoded/blvm-node/commit/7f28f735135e4a093110e41dce8557767a878488))
- perf: reduce IBD peak RSS and prevent staged-delta memory spikes ([322a7b4def39](https://github.com/BTCDecoded/blvm-node/commit/322a7b4def39ea24077284937bd29c0104aa6083))
- docs: README, governance stubs, bench paths, testnet compose [skip_ci] [skip ci] ([7ca493b800fc](https://github.com/BTCDecoded/blvm-node/commit/7ca493b800fc18de013c9b9b222bf1d0799811da))
- fix: resolve coordinator livelock when pipeline backlog exceeds next_prefetch_height ([2162ac645b86](https://github.com/BTCDecoded/blvm-node/commit/2162ac645b862fea126a1fed27260fa7b3007b87))
- fix(ibd): validation pipeline, UTXO flush, ram tier; integration test fixes ([6755115621f4](https://github.com/BTCDecoded/blvm-node/commit/6755115621f440394ccd25d10b60c4f6fad1b11e))
- feat: miniscript → blvm-miniscript module; generic RPC override system; marketplace auto-load ([37d49f2150a1](https://github.com/BTCDecoded/blvm-node/commit/37d49f2150a116ef58e5aa999c9b7f38696f3e3a))
- feat: bootstrap-download missing enabled_modules on first boot ([07668810b467](https://github.com/BTCDecoded/blvm-node/commit/07668810b467ba5c761869fd1a33efca0f0999d3))
- refactor: remove in-process ZMQ; use blvm-zmq module ([2ef6cd0fb13f](https://github.com/BTCDecoded/blvm-node/commit/2ef6cd0fb13f129200587c6d2c3d45193893b122))
- chore: clippy -D warnings; align cargo audit with hickory ignores ([bd6f927c660d](https://github.com/BTCDecoded/blvm-node/commit/bd6f927c660d2379564b3728e42890b402e4369c))
- chore(ibd): log [IBD_RESUME] and warn on missing UTXO watermark ([c5158c3b5234](https://github.com/BTCDecoded/blvm-node/commit/c5158c3b5234c12455f125c4fcfba6495fea1813))
- chore: fix missing rpc_overrides field in ModuleManifest/ModuleMetadata test initializers ([349ad8553ed6](https://github.com/BTCDecoded/blvm-node/commit/349ad8553ed66d711d4f1f85f885e0bef22aa626))
- ci(verify): assume deps pre-installed on runner; drop sudo install ([50149118afd1](https://github.com/BTCDecoded/blvm-node/commit/50149118afd1dcb7ba3915f0e8567882afac7e8f))
- ci(fuzz): replace 2-shard matrix with single parallel job using xargs -P$(nproc) [skip ci] ([cdfa5b49c014](https://github.com/BTCDecoded/blvm-node/commit/cdfa5b49c014ed797f9ef41a11d0502dec2390ec))
- ci: merge fuzz-build into fuzz job ([3a3c74e52a77](https://github.com/BTCDecoded/blvm-node/commit/3a3c74e52a77f8a3716816bd083eeed21728ed12))
- ci: skip native dep installs when already present on runner ([475b403ccf25](https://github.com/BTCDecoded/blvm-node/commit/475b403ccf253198fee41539cd9926e608a8e9fe))
- ci(fuzz): skip missing binaries instead of failing hard ([d14466a18a8d](https://github.com/BTCDecoded/blvm-node/commit/d14466a18a8d7394c37a2861b345a3c92d60719b))
- ci: use lld to prevent OOM linker kills on large crate ([12074ad096d7](https://github.com/BTCDecoded/blvm-node/commit/12074ad096d71cdf2e67134e6b640ec999472770))
- ci: redirect all build/cache/tmp paths from /tmp to /mnt/data ([e6a2a9c6d25d](https://github.com/BTCDecoded/blvm-node/commit/e6a2a9c6d25df8259b910140282d0a1ec88f476d))
- ci: nest TMPDIR inside per-run build dir (no top-level /mnt/data perms) ([7c3d87f2fc28](https://github.com/BTCDecoded/blvm-node/commit/7c3d87f2fc28d879fc2fb45c51edf1eb2e9c2600))
- ci: move build/cache/tmp paths under /mnt/data/bitcoin (runner perms) ([1d83a28764db](https://github.com/BTCDecoded/blvm-node/commit/1d83a28764db59a34af22252dd08a453c7ae413a))
- ci: use workspace-relative paths for all build/cache/tmp dirs ([81117f9aacfd](https://github.com/BTCDecoded/blvm-node/commit/81117f9aacfd0002df5c06864bf09247cd45479e))
- ci: drop TMPDIR override, only CARGO_TARGET_DIR needed off /tmp ([424ac1fbb951](https://github.com/BTCDecoded/blvm-node/commit/424ac1fbb9515794a491956b62096e8a7e272a3a))
- fix(tests): use valid_transaction() in test_mempool_manager [skip_docs] [skip_verify] [skip_fuzz] ([1eb18ecb0f86](https://github.com/BTCDecoded/blvm-node/commit/1eb18ecb0f86880cbd6ed2f0b6acd7a6090f4905))

## [v0.1.7] - 2026-05-12

### Reconstructed

- ci: skip pacman installs if packages already present on runner ([274fa53e9716](https://github.com/BTCDecoded/blvm-node/commit/274fa53e97167e03a0ff918858bec46634a87407))
- fix(rpc): add missing server_arc field in RpcManager::with_quinn initializer [skip_docs] [skip_verify] [skip_fuzz] ([164e522c1d54](https://github.com/BTCDecoded/blvm-node/commit/164e522c1d54e58edb7d114e4088bacf78b52bcd))

## [v0.1.9] - 2026-05-13

### Reconstructed

- feat: implement Option B self-describing module registry ([e23d256a9bbe](https://github.com/BTCDecoded/blvm-node/commit/e23d256a9bbe570f66f9e129e2d286aee338b3ba))
- feat(module): add x86_64-windows platform, install binary as .exe on Windows ([b7231603a5b3](https://github.com/BTCDecoded/blvm-node/commit/b7231603a5b3ef45de94dad7738b5d9d48be3f75))
- fix: link sysinfo + reqwest on Windows (was only under cfg(unix)) ([31a78b264126](https://github.com/BTCDecoded/blvm-node/commit/31a78b2641263fefbf8489bbf93c927120bac5db))
- fix(tests): add ModuleManifest.downloads to test structs ([f736859e79cd](https://github.com/BTCDecoded/blvm-node/commit/f736859e79cd67c3c300989277133f54cd5174cd))
- ci: fix runner disk cleanup path + low-space aggressive prune ([baf83cfa97b5](https://github.com/BTCDecoded/blvm-node/commit/baf83cfa97b582c8d1c01f6f7eec9717a246746d))
- ci: harder disk eviction, per-run TMPDIR, prune runner diag logs ([f78451318fa5](https://github.com/BTCDecoded/blvm-node/commit/f78451318fa55168eff9875fe735e9a9f7746358))
- ci: align build root across runners and fix TMPDIR on pooled hosts ([67ec709085ba](https://github.com/BTCDecoded/blvm-node/commit/67ec709085ba9e720473569930f53ee4b972897f))
- ci(setup): mkdir BLVM_BUILD_TARGET/TMPDIR after checkout ([fc901c50d080](https://github.com/BTCDecoded/blvm-node/commit/fc901c50d080d4c925d5275c4e1773dcf529d8b1))
- test(ipc): harden module IPC client tests for slow CI [skip_docs] [skip_verify] [skip_fuzz] ([75c5cb3bcec5](https://github.com/BTCDecoded/blvm-node/commit/75c5cb3bcec58ca5b94fd3fab2891ade0a2d1dd6))
- test(ipc): fix client tests when TMPDIR is long (sockaddr_un limit) ([af90ca6d2e82](https://github.com/BTCDecoded/blvm-node/commit/af90ca6d2e828cc8383cca65551ea9852f61c31e))
- style: rustfmt module_ipc_client_tests ([4d905d03476a](https://github.com/BTCDecoded/blvm-node/commit/4d905d03476ae471f4e74445693e2ab06b772989))
- test(ipc): share socket harness for server tests (CI sockaddr_un path) ([74e07bd86c70](https://github.com/BTCDecoded/blvm-node/commit/74e07bd86c70589ccef327f6afd2406736eefa05))

## [v0.1.10] - 2026-05-15

### Reconstructed

- feat(modules): [modules].registry_url for bootstrap alongside legacy marketplace key ([d2cb010c7593](https://github.com/BTCDecoded/blvm-node/commit/d2cb010c7593b84d6942baabf9f81cd7bb8b1748))
- feat(modules): bootstrap defaults, disabled list, module KV policy ([4b02de3d399a](https://github.com/BTCDecoded/blvm-node/commit/4b02de3d399a4f53b834ea59cf34dfe45e5d4f8f))
- chore: tighten CI clippy for tests and clean up module KV + misc clippy ([0e08a47dc8f6](https://github.com/BTCDecoded/blvm-node/commit/0e08a47dc8f6fcfb3396fc82eaafe85060f21099))
- feat(module): FIBRE hooks for loadable module; drop in-process FIBRE ([f7ff5f49234d](https://github.com/BTCDecoded/blvm-node/commit/f7ff5f49234d44aa426bbc256242112fd9192842))
- chore(ci): enforce fmt/clippy; expect blvm-fibre in default modules ([1ce51712df6d](https://github.com/BTCDecoded/blvm-node/commit/1ce51712df6d7f4bcbf659bc241f0e1a0fc25394))

## [v0.1.11] - 2026-05-15

### Reconstructed

- chore(release): bump to 0.1.10 ([bb0467b24ef7](https://github.com/BTCDecoded/blvm-node/commit/bb0467b24ef7900e96f9dfa22d9d5b2b7829762a))

## [v0.1.12] - 2026-05-16

### Reconstructed

- ci(release): derive next patch from max used version ([e92a932cf979](https://github.com/BTCDecoded/blvm-node/commit/e92a932cf979e0240e1ea5bcc0ecc6d5462a0372))
- feat(stratum-v2): module-first transport; drop in-node Stratum listener ([0c2cc9656cd4](https://github.com/BTCDecoded/blvm-node/commit/0c2cc9656cd4ce3da69f827dcaab27216d245c40))
- feat(stratum-v2): gate P2P Stratum demux; trim default bootstrap modules ([a299bd84ca1a](https://github.com/BTCDecoded/blvm-node/commit/a299bd84ca1a25b00c2cbf52c20f476be9c353b1))

## [v0.1.13] - 2026-05-16

### Reconstructed

- feat(modules): GitHub Releases bootstrap with checksum verify and registry conventions ([f5d6c0127bbe](https://github.com/BTCDecoded/blvm-node/commit/f5d6c0127bbe922e4877052c7583e9be84a3e3fd))

## [v0.1.14] - 2026-05-17

### Reconstructed

- docs: align guides and examples with NodeConfig and blvm operator CLI ([d6d0e76355d1](https://github.com/BTCDecoded/blvm-node/commit/d6d0e76355d1b5dc0695151f134822bd810528d9))
- fix(modules): default module subprocess KV backend to sled for RocksDB/Redb ([a162acc1600a](https://github.com/BTCDecoded/blvm-node/commit/a162acc1600ab7faca4aa206616f6a68959a64a9))

## [v0.1.16] - 2026-05-17

### Reconstructed

- fix(security): centralize cargo audit ignores; tidy CI and cache comments ([2d1c48b66b3e](https://github.com/BTCDecoded/blvm-node/commit/2d1c48b66b3e6c121442c94288644de1524bc485))
- ci: align coverage cache root with main CI; document BLVM_RUNNER_BUILD_ROOT ([7a6aa3c84afa](https://github.com/BTCDecoded/blvm-node/commit/7a6aa3c84afa3008b3dac4515eae347fa0288f58))

## [v0.1.17] - 2026-05-17

### Reconstructed

- feat(node): unify JSON-RPC over TCP HTTP and HTTP/3 QUIC ([4ed4fc855430](https://github.com/BTCDecoded/blvm-node/commit/4ed4fc8554307b410ce0b8192005e8025619a439))

## [v0.1.18] - 2026-05-21

### Reconstructed

- feat(node): improve mainnet IBD onboarding defaults ([d210dfd0fe62](https://github.com/BTCDecoded/blvm-node/commit/d210dfd0fe628513385e72d184bcd08a2538ec3f))
- fix(ci): detect working linker instead of hardcoded lld ([b524284d212b](https://github.com/BTCDecoded/blvm-node/commit/b524284d212b9ef1eea0d42729262fcbc97a021b))
- fix(node): mainnet IBD auto-LAN prefer and WAN single-peer parallel sync ([6369af9f6e85](https://github.com/BTCDecoded/blvm-node/commit/6369af9f6e85cdffe37c0bcda5cacb65d658a711))

## [v0.1.19] - 2026-05-27

### Reconstructed

- feat(ci): publish-dev and develop-chain for coordinated dev releases ([9a5185f294f0](https://github.com/BTCDecoded/blvm-node/commit/9a5185f294f011df524c9cbd06f548ece6efc99c))
- fix(node): audit remediation for IBD, RPC, storage, and transport ([8e1ca842bb54](https://github.com/BTCDecoded/blvm-node/commit/8e1ca842bb548a568d44a6a7f4d8a786c3863e89))

## [v0.1.20] - 2026-05-27

### Reconstructed

- ci: remove Build(dev,shared) step; promote test/clippy/docs to first tier [no ci] ([8924cebfd35e](https://github.com/BTCDecoded/blvm-node/commit/8924cebfd35e600719a1ca1c34c2ef6c88fffcb5))
- fix(transport): derive Debug on NoServerCertVerification for rustls ServerCertVerifier [no ci] ([90afb878e83b](https://github.com/BTCDecoded/blvm-node/commit/90afb878e83b1cef4d1fa80f9501907d52a6740a))
- ci: trigger publish for NoServerCertVerification Debug fix ([4673dd8bc22b](https://github.com/BTCDecoded/blvm-node/commit/4673dd8bc22be9909f598c60da93bac6b81e6a9d))
- fix(ci): remove blank needs entry left by build-dev removal [no ci] ([7d4a43900204](https://github.com/BTCDecoded/blvm-node/commit/7d4a439002044f0a87a8043cc7491d224c3f417d))
- fix(ci): remove dangling needs..result guard left by build-dev removal [no ci] ([eddb0cc4d266](https://github.com/BTCDecoded/blvm-node/commit/eddb0cc4d266c1a4fcda2f850720a47660a69b49))

## [v0.1.21] - 2026-05-27

### Reconstructed

- security: RPC RBAC, startup guard, admin defaults; bump actions to Node24 [no ci] ([5531e45e175e](https://github.com/BTCDecoded/blvm-node/commit/5531e45e175ebc18489829dad59e0a757a815bd5))
- fix(node): audit hardening, dep minimization, and storage rename ([c1616362133f](https://github.com/BTCDecoded/blvm-node/commit/c1616362133f69c7f0885daf9f6bc11ff183d1a7))
- fix(node): use published BIP54 deployment helper for crates.io CI ([7006884fc390](https://github.com/BTCDecoded/blvm-node/commit/7006884fc3908d379ffdfba807a466dde60feb36))
- fix(node): restore bip54_deployment_for_network via protocol re-export ([d21c8f1920d6](https://github.com/BTCDecoded/blvm-node/commit/d21c8f1920d6072ddfbe8f2867df73623a3c94df))

## [v0.1.22] - 2026-05-28

### Reconstructed

- fix(node): audit hardening — REST auth guard, WAL docs, mutex safety, QUIC cert ([4acfc23c2d56](https://github.com/BTCDecoded/blvm-node/commit/4acfc23c2d56b389e48873f5635141770dd246d8))
- fix(build): resolve --no-default-features compile errors and add audit.toml ([39ab20e26196](https://github.com/BTCDecoded/blvm-node/commit/39ab20e26196226684eb63fe7bfea031c568d427))

## [v0.1.23] - 2026-05-28

### Reconstructed

- fix(quinn): gate outbound TLS client config off production feature ([ab0267c09e8c](https://github.com/BTCDecoded/blvm-node/commit/ab0267c09e8c463809da7288196a29e5abc3dd55))

## [v0.1.24] - 2026-05-31

### Reconstructed

- fix(node): annotate handle_version_received for protocol-verification builds ([d695298712d3](https://github.com/BTCDecoded/blvm-node/commit/d695298712d3dd74ab6c227e21ea412bb2fcace6))

## [v0.1.25] - 2026-06-04

### Reconstructed

- fix(ibd): enable assume-valid, graceful shutdown, and faster defaults ([8de25449d940](https://github.com/BTCDecoded/blvm-node/commit/8de25449d94044e91e34fc7b90306500d39577b1))
- perf(ibd): drain prefetched map + default-off per-op MuHash during IBD (Phase 0-B, 0-D) ([84d1016665a2](https://github.com/BTCDecoded/blvm-node/commit/84d1016665a2279e56729dc5bf902ea74b07c8f1))
- feat(ibd): Phase 1 — UtxoDatabase age-tiered UTXO engine (Hornet port) ([699a724512e7](https://github.com/BTCDecoded/blvm-node/commit/699a724512e7eae84180a1991175c083c1f1c2d1))
- feat(ibd-engine): Phase 2 — SpendSession + pipeline wiring ([4c4b23a00cf6](https://github.com/BTCDecoded/blvm-node/commit/4c4b23a00cf62e1b0b296c5d4a27392ed89fcf65))
- feat(ibd-engine): Phase 3 — watermark export + MuHash from scan ([709b99d8c05b](https://github.com/BTCDecoded/blvm-node/commit/709b99d8c05b70629bad00abf6fbf897bfc7be6b))
- ibd: WAN multi-peer work-stealing download ([4e293dfecd36](https://github.com/BTCDecoded/blvm-node/commit/4e293dfecd36b52a8a488c0d5ef5e3959adb5dcb))
- ibd: revert WAN multi-peer to opt-in; reduce fail timeout to 1s ([eddbe4b23f36](https://github.com/BTCDecoded/blvm-node/commit/eddbe4b23f3606a993f4b8b55146a78ff0bd2a88))
- ibd: fix WAN multi-peer — 1 worker/peer + peer blacklisting ([ec7134a05fac](https://github.com/BTCDecoded/blvm-node/commit/ec7134a05facbced38da92e549057dc8b8578a42))
- feat(ibd): engine checkpoints, OOM-safe export/seed, and crash-safe UTXO flush ([2f73f42b3a6a](https://github.com/BTCDecoded/blvm-node/commit/2f73f42b3a6af6fe155f4bd2fa63c127bf98465f))
- fix(ci): refresh blvm-spec cache and apply rustfmt ([9485f2de50ca](https://github.com/BTCDecoded/blvm-node/commit/9485f2de50cab3773d9c278007a3a2650ca4afb6))
- fix(verify): annotate calculate_checksum with explicit spec name + ensures ([85c176e7dfb7](https://github.com/BTCDecoded/blvm-node/commit/85c176e7dfb70fdd84ac7b1028cd7e0466a5073f))
- style: rustfmt calculate_checksum spec_locked attributes ([648070fc5e5b](https://github.com/BTCDecoded/blvm-node/commit/648070fc5e5bec6723962a9feb0d1d53629a4d09))

## [v0.1.26] - 2026-06-04

### Reconstructed

- fix(build): gate io-uring and IBD engine file I/O for Windows cross-compile ([d27a5a4a3801](https://github.com/BTCDecoded/blvm-node/commit/d27a5a4a380128bd2e55ca39224a028b994e39c8))

## [v0.1.27] - 2026-06-04

### Reconstructed

- fix(deps): require blvm-consensus >=0.1.26 for 10-arg connect_block_ibd ([c4c892a76fac](https://github.com/BTCDecoded/blvm-node/commit/c4c892a76fac6e62291be9d536bc82664a0bd204))

## [v0.1.28] - 2026-06-06

### Reconstructed

- chore(ci): add self-hosted runner spec-lock install script ([822023f71a14](https://github.com/BTCDecoded/blvm-node/commit/822023f71a14060440de8fdd20bcef4befa3726b))
- feat(rpc): ckpool solo mining — HTTP Basic auth, GBT shape, Core hash order ([8a3a3803a288](https://github.com/BTCDecoded/blvm-node/commit/8a3a3803a288307cf5ac8b037dfbdb60b9715dbd))

## [v0.1.29] - 2026-06-07

### Reconstructed

- feat(storage): Bitcoin Core drop-in — migrate-on-start, checkpoint resume, block reuse ([f72b5217dad6](https://github.com/BTCDecoded/blvm-node/commit/f72b5217dad67f7f020276de97b1c419cc5ab729))
- feat(module): subprocess ModuleAPI IPC proxy, mesh RPC, and payment verify paths ([86a245a60ba6](https://github.com/BTCDecoded/blvm-node/commit/86a245a60ba622cc971f264150ab5c22c05b7757))
- test: raise blvm-node line coverage toward ~44.5% gate ([b9858690df7c](https://github.com/BTCDecoded/blvm-node/commit/b9858690df7c38feb2310e522673a3626343916d))
- fix(test): mesh_ipc_proxy_test uses ModuleIpcHandle after IPC refactor ([2930d8745142](https://github.com/BTCDecoded/blvm-node/commit/2930d8745142b06b6de57c730a5d6a3e2311d2f2))
- fix(rpc): create data dir before savemempool writes mempool.dat ([8d43a0bcb017](https://github.com/BTCDecoded/blvm-node/commit/8d43a0bcb0177fb13c8a59277a29bd134c898a70))

## [v0.1.30] - 2026-06-08

### Reconstructed

- feat(rpc): expose mesh hop invoice and route quote RPC ([e09f4244c881](https://github.com/BTCDecoded/blvm-node/commit/e09f4244c881167ab9088ccea88f2da455afeaec))
- refactor(rpc): remove mesh-specific core RPC handlers ([35b1393ce05d](https://github.com/BTCDecoded/blvm-node/commit/35b1393ce05d53bf2cc444e146d87f5cdb532815))

## [v0.1.31] - 2026-06-08

### Reconstructed

- docs: remove product-specific references from mesh tests and comments ([1a1104c560ac](https://github.com/BTCDecoded/blvm-node/commit/1a1104c560ac5749980e78cff9edd6e27c71d4f0))

## [v0.1.32] - 2026-06-08

### Reconstructed

- feat(module): block pipeline hook for filter_block_before_store during IBD flush ([ee4042b5318a](https://github.com/BTCDecoded/blvm-node/commit/ee4042b5318a5ca9eb85ff49302d900837a8af19))
- ci: prefer mold linker and cap test link parallelism ([48af386f0f01](https://github.com/BTCDecoded/blvm-node/commit/48af386f0f011efb2efc54bbad8a9210ed57fb32))
- ci: serialize runs and isolate per-run target dirs ([c2d5b0f8a9fc](https://github.com/BTCDecoded/blvm-node/commit/c2d5b0f8a9fc8e0fbafdd57f18977beb06be8e24))
- ci: serialize heavy jobs and fix fuzz target path ([75c3e3a5f8bd](https://github.com/BTCDecoded/blvm-node/commit/75c3e3a5f8bd30e54e960a8c21994354f7769484))
- revert(ci): undo runner disk/serialization workflow experiments ([045b3ec238ee](https://github.com/BTCDecoded/blvm-node/commit/045b3ec238ee79a6a2cba3baedfb6084eb72f836))
- test(registry): expect short method name from route_method ([e43e660c716e](https://github.com/BTCDecoded/blvm-node/commit/e43e660c716e87ff2095d4185819aba9da00733b))

## [v0.1.33] - 2026-06-08

### Reconstructed

- feat(modules): version-pinned enabled_modules with wildcard bootstrap ([bdb770c8bb00](https://github.com/BTCDecoded/blvm-node/commit/bdb770c8bb00956d64f680dc8ba84c34cb5d4b9e))

## [v0.1.34] - 2026-06-08

### Reconstructed

- feat(payment): add verifyonchainpaymentbytx for two-node path-3 verify ([dc4d283aba4c](https://github.com/BTCDecoded/blvm-node/commit/dc4d283aba4c9e59278cfaffff6d088abb46f0d2))

## [v0.1.35] - 2026-06-16

### Reconstructed

- feat(storage): heed3/LMDB default backend with rkyv zero-copy UTXO reads ([0a1df4d6b97c](https://github.com/BTCDecoded/blvm-node/commit/0a1df4d6b97cfe60b31c24ac1d1b1a008883181e))
- fix(deps): restore blvm-consensus semver range to >=0.1, <1 ([cf8c84d3b8cd](https://github.com/BTCDecoded/blvm-node/commit/cf8c84d3b8cdbe56a232d7e576ea2cb775853b24))

## [v0.1.36] - 2026-06-17

### Reconstructed

- feat(storage): heed3/LMDB default backend with rkyv zero-copy UTXO reads ([3988512db18b](https://github.com/BTCDecoded/blvm-node/commit/3988512db18b8423fccd7aa53d01bbd452c72fc6))
- fix: edition 2024 compile fixes and dependency alignment ([fb49b5b0ff9d](https://github.com/BTCDecoded/blvm-node/commit/fb49b5b0ff9d4c075ba158f0025bc2cfb6e40b0c))

## [v0.1.37] - 2026-06-17

### Reconstructed

- fix(core-import): LevelDB chainstate, mixed index safety, default block reuse ([578d2ab32ddd](https://github.com/BTCDecoded/blvm-node/commit/578d2ab32ddd87477f0f0f79fff13e51b4bbc982))
- ci(fuzz): install cargo-fuzz with +nightly to avoid stable MSRV mismatch ([073031d758f3](https://github.com/BTCDecoded/blvm-node/commit/073031d758f3ebab0301b66ba9b1325117a0d974))

## [v0.1.38] - 2026-06-17

### Reconstructed

- ci(fuzz): install cargo-fuzz with +nightly to avoid stable MSRV mismatch ([48ba2dc6d025](https://github.com/BTCDecoded/blvm-node/commit/48ba2dc6d025f893dda86a8013c285b4a4f97443))

## [v0.1.39] - 2026-06-18

### Reconstructed

- ci(fuzz): install cargo-fuzz with +nightly to avoid stable MSRV mismatch ([02eb3b2ce325](https://github.com/BTCDecoded/blvm-node/commit/02eb3b2ce325a688be2cae01d8e21520c5391d01))
- feat(node): fork choice, reorg executor, REST wiring, and operator docs ([9ee84fe6067e](https://github.com/BTCDecoded/blvm-node/commit/9ee84fe6067e9de8fad39e568193d7067e8ba765))

## [v0.1.41] - 2026-06-19

### Reconstructed

- feat(node): Signet network wiring, H08 prev-hash check, custom challenge config ([8726c3994b29](https://github.com/BTCDecoded/blvm-node/commit/8726c3994b29a868bfba65377efb5be9d8585ecc))
- test: shorten integration stress tests and skip coverage suites in default CI ([3a148a6079bd](https://github.com/BTCDecoded/blvm-node/commit/3a148a6079bd0c687481f052338ea615066a2488))
- test: seed mining RPC tests with 8 blocks instead of 2016 ([0db3aaea3560](https://github.com/BTCDecoded/blvm-node/commit/0db3aaea3560a7b2a27959050303a8d019a94d3a))
- ci: drop redundant heed3 test step [skip ci] ([88181bd7d1e5](https://github.com/BTCDecoded/blvm-node/commit/88181bd7d1e5c6fe5507ecf95e0715e8016d48d0))
- ci(release): push tags with GITHUB_TOKEN and workflows permission ([ba172c9e0f61](https://github.com/BTCDecoded/blvm-node/commit/ba172c9e0f61a13178241c6ec1dba9e76b17a5d9))
- ci(release): create tags via GitHub Release API, not git push ([708a9bf93f6c](https://github.com/BTCDecoded/blvm-node/commit/708a9bf93f6c345fe60890cc0dc52d77bdd509f3))
- Revert "ci(release): create tags via GitHub Release API, not git push" ([e711960d0aba](https://github.com/BTCDecoded/blvm-node/commit/e711960d0aba85a32d699356f09838f85fa6edd2))
- Revert "ci(release): push tags with GITHUB_TOKEN and workflows permission" ([8cd97531065d](https://github.com/BTCDecoded/blvm-node/commit/8cd97531065df148bd2c1b39b5ed69e21d931264))
- ci(release): push release tags with REPO_ACCESS_TOKEN ([af3c2b3f606a](https://github.com/BTCDecoded/blvm-node/commit/af3c2b3f606a6b6c36f531c64de183ce3a514250))

## [v0.1.42] - 2026-06-19

### Reconstructed

- fix(ibd): drain DEL tombstone backlog that wedged pending-ops cap ([3dcdb0c452d0](https://github.com/BTCDecoded/blvm-node/commit/3dcdb0c452d0a26686276721fb3989bd94c947b0))

## [v0.1.43] - 2026-06-19

### Reconstructed

- fix: address blvm#6 lower-severity IBD and RPC CLI findings ([b9eb07681b61](https://github.com/BTCDecoded/blvm-node/commit/b9eb07681b61e213eecb6002036f9e252dfdf1cd))
- ci(fuzz): verify and reinstall nightly rustc on isolated runners ([45a8c6397bba](https://github.com/BTCDecoded/blvm-node/commit/45a8c6397bbaff76e23ea274ffc1ff79f5557677))
- style: rustfmt filter_ibd_download_peers test ([0321be3172ef](https://github.com/BTCDecoded/blvm-node/commit/0321be3172efc6a4a0fbe73be0f406b0b4c9daa7))
- fix(rpc): reject invalid Bearer even when auth is optional ([fded6d8f7233](https://github.com/BTCDecoded/blvm-node/commit/fded6d8f72331a082d9b24eb2ca54424f53020ed))

## [v0.1.44] - 2026-06-19

### Reconstructed

- fix(ibd): complete validation at effective_end_height (tip off-by-one) ([5eecf744b3c1](https://github.com/BTCDecoded/blvm-node/commit/5eecf744b3c16c859e75f27f2e993b5199fbe90e))

## [v0.1.45] - 2026-06-19

### Reconstructed

- fix(ibd): unblock shutdown flush paths and satisfy clippy -D warnings ([ae281ece447b](https://github.com/BTCDecoded/blvm-node/commit/ae281ece447b6706d662942ca11a27ccf7c43c1f))
- fix(test): isolate block pipeline integration tests from global state ([42b77cddfe04](https://github.com/BTCDecoded/blvm-node/commit/42b77cddfe04ad7689f8d27b3bb4c203f203cd08))
- style: rustfmt block pipeline test isolation changes ([d098138e899b](https://github.com/BTCDecoded/blvm-node/commit/d098138e899b31100f818b28d165ead35571c390))

## [v0.1.46] - 2026-06-24

### Reconstructed

- docs: align regtest RPC default to 18443 [skip ci] ([9874306a64e1](https://github.com/BTCDecoded/blvm-node/commit/9874306a64e17615b44eb93f8599875d0249a983))
- fix(node): parallel IBD sync, post-IBD tip follow, and network/RPC fixes ([9ed2aa56b271](https://github.com/BTCDecoded/blvm-node/commit/9ed2aa56b271964ab10b2aead3dbc01a99332df0))
- fix(test): chunk assigner bootstrap gate and rustfmt ([37936e6a79e7](https://github.com/BTCDecoded/blvm-node/commit/37936e6a79e7e59f44fe9a613c596ad83bf32592))
- fix(test): align fork reorg tests with storage-derived connect height ([9ff9aa19fd1f](https://github.com/BTCDecoded/blvm-node/commit/9ff9aa19fd1f538e8ba23f3e31732e9d86fab59e))
- fix(test): deterministic block times in fork reorg mining ([9149cab140b5](https://github.com/BTCDecoded/blvm-node/commit/9149cab140b5b7cfef9c0ad59ffd1495c71a7293))
- fix(test): regtest_mine_sync storage-derived connect height ([3a751a650b19](https://github.com/BTCDecoded/blvm-node/commit/3a751a650b196e5ec4ec5a8f65791859eaac68d1))
- fix(test): align sync_coordinator valid-block test with connect height ([bd739b99b66b](https://github.com/BTCDecoded/blvm-node/commit/bd739b99b66beb567ba1b9eab4fa9b4a0233a4ed))

## [v0.1.47] - 2026-06-24

### Reconstructed

- fix(security): bump quinn-proto git patch to 0.11.15 ([d1da9746a6e8](https://github.com/BTCDecoded/blvm-node/commit/d1da9746a6e804a25d7196762ad7b33ddd8668c7))

## [v0.1.48] - 2026-06-24

### Reconstructed

- fix(windows): gate libc sysconf calls behind unix + libc feature ([f09a464aeb9f](https://github.com/BTCDecoded/blvm-node/commit/f09a464aeb9f51267d21f304a5c29365bd0ee93b))

## [v0.1.49] - 2026-06-24

### Reconstructed

- fix(ibd): seed BIP113 recent headers from blockstore at validation start ([ac6c91ec92df](https://github.com/BTCDecoded/blvm-node/commit/ac6c91ec92dfddddbc9ee4e1b247f3e794ef11d9))

## [v0.1.50] - 2026-06-24

### Reconstructed

- fix(ibd): unblock single-chunk retry when peer exclude deadlocks workers ([a85049798c5c](https://github.com/BTCDecoded/blvm-node/commit/a85049798c5c8b16dc609a6cc2bb0382c7d485b5))

## [v0.1.51] - 2026-06-24

### Reconstructed

- fix(ibd): resume gap downloads and wait for peer reconnect ([84a0f740efd7](https://github.com/BTCDecoded/blvm-node/commit/84a0f740efd7ed240c15e052085c6ff584be5b44))
- fix(ibd): assign next-height chunk when max_ahead is zero ([8c47c88dc337](https://github.com/BTCDecoded/blvm-node/commit/8c47c88dc337cf3495f922bb2acdc473bb8a02e7))
- fix(ibd): avoid mutex self-deadlock in chunk assigner get_work ([9277be533c9a](https://github.com/BTCDecoded/blvm-node/commit/9277be533c9aceb2351f0d2ac097ec26524db851))
- chore: satisfy cargo fmt and clippy -D warnings on blvm-node ([e961d3d65c7d](https://github.com/BTCDecoded/blvm-node/commit/e961d3d65c7d827ff24d26ee0dfa732a8742dcce))

## [v0.1.52] - 2026-06-25

### Reconstructed

- fix(release): align defaults and fix Windows dep resolution ([b5d9aaefbf22](https://github.com/BTCDecoded/blvm-node/commit/b5d9aaefbf22b33c0c530f64660b363ce9a54434))
- fix(ci): pin ed25519/pkcs8 for iroh defaults and restore nix/libc ([8dd93686f48a](https://github.com/BTCDecoded/blvm-node/commit/8dd93686f48a0ecd1996b7c773a6b30db8ea9964))
- fix(ci): satisfy clippy and tests for expanded default features ([3dc763cfff56](https://github.com/BTCDecoded/blvm-node/commit/3dc763cfff561732de268e46abfdd80d70d8fad7))
- fix(ci): repair dandelion tests and iroh roundtrip on defaults ([21d85db81934](https://github.com/BTCDecoded/blvm-node/commit/21d85db819340dac9fda3181fdf2446a43b3302b))
- fix(test): REST address tests accept base58 on empty chain ([ba0151436107](https://github.com/BTCDecoded/blvm-node/commit/ba01514361079da437e6c7fde70c12c9f1015e31))

## [v0.1.53] - 2026-06-26

### Reconstructed

- fix(ibd): recover parallel download gaps without multi-minute stalls ([4c53d16f712b](https://github.com/BTCDecoded/blvm-node/commit/4c53d16f712b63670c0b06a8f00a23b669fc19c6))

## [v0.1.54] - 2026-08-19

### Reconstructed

- fix(ibd): RSS envelope + cut hot-path allocations ([7c0644cab66b](https://github.com/BTCDecoded/blvm-node/commit/7c0644cab66b0134fa5e34a55861299644278d2d))
- peel: P2P addr/address_db and tiny IBD signal/sync/outbound ([27decf064129](https://github.com/BTCDecoded/blvm-node/commit/27decf064129759a0ce54f7d0f0cd9f84ce548bf))
- dens-keep-ibd: park parallel IBD and mixed P2P leftover ([1a16d6bb4d97](https://github.com/BTCDecoded/blvm-node/commit/1a16d6bb4d97ca92bb9d57b17748f92a0724e876))
- dens-keep-engine: park IBD engine, heed3, and ckpt ops leftover ([dead6ec60ffb](https://github.com/BTCDecoded/blvm-node/commit/dead6ec60ffbe6ab0778cdd6fa35db68a3c64d66))
- dens-keep: T4 IBD splits, ibd-dev gate, and fuzz CUDA opt-out. ([e9fe5389450b](https://github.com/BTCDecoded/blvm-node/commit/e9fe5389450ba555e6d0311426594ae1d85303bf))
- fix(ci): rustfmt rawtx/strip_bodies and skip CUDA on Test. ([88efbb2eb725](https://github.com/BTCDecoded/blvm-node/commit/88efbb2eb72525d69a2825a16c00a80cb39aee55))
- chore(ci): retrigger after moving container images to /mnt/bulk. ([7d6dd525fd30](https://github.com/BTCDecoded/blvm-node/commit/7d6dd525fd30a68230d4d33937a4a7ae16cd2a41))

## [v0.1.55] - 2026-09-22

### Reconstructed

- No non-merge commits in this tag range.

## [v0.1.56] - 2026-10-05

### Reconstructed

- perf(ibd): window assigner, UTXO engine on-disk index, pipelined validator ([c0579ee04271](https://github.com/BTCDecoded/blvm-node/commit/c0579ee04271b9d8ebff2d4ecebadea242e43259))
- feat(mining): Commons Pool GBT wiring and declared-txid templates ([08a48068a1dd](https://github.com/BTCDecoded/blvm-node/commit/08a48068a1dd7860097c062e73c6a43268fb5d5e))
- feat(mempool): sigop-adjusted vsize and UnexecIf policy knob ([fde7e501db1c](https://github.com/BTCDecoded/blvm-node/commit/fde7e501db1cba6372665b97dcc70e5d5f61c5cf))
- ibd: tip-sync thread off the orchestrator (R-362); exact sub-ms wall timers (R-363) ([69b3e591afdb](https://github.com/BTCDecoded/blvm-node/commit/69b3e591afdbf2ebe58d35114f9ac2e12eca235c))
- ibd: drop flag-off hash_fetch and lock the H+1 assigner ([038793b19e7c](https://github.com/BTCDecoded/blvm-node/commit/038793b19e7cbc78e9c037e18a3653d1ed063d7c))
- fix(ibd): BIP113 MTP window tracks dispatch, not drain (R-365) ([1d12241013ef](https://github.com/BTCDecoded/blvm-node/commit/1d12241013ef8252ba8f9bed65377916f321c312))
- ibd: young fan-in and a checkpoint tee that snapshots before unlock ([c9d847e175e5](https://github.com/BTCDecoded/blvm-node/commit/c9d847e175e5cc13ee9f6fb469f6ad60fd38c163))
- feat(module): serve a block and its witnesses over module IPC ([276ee8dfbf4d](https://github.com/BTCDecoded/blvm-node/commit/276ee8dfbf4d333cb9803e380d7feae8c6e2a667))
- fix(net): answer ban-list messages only when sharing is configured ([210675271fc6](https://github.com/BTCDecoded/blvm-node/commit/210675271fc6c5e942b7db7d3542d9b1b321fc72))
- test(ibd): serialize body-dup counter snapshots ([8af3db620af4](https://github.com/BTCDecoded/blvm-node/commit/8af3db620af4ef3606e95d4610f49918739e4668))
- chore(node): rustfmt leftover hunks ([95cf3334657b](https://github.com/BTCDecoded/blvm-node/commit/95cf3334657b65fe8842c12c00b578b96a751c67))
- fix(ibd): rustfmt the checkpoint brace and call jemalloc only where it is linked ([c720e6e9fb0f](https://github.com/BTCDecoded/blvm-node/commit/c720e6e9fb0f441be8c224849fdb0b3fb8825785))
- fix(rpc): export SegWit transaction bytes and witness ids ([e054cc10d885](https://github.com/BTCDecoded/blvm-node/commit/e054cc10d885208c9d8247523ca2f359ebf87380))
- fix(rpc): show transaction ids in BIP145 display order ([b1cc2771e263](https://github.com/BTCDecoded/blvm-node/commit/b1cc2771e2636eb60583b78d7163e9e0b9d59132))
- fix(ibd): keep delivered hole heights hidden while the tip is uncovered ([8a45c36f0c4e](https://github.com/BTCDecoded/blvm-node/commit/8a45c36f0c4e9a369b418d9c62424f04471d1c45))
- chore(node): rustfmt the scored tree and clear clippy doc warnings ([f02fc09d66c3](https://github.com/BTCDecoded/blvm-node/commit/f02fc09d66c3e463a91d7c36ad9048f491862324))
- test(ibd): expect the scored front duplicate, not a one-block peel ([a7edd6843474](https://github.com/BTCDecoded/blvm-node/commit/a7edd6843474792443b45165f02b6b5f92874da5))
- test(ibd): serialize probe-table tests on the ibd lock ([d6af8ee31c25](https://github.com/BTCDecoded/blvm-node/commit/d6af8ee31c255fec456826a3b59e352ef14d1c60))

## [v0.1.57] - 2026-10-05

### Reconstructed

- fix(node): compile the retire loop and supply relative-lock medians ([80f123c9d53e](https://github.com/BTCDecoded/blvm-node/commit/80f123c9d53e007e75ac94f38d409d7d1f2a2af3))
- fix(node): keep the duplicate-coinbase index until the height-in-coinbase block ([6e9631fc2c93](https://github.com/BTCDecoded/blvm-node/commit/6e9631fc2c9314a7cb8b34044595d04cd84a8126))
- fix(miner): give the coinbase the witness-commitment nonce ([2d78eafcab7a](https://github.com/BTCDecoded/blvm-node/commit/2d78eafcab7a74b967f5d98f1535c5f3ff030c7e))
- fix(miner): expect the coinbase reserved nonce in the witness test ([53cbc9c465e5](https://github.com/BTCDecoded/blvm-node/commit/53cbc9c465e51ff43fbde4da7b004658d75d197b))
- fix(miner): a same-second regtest block must be later than the median ([589df38f17f7](https://github.com/BTCDecoded/blvm-node/commit/589df38f17f7563fcfed63253c5e9fa1c53a9bf2))
- fix(miner): regtest coinbase carries the commitment nonce ([b592aac6a9ac](https://github.com/BTCDecoded/blvm-node/commit/b592aac6a9ace4077feb84787979f2085b879f5b))
- test(signet): validate genesis before storing its header ([8038268289e1](https://github.com/BTCDecoded/blvm-node/commit/8038268289e19940ae69c4ddddde9a5ebb5c3758))

## [v0.1.58] - 2026-10-05

### Reconstructed

- No non-merge commits in this tag range.

## [v0.1.59] - 2026-10-06

### Reconstructed

- fix(node): check required bits against stored headers ([c08d7b72e15c](https://github.com/BTCDecoded/blvm-node/commit/c08d7b72e15ceba74cc87a1121177088c6f1f4d7))
- style(node): format the difficulty-ancestor lookup ([9394bb11369f](https://github.com/BTCDecoded/blvm-node/commit/9394bb11369fdf97edfcda100614908cb6d69795))
- fix(ci): use REPO_ACCESS_TOKEN for release tags when workflows change ([649cd49dd11c](https://github.com/BTCDecoded/blvm-node/commit/649cd49dd11c04dae8e7eaf0af6632d888ce4065))
- test(miner): index the genesis header before connecting a mined block ([d0a123e32af1](https://github.com/BTCDecoded/blvm-node/commit/d0a123e32af111a9673c741697bc6315c384c596))
- style(miner): wrap the genesis header store ([883499366529](https://github.com/BTCDecoded/blvm-node/commit/883499366529e55222db4c1aba096b280609d2f4))
- refactor(ibd): name the heaviest parallel IBD stages ([6221d9a10aa4](https://github.com/BTCDecoded/blvm-node/commit/6221d9a10aa4dae2ff1d90eca849d4f1eb639f80))
- ci: add early disk cleanup to Test and Fuzz jobs ([c069bb8341f0](https://github.com/BTCDecoded/blvm-node/commit/c069bb8341f0dca6995dc5b3d154c0c64270301c))
- perf(ibd): reuse append and cold-lookup buffers ([c99ba098c6e2](https://github.com/BTCDecoded/blvm-node/commit/c99ba098c6e20cbf28420cad88611766e794510a))
- fix(mining): index genesis before regtest generatetoaddress ([d212f7a804b6](https://github.com/BTCDecoded/blvm-node/commit/d212f7a804b69bbb834a7c266c672cd9606432ed))
- test(mining): match getblocktemplate height and mainnet rules ([4898d8f29847](https://github.com/BTCDecoded/blvm-node/commit/4898d8f2984702d0678947e74f192ceb8f23df98))

## [v0.1.60] - 2026-10-06

### Reconstructed

- fix(rpc): make ping RPC actually send pings to all peers ([5952c93aca15](https://github.com/BTCDecoded/blvm-node/commit/5952c93aca15e5c8948343db1c237bbe33392252))
- style: fix formatting ([d6490ecb98bb](https://github.com/BTCDecoded/blvm-node/commit/d6490ecb98bb909397f25bcba095007e9d3606bc))

## [v0.1.61] - 2026-10-06

### Reconstructed

- fix(rpc): verifychain false failures on chains with spends ([68d77989227d](https://github.com/BTCDecoded/blvm-node/commit/68d77989227d145ce205d9bae9652440363e902b))
- Fix verifychain level 4: consensus disconnect semantics + per-block MTP ([c6c33d4f85cd](https://github.com/BTCDecoded/blvm-node/commit/c6c33d4f85cdc2beec7b0f3c88dddbaa898d0620))
- Fix formatting (trailing space in doc comment + rustfmt) ([9f94ed9cf6d5](https://github.com/BTCDecoded/blvm-node/commit/9f94ed9cf6d5a1048de27b5ba82daf8f77b808b9))
- Fix level 4 rewind: one-pass-per-entry disconnect semantics ([86f8ce02809e](https://github.com/BTCDecoded/blvm-node/commit/86f8ce02809e46112f0c132a6b66df2ed1ec128f))
- Fix: revert Cargo.toml to main, remove trailing space in docstring ([10e608e30c97](https://github.com/BTCDecoded/blvm-node/commit/10e608e30c97f172c1d08a06109af1fd21830177))

## [v0.1.62] - 2026-10-07

### Reconstructed

- feat(node): resume IBD from the canonical UTXO snapshot ([0fb7c298c42a](https://github.com/BTCDecoded/blvm-node/commit/0fb7c298c42a8ecde091ecd1875e62f2df8d116c))
- style: rustfmt the testnet4 and resume changes so CI fmt --check passes ([ed36bc3ad746](https://github.com/BTCDecoded/blvm-node/commit/ed36bc3ad746b3877d521c9f3173dc7836b48925))
- fix(node): encode mined BIP34 heights 1 through 16 as OP_1 through OP_16 ([665308365002](https://github.com/BTCDecoded/blvm-node/commit/665308365002910d1d0ed6bc90e3f2d1c1a0f2de))
- fix(node): use OP_1 for the height-1 coinbase in verifychain tests ([8c25ab56aa17](https://github.com/BTCDecoded/blvm-node/commit/8c25ab56aa17047ba79b0ba462ecdcd60ef745db))

## [v0.1.63] - 2026-10-07

### Reconstructed

- fix(detection): add testnet4 arms to detect_network and testnet3 alias ([ed52660ee064](https://github.com/BTCDecoded/blvm-node/commit/ed52660ee064e6b295a9d330e92277b0b741d238))

## [v0.1.64] - 2026-10-08

### Reconstructed

- fix(node): reorg on the real fork and use the network subsidy ([cc4867f085ef](https://github.com/BTCDecoded/blvm-node/commit/cc4867f085efce3b588d0c3677c5ab9a396e5ba4))
- test(node): encode the ckpool submitblock coinbase for the chain height ([70fdb5f3dcdd](https://github.com/BTCDecoded/blvm-node/commit/70fdb5f3dcdde60841e1094df97de6b6aaa07c0c))
- test(node): signet genesis leaves the UTXO set empty ([00618edd4add](https://github.com/BTCDecoded/blvm-node/commit/00618edd4addc76c182ed74106a0bff524093259))
- fix(storage): register the block_undo table on the redb backend ([de5dac8a74f6](https://github.com/BTCDecoded/blvm-node/commit/de5dac8a74f63a088bca3bf373f51ebc4451dc27))

## [v0.1.65] - 2026-10-10

### Reconstructed

- fix(node): relay witness transactions with their stacks ([0085f26f0c6d](https://github.com/BTCDecoded/blvm-node/commit/0085f26f0c6daee723066aa284086776f618ae4c))
- fix(node): finish the mempool, relay, and RPC behavior the plan locked ([5b3c0d136bbf](https://github.com/BTCDecoded/blvm-node/commit/5b3c0d136bbf681811032c3f79c7334badf81221))
- fix(node): format the relay changes and satisfy clippy ([c6376bf3215d](https://github.com/BTCDecoded/blvm-node/commit/c6376bf3215df4fb45a6fbfa5f90559c89a7bdcc))
- test(node): key a merged ban by address with port zero ([a6e32f43006a](https://github.com/BTCDecoded/blvm-node/commit/a6e32f43006a3f3adfe74155b2abd1dbb4ac54aa))
- fix(node): submit a mined block at the next height ([78bdc22c9ed9](https://github.com/BTCDecoded/blvm-node/commit/78bdc22c9ed995739c31255f08f9bfa96aa3da27))
- test(node): expect the block template at the chain tip plus one ([dcbeafd19c52](https://github.com/BTCDecoded/blvm-node/commit/dcbeafd19c52ad3ffeee7919f20db823c3233814))
- test(node): look up the template transaction with the RPC hash ([5b9c905ecc53](https://github.com/BTCDecoded/blvm-node/commit/5b9c905ecc53b56d3b042b948c7c71ff9fb0b9c1))
- test(node): query address UTXOs with the address index on ([81e20b8e8826](https://github.com/BTCDecoded/blvm-node/commit/81e20b8e882609186dfb13d374bd26852ace9c86))
- test(node): submit a block this node already mined onto genesis ([39025912a410](https://github.com/BTCDecoded/blvm-node/commit/39025912a410146320815ae1a05c71702a3459c8))

## [v0.1.66] - 2026-10-10

### Reconstructed

- fix(node): keep the tip, honor config, and close the next hunt ([efa0e33a4858](https://github.com/BTCDecoded/blvm-node/commit/efa0e33a48589e2d1a79bb69b1560f6dc5d95228))
- fix(node): stop disconnecting valid relay and honor peer asks ([7750a393d5ab](https://github.com/BTCDecoded/blvm-node/commit/7750a393d5ab127a1eee2bc49b4ac5d71ab44650))
- fix(node): format for CI rustfmt and clear the package fee bump ([a9132b09d030](https://github.com/BTCDecoded/blvm-node/commit/a9132b09d030df899d1ed60f9923e0d3903abdf5))
- fix(node): keep getblocks bodies and isolate append diagnostics ([5685eae75022](https://github.com/BTCDecoded/blvm-node/commit/5685eae750228e0a3b8a2e3f18433ef1fa772393))

## [v0.1.67] - 2026-10-11

### Reconstructed

- fix(node): cap mempool descendants and mine a block that connects ([8cb87db7df77](https://github.com/BTCDecoded/blvm-node/commit/8cb87db7df779997bb4278080e84ed1d6d2578f8))

## [v0.1.68] - 2026-10-11

### Reconstructed

- fix(node): follow a reorg, relay witnesses, and keep the active height ([5b93c95a9da2](https://github.com/BTCDecoded/blvm-node/commit/5b93c95a9da24924b18bbd4447030b08732ac0f5))
- fix(node): store the UTXO that was written ([262cfd5cab33](https://github.com/BTCDecoded/blvm-node/commit/262cfd5cab3396c3d77f25db6867622625d4bdad))
- test(node): a dust output stays out of the mempool ([e0c755bcd60d](https://github.com/BTCDecoded/blvm-node/commit/e0c755bcd60d3f419f46f9c7e4f146b7a34afc7b))

## [v0.1.69] - 2026-10-11

### Reconstructed

- fix(node): admit a future witness spend when the payload is true ([e39e82a1abd1](https://github.com/BTCDecoded/blvm-node/commit/e39e82a1abd1c5e26089bbfc0280a00edbb2a637))

## [v0.1.70] - 2026-10-15

### Reconstructed

- audit: document quinn-proto RUSTSEC-2026-0338..0342 ignores (fix blocked by iroh 0.95 rc crypto) ([aca5b2189039](https://github.com/BTCDecoded/blvm-node/commit/aca5b2189039b3df041ae175630a1d1f1e7a1bd8))
- fix(module): flush decrypted binary before returning in store_decrypted_module ([3c52be804a1a](https://github.com/BTCDecoded/blvm-node/commit/3c52be804a1af609929ff0ef0578d4de4d485a2f))

## [v0.1.71] - 2026-10-15

### Reconstructed

- fix(storage): register the block_index table on the redb backend ([b117c2687f84](https://github.com/BTCDecoded/blvm-node/commit/b117c2687f848c88a2d6639f2b23ed506015820e))

## [v0.1.72] - 2026-10-15

### Reconstructed

- fix(storage): detect signet_<hash> directories and respect network arg ([53332f56c45b](https://github.com/BTCDecoded/blvm-node/commit/53332f56c45b66a99c27b617e863f0d2e710532f))
- fix(storage): use detect_network_strict in Core block reuse path ([2bdd1d15c508](https://github.com/BTCDecoded/blvm-node/commit/2bdd1d15c508fc2d1ca0e449a73a61cb19d8b891))
- fix(storage): cross-check the Core reuse network against marker, folder and node ([8487dbd7bc43](https://github.com/BTCDecoded/blvm-node/commit/8487dbd7bc431d9fab6c806a750fb38c302d15db))
- ci: run the test suite with --features rocksdb ([b79ba28b089f](https://github.com/BTCDecoded/blvm-node/commit/b79ba28b089f5de5ca289b5c18744b1d1413c920))
- test(ibd): serialise h-slow GetData latch reset with rotate-once tests ([6147bf618694](https://github.com/BTCDecoded/blvm-node/commit/6147bf61869499552ce806ecc994bf9fd0151983))

## [v0.1.73] - 2026-10-18

### Reconstructed

- test(node): require the result the harness used to skip ([cfdd4dfc7b8e](https://github.com/BTCDecoded/blvm-node/commit/cfdd4dfc7b8e744fcdf61c7373e24fec854b04e2))
- fix(rpc): push the witness program length ([f90ba4ba7e73](https://github.com/BTCDecoded/blvm-node/commit/f90ba4ba7e737a505fb1c2bbd20d6e4703b8cf95))
- ci(node): install the current spec-lock before verify ([8f4aba5fe46b](https://github.com/BTCDecoded/blvm-node/commit/8f4aba5fe46b5667080cc29b6e6c2bb5a701dbf6))

## [v0.1.74] - 2026-10-19

### Reconstructed

- fix(node): close the open RPC, payment, and module QA locks ([7c73c23b71b5](https://github.com/BTCDecoded/blvm-node/commit/7c73c23b71b5e475779afb8fe3a4b22c390ed61a))
- test(node): give tip updates a parent chainwork ([d8c9be5b8b4a](https://github.com/BTCDecoded/blvm-node/commit/d8c9be5b8b4aa1ebb3d59c575be7dcbd6baf649e))
- test(node): seed parent chainwork before a fabricated tip ([3e372cdf77ad](https://github.com/BTCDecoded/blvm-node/commit/3e372cdf77adc51b3e7d3a9cac9bc743e604d90f))
- ci(node): let rustup install beside the distro rustc ([b4243cf08222](https://github.com/BTCDecoded/blvm-node/commit/b4243cf08222ce9b24fcf7b81549be7b16d8cda9))
- style(node): satisfy clippy on the payment and RPC edits ([d5a3fd9a7be3](https://github.com/BTCDecoded/blvm-node/commit/d5a3fd9a7be38b14a1a2abd091957621c94fae3b))
- test(node): allow a new file inside the module sandbox ([a925ed6686b9](https://github.com/BTCDecoded/blvm-node/commit/a925ed6686b97179b4d863f0b4c12739050dab07))
- test(node): an empty pool and a missing event manager are errors ([028c3c974862](https://github.com/BTCDecoded/blvm-node/commit/028c3c97486261e5446e4b8e3e4c7c94109cec63))
- test(node): pass payment txids in RPC display order ([1a23b845c21a](https://github.com/BTCDecoded/blvm-node/commit/1a23b845c21a3fc79c48a20b83e654eea9082a06))
- test(node): empty chain REST matches the index and tip locks ([892af064d841](https://github.com/BTCDecoded/blvm-node/commit/892af064d8415e4e8a65b9afb5cb0e4659469652))
- test(node): logging is active only at debug or trace ([b3c5344e5cb7](https://github.com/BTCDecoded/blvm-node/commit/b3c5344e5cb7cce23dc0024f9c2f1124c4391146))
- test(node): getblockcount without storage is an error ([824e00b9e634](https://github.com/BTCDecoded/blvm-node/commit/824e00b9e634b1266e3c7bb8762e0c09c6b72121))

## [v0.1.75] - 2026-10-21

### Reconstructed

- docs: match getblockchaininfo and getpruneinfo examples to the RPC objects. ([f82d8c4a5299](https://github.com/BTCDecoded/blvm-node/commit/f82d8c4a529981d4fd69f74f9a5593a5bd404f63))
- fix(node): close the confirmed hunt locks for RPC, mempool, and headers ([8fcda11aa0e9](https://github.com/BTCDecoded/blvm-node/commit/8fcda11aa0e94fcf2933e029ff90a58b641c628a))
- style(node): format the hunt-lock sources ([73005d47e575](https://github.com/BTCDecoded/blvm-node/commit/73005d47e5754d34adaeed0426a7862c199c9d1f))
- fix(node): drop same-type casts of header bits ([5742d1d509a8](https://github.com/BTCDecoded/blvm-node/commit/5742d1d509a83252fdf799641b2de33e03f16b67))
- fix(node): keep the hunt locks inside the existing tests ([f1b9cfaf922a](https://github.com/BTCDecoded/blvm-node/commit/f1b9cfaf922a7ccdfe5cc3d7e7731c037344b2ce))
- test(node): finish the handshake before the block arrives ([42fc025aa116](https://github.com/BTCDecoded/blvm-node/commit/42fc025aa1166d3767b37662ff4d3b659dac705c))
- fix(node): report an uninitialized chain before the parent check ([71b2ada12e34](https://github.com/BTCDecoded/blvm-node/commit/71b2ada12e34616eda63aee013c7089cf38d3ddf))
- fix(node): resolve the rocksdb block-cache helper from its parent module ([d9c9b37e980b](https://github.com/BTCDecoded/blvm-node/commit/d9c9b37e980b68e256726d1b035a63102723ba19))

## [v0.1.76] - 2026-10-21

### Reconstructed

- fix(node): cap RPC bodies and mempool witness bytes, and report chain errors honestly ([a6944655e19c](https://github.com/BTCDecoded/blvm-node/commit/a6944655e19c33979e14e41fbcd4efa0982016a7))
- fix(node): reject only a transaction larger than the mempool cap ([cb58cc4e333b](https://github.com/BTCDecoded/blvm-node/commit/cb58cc4e333b33cd18f8f2f752da41229874d614))
- fix(node): expect getblockchaininfo to error before a chain is loaded ([4f1be96a65f8](https://github.com/BTCDecoded/blvm-node/commit/4f1be96a65f8980d9d3f149a4d17182df8a0c4f9))
- style(node): format the uninitialized-chain assertion ([91c165524655](https://github.com/BTCDecoded/blvm-node/commit/91c1655246551a0a415e92defef5f84771098437))

## [v0.1.77] - 2026-10-22

### Reconstructed

- fix(module): gate WASM CLI-spec registration on unix; fix storage test types ([e34174ec0a1b](https://github.com/BTCDecoded/blvm-node/commit/e34174ec0a1b06ab22d6c923e60380389fd20956))
- test(payment): use utils::new_request_id instead of the removed uuid crate ([12c326440b4d](https://github.com/BTCDecoded/blvm-node/commit/12c326440b4def5261b1e93f671ce100604b3fe5))
- style: rustfmt import order in payment_rest_integration_tests ([62daa4aa8594](https://github.com/BTCDecoded/blvm-node/commit/62daa4aa8594a5a37dd259ec51ae5b252e7c1d65))
- fix(rpc): refill the RPC rate limiter from a monotonic clock ([d5079c8d1ba5](https://github.com/BTCDecoded/blvm-node/commit/d5079c8d1ba5ea8b310908ce5e485d5771ce7fd6))
- docs: stop calling the consensus mining functions formally verified. ([7ade033ea801](https://github.com/BTCDecoded/blvm-node/commit/7ade033ea801e3419ab1d24248fc37401e05a967))
- ci: guard Build job against low disk before restoring the target cache. ([da8ab260afde](https://github.com/BTCDecoded/blvm-node/commit/da8ab260afde1a720b8386834f65b4ebd2d055af))
- test(rpc): pin limiter no-banking while full and remainder drop on fill. ([b86f56aac1e9](https://github.com/BTCDecoded/blvm-node/commit/b86f56aac1e9822c27293be1d72970965e1e2563))
- test(rest): send a full vault config so the unvault test creates its vault. ([129ad0d87627](https://github.com/BTCDecoded/blvm-node/commit/129ad0d87627055654c793fdc1ce4e0d0017c8e6))
- ci: stop cleanup steps from deleting concurrent runs' build dirs. ([978bdb5a2a01](https://github.com/BTCDecoded/blvm-node/commit/978bdb5a2a01340ab9ddb76770debe322fca5387))
- fix(rpc): report no tip on submitblock and run the ckpool payout test on mainnet. ([09a652694107](https://github.com/BTCDecoded/blvm-node/commit/09a652694107df6be30780fb0ee019e8f4a81b18))
- fix(rpc): take validateaddress network from the stored chain and check base58 versions ([21ac13b5e95b](https://github.com/BTCDecoded/blvm-node/commit/21ac13b5e95b8125c79e03ca7554acd3264706bc))
- fix(storage): import rocksdb_block_cache_mb into rocksdb_impl ([7fdae4676530](https://github.com/BTCDecoded/blvm-node/commit/7fdae46765300234e9de0d5a29a5ac6455e3423d))
- chore(audit): ignore RUSTSEC-2026-0381/0383 until upstreams leave webpki 0.26 ([26c4d5e6c106](https://github.com/BTCDecoded/blvm-node/commit/26c4d5e6c106dbedc560081207cf25b08fcb47ee))
- test(rpc): cover testnet3, signet and testnet4 in validateaddress network tests ([5f0f61cd34dc](https://github.com/BTCDecoded/blvm-node/commit/5f0f61cd34dc80d2442e048bd6c634e0c27b2e1f))
- merge: take main's submitblock no-tip check in mining.rs ([e7d41e2f88e4](https://github.com/BTCDecoded/blvm-node/commit/e7d41e2f88e498d2d1df375c81eeb99f42eb8544))
- test(node): keep getblockchaininfo softforks coverage on an initialized regtest chain ([4ed680ff7cae](https://github.com/BTCDecoded/blvm-node/commit/4ed680ff7caeb9a5b5a55eb24328fc58908d14a2))
- fix(node): check address networks whenever the chain is known ([cafc63c599e8](https://github.com/BTCDecoded/blvm-node/commit/cafc63c599e8b9af7b7579ff383b3237c04e3d8e))
- fix(rpc): validateaddress fails closed when the chain is unknown ([74e2c8a67c33](https://github.com/BTCDecoded/blvm-node/commit/74e2c8a67c33e8001c42eec5a484cc4712555bbc))

## [v0.1.78] - 2026-10-22

### Reconstructed

- chore: sync package version with crates.io (0.1.73); commit Release version bump ([c38baa9d609f](https://github.com/BTCDecoded/blvm-node/commit/c38baa9d609f86dd64b7dfa6c7d7857685081254))
- ci: run PR checks ([482e0ca33a2a](https://github.com/BTCDecoded/blvm-node/commit/482e0ca33a2a012b8a66c5bfc9197816dc44a829))
- Bump version to 0.1.78 for release [skip ci] ([c002807951ad](https://github.com/BTCDecoded/blvm-node/commit/c002807951ad501f46fec95a05114b7397c379f2))

## [v0.1.79] - 2026-10-22

### Reconstructed

- fix(network): use Bitcoin Core's default signet magic 0a03cf40 ([d8a3467f9aba](https://github.com/BTCDecoded/blvm-node/commit/d8a3467f9abadcd569e0705c40177e791b6002ab))
- Bump version to 0.1.79 for release [skip ci] ([e17062905f10](https://github.com/BTCDecoded/blvm-node/commit/e17062905f104093c53f77366bafb790cf22b6d1))

## [v0.1.80-unverified] - 2026-10-24

### Reconstructed

- fix(node): reject a bad peer frame and keep default relay inside standard policy ([d9c153c77157](https://github.com/BTCDecoded/blvm-node/commit/d9c153c77157262a03734024a89c877cbdd33f0a))
- fix(node): borrow scripts for mempool checks and compact filters ([eddbe74d18d9](https://github.com/BTCDecoded/blvm-node/commit/eddbe74d18d9c42128e44e955261e45db8ceda91))
- fix(node): format the filter and blockstore paths ([9525201a19f1](https://github.com/BTCDecoded/blvm-node/commit/9525201a19f1baea6f7d293db1105fed61c6f674))
- fix(node): derive filter spans and build the GCM nonce by value ([f4130b5da476](https://github.com/BTCDecoded/blvm-node/commit/f4130b5da47617b0070c4492643d68f838b7b9a1))
- fix(node): admit a standard output in the mempool storage tests ([5ae8a95bc9bc](https://github.com/BTCDecoded/blvm-node/commit/5ae8a95bc9bcca3b302bf89e2b79b8662cf7f304))
- fix(node): admit a standard output in the mining mempool tests ([105648386176](https://github.com/BTCDecoded/blvm-node/commit/105648386176337cf890a843fc771751503cf66f))
- fix(node): admit a standard output in the block template tests ([8f0f6f797a95](https://github.com/BTCDecoded/blvm-node/commit/8f0f6f797a951542494a3e541229166acd6ac681))
- fix(node): let the RBF fixtures spend a bare OP_1 ([3d6655d166af](https://github.com/BTCDecoded/blvm-node/commit/3d6655d166af1dc0272cb1e1b14d4966bde603bb))
- Bump version to 0.1.80 for release [skip ci] ([19fe42d58ab5](https://github.com/BTCDecoded/blvm-node/commit/19fe42d58ab55a73c021806533b21447a98869bd))

## [v0.1.81] - 2026-10-29

### Reconstructed

- fix(node): keep the three default modules when a config table is present ([0cadb229632d](https://github.com/BTCDecoded/blvm-node/commit/0cadb229632d40ea29468340b9881b36521589b7))
- Move the payment state machine into blvm-payment. ([9cdcd299d421](https://github.com/BTCDecoded/blvm-node/commit/9cdcd299d421b4160831c59f691e0e07a1760a3f))
- Test assume_valid_hash display hex and byte arrays. ([22ef7c2ee7b2](https://github.com/BTCDecoded/blvm-node/commit/22ef7c2ee7b2d0095d974cdb3845b63415bc3b56))
- Bridge missing parent chainwork before advancing the tip. ([5039c6caead1](https://github.com/BTCDecoded/blvm-node/commit/5039c6caead16e7303ebf0ef2a2fab7861d43d23))
- Latch Shared when startup swap is under 15% free. ([a4b26fec6f85](https://github.com/BTCDecoded/blvm-node/commit/a4b26fec6f85e102d913056826a0508c06c96a19))
- Bump zstd to 0.14 for RUSTSEC-2026-0391, -0398 and -0403 ([3181f75ac6f4](https://github.com/BTCDecoded/blvm-node/commit/3181f75ac6f4a7a3a4b56a50dd5b84c877f967c7))
- Run cargo fmt on main after 22ef7c2 ([c8f32133ba87](https://github.com/BTCDecoded/blvm-node/commit/c8f32133ba871b27191fcffd135df124193b2fcb))
- Keep a caught-up node on the public tip. ([5340fac95c97](https://github.com/BTCDecoded/blvm-node/commit/5340fac95c97d0736f6f38cda470ce7aa809f914))
- style(node): cargo fmt after the tip and undo repair ([59367bae6051](https://github.com/BTCDecoded/blvm-node/commit/59367bae605169d369df6c5766137718a2f54a43))
- fix(node): keep a cooled peer on the reconnect queue ([2d82337e8513](https://github.com/BTCDecoded/blvm-node/commit/2d82337e85131e3d32b0ed8cc04c929f6b1ee7d5))
- Bump version to 0.1.81 for release [skip ci] ([e9f256bfa430](https://github.com/BTCDecoded/blvm-node/commit/e9f256bfa4307e0c744dc396e97a794e91cbceee))

## [v0.1.82] - 2026-10-30

### Reconstructed

- fix(node): read chain coins from the checkpoint and stop repeating the full scan ([7dd7eec30e43](https://github.com/BTCDecoded/blvm-node/commit/7dd7eec30e430d803ca2ba8079856a6463b1c9ec))
- fix(node): reorg the chainstate table when no checkpoint is present ([a83c166250d3](https://github.com/BTCDecoded/blvm-node/commit/a83c166250d308b19288fc7a8417bc77557ad610))
- Bump version to 0.1.82 for release [skip ci] ([fcf6c7cb8b48](https://github.com/BTCDecoded/blvm-node/commit/fcf6c7cb8b48164369578d72121fc86f6673d07c))

## [v0.1.83] - 2026-10-30

### Reconstructed

- fix(node): follow a heavier fork without restarting IBD ([001383104836](https://github.com/BTCDecoded/blvm-node/commit/001383104836d3042896fba81de1c8e7207e4be5))
- Bump version to 0.1.83 for release [skip ci] ([378855788624](https://github.com/BTCDecoded/blvm-node/commit/378855788624e5511bd574a0045e77bc51a64011))

## [v0.1.84] - 2026-11-04

### Reconstructed

- fix(node): keep bulk IBD peers busy and duplicate a covered front ([a74bde453f26](https://github.com/BTCDecoded/blvm-node/commit/a74bde453f262b08103f72f17dd0b20b30f29fad))
- style(node): wrap the covered-front assertion ([bf6d9eefb0bc](https://github.com/BTCDecoded/blvm-node/commit/bf6d9eefb0bc08a86fb0bf216b19089018295b6b))
- Store the checked mainnet assume-valid hash and bake production IBD defaults. ([0f9776e9b918](https://github.com/BTCDecoded/blvm-node/commit/0f9776e9b918efb25bdcd6278a1c5d51e0a6d036))
- fix(node): close mempool and template splits with Core ([f7804dcdc253](https://github.com/BTCDecoded/blvm-node/commit/f7804dcdc2535b58e447b98d3c4bb6a77b5b2606))
- style(node): format the mempool and template fixes ([74eb0a4c199d](https://github.com/BTCDecoded/blvm-node/commit/74eb0a4c199d7ea92e40d972affe49e2f2b5fe9a))
- fix(node): admit the policy the tests are actually checking ([23ff2493bd75](https://github.com/BTCDecoded/blvm-node/commit/23ff2493bd75da42eb28bea3bd6d688471f6dbc4))
- fix(node): admit the second mempool fixture ([3aada044f765](https://github.com/BTCDecoded/blvm-node/commit/3aada044f765a9f98e6b0cfdf26a58cdbb480902))
- fix(node): price the bare funded spend in testmempoolaccept ([9d9392678ff2](https://github.com/BTCDecoded/blvm-node/commit/9d9392678ff2899ce1ba5ecea27a0480a2132f8d))
- fix(node): import the RocksDB cache helpers into the feature module ([dfb3517e4ed8](https://github.com/BTCDecoded/blvm-node/commit/dfb3517e4ed8af92f89713dfd951664e3932cce4))
- Bump version to 0.1.84 for release [skip ci] ([58ea6d7a988a](https://github.com/BTCDecoded/blvm-node/commit/58ea6d7a988aa24e22579bc6c42846472faaa292))

## [v0.1.85] - 2026-11-05

### Reconstructed

- docs: count the 13 functions proved from the body ([d948b3f46ffe](https://github.com/BTCDecoded/blvm-node/commit/d948b3f46ffe8dedb2c26427057a2aa5658f9bc1))
- Bump version to 0.1.85 for release [skip ci] ([f209364f2e09](https://github.com/BTCDecoded/blvm-node/commit/f209364f2e0931093d8672b32f8b952a3714e7c6))

## [v0.1.86] - 2026-11-05

### Reconstructed

- docs: call this crate the BLVM node and point links at the documentation host ([e2f5d1d3eb92](https://github.com/BTCDecoded/blvm-node/commit/e2f5d1d3eb9249ed755880180e94b4dab590c5b3))
- Bump version to 0.1.86 for release [skip ci] ([b1c1b79cf38a](https://github.com/BTCDecoded/blvm-node/commit/b1c1b79cf38a456605a3df5f38f9b8a34f925d4a))

## [v0.1.87] - 2026-11-05

### Reconstructed

- fix(node): stop treating damaged chain data as mainnet ([ae74fd784890](https://github.com/BTCDecoded/blvm-node/commit/ae74fd784890f06742cefc4b85f0d2aa16650c98))
- Bump version to 0.1.87 for release [skip ci] ([f1fb97c44366](https://github.com/BTCDecoded/blvm-node/commit/f1fb97c443662c35122e07376e83e80cfca977ce))

## [v0.1.88] - 2026-11-07

### Reconstructed

- fix(node): persist mempool witnesses and close shutdown and RBF gaps ([4df7d363a713](https://github.com/BTCDecoded/blvm-node/commit/4df7d363a713c0ee9d6affefc5519cbeed38ff65))
- fix(node): refuse a package when the UTXO lock is busy ([7b923c3b9c87](https://github.com/BTCDecoded/blvm-node/commit/7b923c3b9c871f1edd61b47401f2dc72ff3e18b0))
- Drop TidesDB and land the fenced storage boundary with the new wire paths. ([6dd5964a7761](https://github.com/BTCDecoded/blvm-node/commit/6dd5964a77618471fdc12375bda4d438c5ef833a))
- fix(node): publish disconnects after a disk reorg ([3b75c6d19050](https://github.com/BTCDecoded/blvm-node/commit/3b75c6d19050bf9fac939898555332217891ea90))
- fix(node): give version-2 compact-block matching the stored witnesses ([fd590f901965](https://github.com/BTCDecoded/blvm-node/commit/fd590f901965c2fb28aa34aa0eb2a71bb1d7a177))
- fix(node): close the confirmed general-QA gaps in storage, peers, and timeouts ([f65919d616a3](https://github.com/BTCDecoded/blvm-node/commit/f65919d616a3b394815ac0a642ef448b9cb5ff78))
- fix(node): ignore a stale addr update for a known peer ([0b8fe68ad884](https://github.com/BTCDecoded/blvm-node/commit/0b8fe68ad88406c182f40f9b4d5865fa57a136f1))
- fix(node): hash each compact-block short id once ([c997e03a8b67](https://github.com/BTCDecoded/blvm-node/commit/c997e03a8b67e6fb0579e5a30e71f59b64bda98e))
- fix(node): keep the compact-block deadline on a repeated hash ([ffd401bf49d8](https://github.com/BTCDecoded/blvm-node/commit/ffd401bf49d87344f78dd706b44fc7a64b129790))
- fix(node): abort the peer read when the peer disconnects ([e7226019fff4](https://github.com/BTCDecoded/blvm-node/commit/e7226019fff46ad3c30b5d8ed9f50a54cdfeb0b9))
- fix(node): drop connection-rate violations after the window ([7cee709398f4](https://github.com/BTCDecoded/blvm-node/commit/7cee709398f487908fdcc0ac642903b7f18e58fc))
- fix(node): cap inbound TCP connections at three per address ([ca0b77e3e9f0](https://github.com/BTCDecoded/blvm-node/commit/ca0b77e3e9f016d06113590896a667b55a925efd))
- fix(node): use standard raw-tx codes and the operator mempool file ([206c5a402cc4](https://github.com/BTCDecoded/blvm-node/commit/206c5a402cc49e5515256b4890f739bfed925124))
- fix(node): confine module processes and close the reviewed trust gaps ([6230a225e259](https://github.com/BTCDecoded/blvm-node/commit/6230a225e2592211be4f1a32395441a4844f7808))
- Advertise NODE_FIBRE from the loaded module and let WASM guests read blocks. ([f7bd23731e2c](https://github.com/BTCDecoded/blvm-node/commit/f7bd23731e2cfbd16db092b6df9ae45ecf385614))
- Keep spec-lock and the extra databases off the default node. ([90f890ff74a5](https://github.com/BTCDecoded/blvm-node/commit/90f890ff74a56a862145e2cb18f1fbd76eccef53))
- fix(node): commit one storage update in one LMDB transaction ([427210deb75f](https://github.com/BTCDecoded/blvm-node/commit/427210deb75ffd470c654603ad2b1ac25e9b45c9))
- fix(node): use the consensus BIP68 version gate in the mempool ([c2fadbbaa72e](https://github.com/BTCDecoded/blvm-node/commit/c2fadbbaa72e911eda7c9ab427096b295a79d501))
- fix(node): honor Core listen=0 and drop unread config fields ([0427d9025ac3](https://github.com/BTCDecoded/blvm-node/commit/0427d9025ac3f652d5502464765146bebabfffdf))
- fix(dandelion): jitter per-tx stem timeout to prevent timing-based origin inference ([7c1a3b5434fb](https://github.com/BTCDecoded/blvm-node/commit/7c1a3b5434fbbe7bea544e0ceb697b8e99520777))
- fix(node): apply BIP90 version floors when storing headers ([98c81df6f7f9](https://github.com/BTCDecoded/blvm-node/commit/98c81df6f7f9eb15bfb604969422b93017097e71))
- fix(node): keep header sync on the next peer ([ca478f20d9cf](https://github.com/BTCDecoded/blvm-node/commit/ca478f20d9cff64b82ea8a28bdab87b2976025ee))
- fix(node): hash the wire header without parsing the block ([462a88f1c6d3](https://github.com/BTCDecoded/blvm-node/commit/462a88f1c6d3a5345921be6f6bc5bb00de80a4ce))
- fix(node): stop cloning a witness list the connect path ignores ([e82c45c5bc26](https://github.com/BTCDecoded/blvm-node/commit/e82c45c5bc26421210e484c72b9d80dc3de7a6ff))
- fix(node): probe confirmed bodies with a key check ([9aed64eb9938](https://github.com/BTCDecoded/blvm-node/commit/9aed64eb9938c1f9ed3c6baa75eacca7e568c862))
- fix(node): serialize only the duplicate-coinbase heights ([a9cae821d57b](https://github.com/BTCDecoded/blvm-node/commit/a9cae821d57b4c579c95198f9633bd26b73faf61))
- fix(node): move a delivered block to its only receiver ([5e231196aa03](https://github.com/BTCDecoded/blvm-node/commit/5e231196aa03c586899265e0c8b15f661f805adf))
- fix(node): log the body probe startup already stored ([bc0aa1d50065](https://github.com/BTCDecoded/blvm-node/commit/bc0aa1d500651eafaca9f78dfd78796c31e7317a))
- fix(node): return the connected UTXO set without cloning it ([9fe6dcdb59f3](https://github.com/BTCDecoded/blvm-node/commit/9fe6dcdb59f3b05cf70c74a38efc2c610d47a0b4))
- style: reflow network helpers and a header test ([cc06dad2d61f](https://github.com/BTCDecoded/blvm-node/commit/cc06dad2d61f3bae1890324db3d40cdda3c92e5b))
- fix(node): keep the next header peer after a short batch ([01f5e92b33f2](https://github.com/BTCDecoded/blvm-node/commit/01f5e92b33f2eae51d1c369012ec53ac97283e7b))
- revert(node): remove the bundled testnet3 map ahead of F21 ([7108c0f94671](https://github.com/BTCDecoded/blvm-node/commit/7108c0f94671ed5866a5be5074c0e480676ed0a1))
- fix(node): read a stored testnet3 name as testnet ([105178de6752](https://github.com/BTCDecoded/blvm-node/commit/105178de6752c54db92ba286fca215f9483778ae))
- revert(node): remove the bundled coin height check ahead of F33 ([7678f50d0852](https://github.com/BTCDecoded/blvm-node/commit/7678f50d085274d9b4e249451d67f1bd48d01bac))
- fix(node): reject a Core coin height that does not fit in u32 ([5ce137c9b256](https://github.com/BTCDecoded/blvm-node/commit/5ce137c9b256186a6696dc1016288f54112036df))
- fix(node): ask the next header peer from the last slot ([87478dac54fc](https://github.com/BTCDecoded/blvm-node/commit/87478dac54fcc3aa02cf6388b76a085a94564cc7))
- revert(node): restore the exact Failed commitment check ahead of F34 ([91268e74502f](https://github.com/BTCDecoded/blvm-node/commit/91268e74502faac8aa0d7ca5afc1f1834e09fd2f))
- fix(node): refuse startup when a commitment marker is present ([895b1fc5d9ba](https://github.com/BTCDecoded/blvm-node/commit/895b1fc5d9baac646f599da48b1a4dd0f8669013))
- test(node): fail when a matching witness list is cloned ([21469c0b7185](https://github.com/BTCDecoded/blvm-node/commit/21469c0b7185ecb77e5ec4c3e59fe99b6badb740))
- revert(node): split the tip update ahead of F2 ([538c59ddbd18](https://github.com/BTCDecoded/blvm-node/commit/538c59ddbd18e9a25473e9db7b245fabda8f989f))
- fix(node): drop body-dup heights behind the download window ([cf83a688a80e](https://github.com/BTCDecoded/blvm-node/commit/cf83a688a80e8be0437633eca42a824cb81ba878))
- fix(node): commit the tip update in one transaction ([7db0266a23d7](https://github.com/BTCDecoded/blvm-node/commit/7db0266a23d793a3d47b28ab3b08696cff8323ca))
- test(node): fail when the startup body log probes again ([0ebf82d7129e](https://github.com/BTCDecoded/blvm-node/commit/0ebf82d7129e421820d1373a1a4fd3bc4c17bbdb))
- revert(node): leave the custom tx index ahead of F5 ([5ca8f266fd2f](https://github.com/BTCDecoded/blvm-node/commit/5ca8f266fd2f3835f1fb17b314ddae7c1efc8207))
- fix(node): drop the custom tx index when asked ([2c87836887c1](https://github.com/BTCDecoded/blvm-node/commit/2c87836887c1553b069b29d24cf1a9216c99f447))
- fix(node): move the owned block into fork choice ([d813c00a5f0a](https://github.com/BTCDecoded/blvm-node/commit/d813c00a5f0a4292a492f7ad0a993a708ec56560))
- style(node): fit the fork-choice entry signature ([cedc912f8788](https://github.com/BTCDecoded/blvm-node/commit/cedc912f8788d7dee5200520e419d6dca9b13c84))
- fix(node): drop local-gap misses behind the cursor ([64a22982c1f2](https://github.com/BTCDecoded/blvm-node/commit/64a22982c1f26fb7fe6bf857c49ce4d731b357b4))
- fix(node): estimate a block before the bridge lock ([68393b0fbdb8](https://github.com/BTCDecoded/blvm-node/commit/68393b0fbdb88c460a90916e4c1951bd7f5249b4))
- fix(node): give a slow peer the last tile of a short window ([63134b5cd4e1](https://github.com/BTCDecoded/blvm-node/commit/63134b5cd4e1d9fe3394ecb999f078fd1d80f0fb))
- fix(node): keep one ancestor list for a header batch ([84977b6c65ec](https://github.com/BTCDecoded/blvm-node/commit/84977b6c65ecfd75544e4bb24517d65f10f78203))
- fix(node): drop skipped block hashes behind the cursor ([8eccf3fd45c1](https://github.com/BTCDecoded/blvm-node/commit/8eccf3fd45c132f94dcb01fd743641f4275dba46))
- fix(node): forget a GetData hash when the request is cancelled ([0813a988f0dc](https://github.com/BTCDecoded/blvm-node/commit/0813a988f0dc166169284cad4113d86f7136485c))
- fix(node): index the undo log once per block ([d44da012eb7d](https://github.com/BTCDecoded/blvm-node/commit/d44da012eb7d9ef9d1045f4d71311f8a282d79d8))
- fix(node): read smaps once per memory report ([29d0350f706d](https://github.com/BTCDecoded/blvm-node/commit/29d0350f706dc845a9ed4e53f55f247238d0991f))
- fix(node): build the output cache on the connect worker ([3578416e3bca](https://github.com/BTCDecoded/blvm-node/commit/3578416e3bca61d19d59409cb1ed3e6301eb1a8f))
- fix(node): do not let detected LAN peers exclude WAN peers ([630d38459829](https://github.com/BTCDecoded/blvm-node/commit/630d384598292de2cc3544f21bfc2d7ebf2dc053))
- fix(node): top up the roster when a pin does not restrict download ([441e00a2325e](https://github.com/BTCDecoded/blvm-node/commit/441e00a2325eadcc670e45313b84c83de80e30ad))
- fix(node): import a 0.14 coin as its unspent outputs ([27ed11da630a](https://github.com/BTCDecoded/blvm-node/commit/27ed11da630a685946dfc141be096b7ddf8b1648))
- feat(module): add the node-owned timer service ([e2920875520d](https://github.com/BTCDecoded/blvm-node/commit/e2920875520d1bb91bda58ecffd9ecee450d612e))
- fix(module): publish ban-list share and receive events ([18fce9ff2bbb](https://github.com/BTCDecoded/blvm-node/commit/18fce9ff2bbbad503130b63fde8f7f65082f3939))
- style(node): format the module, network, and fee estimator diffs CI rejects ([9865cfb05c5a](https://github.com/BTCDecoded/blvm-node/commit/9865cfb05c5ab1e94a241d8586d3482729ff3c34))
- fix(node): make fuzz, landlock, and clippy match the CI contract ([e0692df0f365](https://github.com/BTCDecoded/blvm-node/commit/e0692df0f3652785986cb5f1322b111ea20e5f5a))
- fix(node): keep Iroh peer identity behind the iroh feature ([1fb07de1201b](https://github.com/BTCDecoded/blvm-node/commit/1fb07de1201bd8a9fb4e873eaca9c6627ea4cf43))
- test(node): store the snapshot header before loadtxoutset ([890e44a3b5ee](https://github.com/BTCDecoded/blvm-node/commit/890e44a3b5ee4304654b7ae7b6c7c4e70157c8fc))
- test(node): wait out the dandelion stem jitter window ([c1febb452eea](https://github.com/BTCDecoded/blvm-node/commit/c1febb452eea9872892f4b64577ee08fc7636069))
- test(node): wait out the dandelion jitter in the suite too ([0efeccd2e697](https://github.com/BTCDecoded/blvm-node/commit/0efeccd2e697b1d2393a210922d2debde93d7bc0))
- fix(node): close module IPC connections when the server task stops ([2b55450fb1c0](https://github.com/BTCDecoded/blvm-node/commit/2b55450fb1c0c638c347afe26fd93d3ca43e7811))
- test(node): a module publishes ModuleEvent, not a chain event ([43ca3be402c4](https://github.com/BTCDecoded/blvm-node/commit/43ca3be402c4104fa35889501b10589e65c771dd))
- test(node): migrate the Core fixture into a directory inside it ([36fd3286bc48](https://github.com/BTCDecoded/blvm-node/commit/36fd3286bc4889aca760d4065f70b71547e774cc))
- fix(node): publish the storage fence before the node crate ([92aaeb01ca88](https://github.com/BTCDecoded/blvm-node/commit/92aaeb01ca88fd7974db280c2f8d5ea782d3ddc8))
- Bump version to 0.1.88 for release [skip ci] ([b491aebbd1c9](https://github.com/BTCDecoded/blvm-node/commit/b491aebbd1c98b5fe475400748523608dc05a807))

## [v0.1.89] - 2026-11-08

### Reconstructed

- fix(node): keep Landlock in the Linux build ([9918c7a7379d](https://github.com/BTCDecoded/blvm-node/commit/9918c7a7379d16437350df45f10c97b120be19bc))
- Bump version to 0.1.89 for release [skip ci] ([9c0a2d121657](https://github.com/BTCDecoded/blvm-node/commit/9c0a2d121657ccbcc7f1457570391647df02000c))

## [v0.1.90] - 2026-11-08

### Reconstructed

- fix(node): install a module release whose module.toml lags the tag ([778423d6504e](https://github.com/BTCDecoded/blvm-node/commit/778423d6504e774b06740fd87837e7dab1b52187))
- Bump version to 0.1.90 for release [skip ci] ([4096a13abc85](https://github.com/BTCDecoded/blvm-node/commit/4096a13abc855063a906feb66acd7d5d21b6efcd))

## [v0.1.91] - 2026-11-09

### Reconstructed

- fix(node): close mempool, IBD, and storage holes from the QA pass ([982ef1cde1d0](https://github.com/BTCDecoded/blvm-node/commit/982ef1cde1d07e14c72c86f91e0b320b76271a39))
- fix(node): replace by the incremental relay fee in standard mode ([05e3e1402723](https://github.com/BTCDecoded/blvm-node/commit/05e3e1402723c7b69498d656ef3bec7f1315a5be))
- Drop the user-facing secp-gpu feature. ([e906796b94a2](https://github.com/BTCDecoded/blvm-node/commit/e906796b94a201183284c4b98866360d54f9bdf4))
- fix(node): keep a reorg out of fee-history failures ([19c16de0472c](https://github.com/BTCDecoded/blvm-node/commit/19c16de0472c54376674eb9c50a3c86c613208c8))
- fix(module): accept every in-flight timer acknowledgement ([4922e38951ca](https://github.com/BTCDecoded/blvm-node/commit/4922e38951cac5ff337c2bdd8d779d219a6724f6))
- docs: match the mempool default and the ZMQ sequence body ([e0a4f8171101](https://github.com/BTCDecoded/blvm-node/commit/e0a4f817110163ede5d089c1717aff057fcae2e0))
- fix(node): apply the configured fee floor in conservative RBF ([d29ecfbf7476](https://github.com/BTCDecoded/blvm-node/commit/d29ecfbf7476d79061e6170f7364c73847be270e))
- Bump version to 0.1.91 for release [skip ci] ([9aa36d2126a8](https://github.com/BTCDecoded/blvm-node/commit/9aa36d2126a87966d7e02053a70aa2649216553d))

## [v0.1.92] - 2026-11-11

### Reconstructed

- fix(node): restore a failed reorg and bound peer intake ([76f2b437ceed](https://github.com/BTCDecoded/blvm-node/commit/76f2b437ceed005da0d5b26365b06e952e6d4cc8))
- fix(node): connect a submitted sibling at its parent's height ([148f75f66fec](https://github.com/BTCDecoded/blvm-node/commit/148f75f66fecfc754e6098126fdaa043ac4c68bf))
- fix(node): grind the side-block submit until it meets the regtest target ([70b8562522b5](https://github.com/BTCDecoded/blvm-node/commit/70b8562522b53dce01ba0435211252a44f156a91))
- Bump version to 0.1.92 for release [skip ci] ([794cc2940a28](https://github.com/BTCDecoded/blvm-node/commit/794cc2940a28e300b0a745dbd774a5ab8fe5268e))
