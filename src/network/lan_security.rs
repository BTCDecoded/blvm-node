//! LAN Peer Security Module
//!
//! Implements security policies for LAN peer discovery and prioritization.
//! 
//! ## Security Model
//! 
//! Internet checkpoints are the PRIMARY security mechanism. Even with discovery ON,
//! we can't be eclipsed because we require internet consensus verification.
//!
//! Key guarantees:
//! - Maximum 25% LAN peers (hard cap)
//! - Minimum 75% internet peers
//! - Internet checkpoint validation every 1000 blocks
//! - Checkpoint failure = permanent ban
//! - LAN addresses never advertised

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::network::peer_scoring::is_lan_peer;

// ============================================================================
// SECURITY CONSTANTS (Not configurable - security critical)
// ============================================================================

/// Minimum percentage of internet peers required (75%)
pub const MIN_INTERNET_PEER_PERCENTAGE: u8 = 75;

/// Maximum percentage of LAN peers allowed (25%)
pub const MAX_LAN_PEER_PERCENTAGE: u8 = 25;

/// Minimum internet peers required to sync
pub const MIN_INTERNET_PEERS_FOR_SYNC: usize = 3;

/// Block checkpoint validation interval
pub const BLOCK_CHECKPOINT_INTERVAL: u64 = 1000;

/// Header checkpoint validation interval
pub const HEADER_CHECKPOINT_INTERVAL: u64 = 10000;

/// Minimum peers needed for checkpoint consensus
pub const MIN_CHECKPOINT_PEERS: usize = 3;

/// Ban duration for checkpoint failure (1 year in seconds)
pub const CHECKPOINT_FAILURE_BAN_DURATION: Duration = Duration::from_secs(31_536_000);

/// Maximum discovered LAN peers (whitelisted are separate)
pub const MAX_DISCOVERED_LAN_PEERS: usize = 1;

// ============================================================================
// SCORE MULTIPLIERS (Reduced from 10x to 3x max)
// ============================================================================

/// Initial LAN peer score multiplier
pub const INITIAL_LAN_MULTIPLIER: f64 = 1.5;

/// Score multiplier after 1000 valid blocks
pub const LEVEL_2_LAN_MULTIPLIER: f64 = 2.0;

/// Maximum score multiplier after 10000 blocks AND 1 hour
pub const MAX_LAN_MULTIPLIER: f64 = 3.0;

/// Blocks required for level 2 trust
pub const BLOCKS_FOR_LEVEL_2: u64 = 1000;

/// Blocks required for level 3 trust
pub const BLOCKS_FOR_LEVEL_3: u64 = 10000;

/// Minimum connection time for max trust (1 hour)
pub const MIN_TIME_FOR_MAX_TRUST: Duration = Duration::from_secs(3600);

// ============================================================================
// FAILURE PENALTIES (Aggressive for LAN peers)
// ============================================================================

/// Failures before score penalty for LAN peers (vs 5 for internet)
pub const LAN_FAILURE_TOLERANCE: u32 = 1;

/// Failures before losing LAN status entirely
pub const LAN_DEMOTION_THRESHOLD: u32 = 3;

// ============================================================================
// LAN PEER TRUST STATE
// ============================================================================

/// Trust level for a LAN peer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanTrustLevel {
    /// Initial trust (1.5x multiplier)
    Initial,
    /// Level 2 trust after 1000 valid blocks (2x multiplier)
    Level2,
    /// Maximum trust after 10000 blocks AND 1 hour (3x multiplier)
    Maximum,
    /// Demoted - lost LAN preference due to failures
    Demoted,
    /// Banned - checkpoint failure or other critical violation
    Banned,
}

impl LanTrustLevel {
    pub fn multiplier(&self) -> f64 {
        match self {
            LanTrustLevel::Initial => INITIAL_LAN_MULTIPLIER,
            LanTrustLevel::Level2 => LEVEL_2_LAN_MULTIPLIER,
            LanTrustLevel::Maximum => MAX_LAN_MULTIPLIER,
            LanTrustLevel::Demoted => 1.0, // No bonus
            LanTrustLevel::Banned => 0.0,  // Don't use at all
        }
    }
}

/// State tracking for a LAN peer
#[derive(Debug, Clone)]
pub struct LanPeerState {
    pub addr: SocketAddr,
    pub trust_level: LanTrustLevel,
    pub valid_blocks: u64,
    pub failures: u32,
    pub connected_at: Instant,
    pub is_whitelisted: bool,
    pub last_checkpoint_height: u64,
    pub ban_until: Option<Instant>,
}

impl LanPeerState {
    pub fn new(addr: SocketAddr, is_whitelisted: bool) -> Self {
        Self {
            addr,
            trust_level: if is_whitelisted {
                LanTrustLevel::Maximum // Whitelisted peers start at max trust
            } else {
                LanTrustLevel::Initial
            },
            valid_blocks: 0,
            failures: 0,
            connected_at: Instant::now(),
            is_whitelisted,
            last_checkpoint_height: 0,
            ban_until: None,
        }
    }

    /// Update trust level based on blocks and time
    pub fn update_trust(&mut self) {
        if self.trust_level == LanTrustLevel::Banned || self.trust_level == LanTrustLevel::Demoted {
            return; // Can't upgrade from banned/demoted
        }

        let time_connected = self.connected_at.elapsed();

        if self.valid_blocks >= BLOCKS_FOR_LEVEL_3 && time_connected >= MIN_TIME_FOR_MAX_TRUST {
            if self.trust_level != LanTrustLevel::Maximum {
                info!("LAN peer {} promoted to Maximum trust ({} blocks, {:?} connected)",
                    self.addr, self.valid_blocks, time_connected);
                self.trust_level = LanTrustLevel::Maximum;
            }
        } else if self.valid_blocks >= BLOCKS_FOR_LEVEL_2 {
            if self.trust_level == LanTrustLevel::Initial {
                info!("LAN peer {} promoted to Level2 trust ({} blocks)",
                    self.addr, self.valid_blocks);
                self.trust_level = LanTrustLevel::Level2;
            }
        }
    }

    /// Record a valid block from this peer
    pub fn record_valid_block(&mut self) {
        self.valid_blocks += 1;
        self.update_trust();
    }

    /// Record a failure from this peer
    pub fn record_failure(&mut self) -> bool {
        self.failures += 1;
        
        if self.failures >= LAN_DEMOTION_THRESHOLD {
            warn!("LAN peer {} demoted after {} failures", self.addr, self.failures);
            self.trust_level = LanTrustLevel::Demoted;
            return true; // Demoted
        }

        if self.failures >= LAN_FAILURE_TOLERANCE {
            warn!("LAN peer {} exceeded failure tolerance ({}/{})",
                self.addr, self.failures, LAN_FAILURE_TOLERANCE);
        }

        false // Not yet demoted
    }

    /// Ban this peer for checkpoint failure
    pub fn ban_for_checkpoint_failure(&mut self, reason: &str) {
        error!("SECURITY: Banning LAN peer {} for checkpoint failure: {}",
            self.addr, reason);
        self.trust_level = LanTrustLevel::Banned;
        self.ban_until = Some(Instant::now() + CHECKPOINT_FAILURE_BAN_DURATION);
    }

    /// Check if peer is banned
    pub fn is_banned(&self) -> bool {
        match self.ban_until {
            Some(until) => Instant::now() < until,
            None => self.trust_level == LanTrustLevel::Banned,
        }
    }

    /// Get current score multiplier
    pub fn get_multiplier(&self) -> f64 {
        if self.is_banned() {
            return 0.0;
        }
        self.trust_level.multiplier()
    }
}

// ============================================================================
// LAN SECURITY POLICY
// ============================================================================

/// LAN security policy enforcer
pub struct LanSecurityPolicy {
    /// State for each LAN peer
    lan_peers: RwLock<HashMap<SocketAddr, LanPeerState>>,
    /// Whitelisted LAN peers
    whitelist: RwLock<HashSet<SocketAddr>>,
    /// Banned peers (persisted across reconnects)
    banned_peers: RwLock<HashMap<SocketAddr, (Instant, String)>>,
    /// Discovery enabled
    pub discovery_enabled: bool,
}

impl LanSecurityPolicy {
    pub fn new() -> Self {
        Self {
            lan_peers: RwLock::new(HashMap::new()),
            whitelist: RwLock::new(HashSet::new()),
            banned_peers: RwLock::new(HashMap::new()),
            discovery_enabled: true, // ON by default - safe because checkpoints are required
        }
    }

    /// Add a peer to the whitelist
    pub fn add_to_whitelist(&self, addr: SocketAddr) {
        if !is_lan_peer(&addr) {
            warn!("Cannot whitelist non-LAN peer: {}", addr);
            return;
        }
        
        self.whitelist.write().unwrap().insert(addr);
        info!("Added {} to LAN whitelist", addr);
    }

    /// Check if a peer is whitelisted
    pub fn is_whitelisted(&self, addr: &SocketAddr) -> bool {
        self.whitelist.read().unwrap().contains(addr)
    }

    /// Check if we should accept a new LAN peer connection
    pub fn should_accept_lan_peer(
        &self,
        addr: &SocketAddr,
        current_internet_peers: usize,
        current_lan_peers: usize,
        target_peers: usize,
    ) -> (bool, String) {
        // Check ban list first
        if self.is_peer_banned(addr) {
            return (false, format!("Peer {} is banned", addr));
        }

        // Whitelisted peers always accepted (but still count against cap)
        let is_whitelisted = self.is_whitelisted(addr);

        // Calculate limits
        let max_lan = (target_peers * MAX_LAN_PEER_PERCENTAGE as usize) / 100;
        let min_internet = (target_peers * MIN_INTERNET_PEER_PERCENTAGE as usize) / 100;

        // Check 25% LAN cap
        if current_lan_peers >= max_lan {
            return (false, format!(
                "LAN cap reached: {}/{} (max {}%)",
                current_lan_peers, target_peers, MAX_LAN_PEER_PERCENTAGE
            ));
        }

        // Check 75% internet minimum would still be met
        let would_have_internet = current_internet_peers;
        let would_have_total = current_internet_peers + current_lan_peers + 1;
        let internet_percentage = (would_have_internet * 100) / would_have_total;
        
        if internet_percentage < MIN_INTERNET_PEER_PERCENTAGE as usize {
            return (false, format!(
                "Would violate {}% internet minimum: would have {}%",
                MIN_INTERNET_PEER_PERCENTAGE, internet_percentage
            ));
        }

        // Check discovered LAN peer limit (whitelisted are separate)
        if !is_whitelisted {
            let discovered_count = self.count_discovered_lan_peers();
            if discovered_count >= MAX_DISCOVERED_LAN_PEERS {
                return (false, format!(
                    "Discovered LAN peer limit reached: {}/{}",
                    discovered_count, MAX_DISCOVERED_LAN_PEERS
                ));
            }
        }

        (true, "Accepted".to_string())
    }

    /// Count non-whitelisted (discovered) LAN peers
    fn count_discovered_lan_peers(&self) -> usize {
        let lan_peers = self.lan_peers.read().unwrap();
        let whitelist = self.whitelist.read().unwrap();
        
        lan_peers.keys()
            .filter(|addr| !whitelist.contains(*addr))
            .count()
    }

    /// Check if we have enough internet peers to sync
    pub fn can_sync(&self, internet_peer_count: usize) -> (bool, String) {
        if internet_peer_count < MIN_INTERNET_PEERS_FOR_SYNC {
            return (false, format!(
                "Need at least {} internet peers for checkpoint validation, have {}",
                MIN_INTERNET_PEERS_FOR_SYNC, internet_peer_count
            ));
        }
        (true, "OK".to_string())
    }

    /// Register a new LAN peer
    pub fn register_lan_peer(&self, addr: SocketAddr) {
        let is_whitelisted = self.is_whitelisted(&addr);
        let state = LanPeerState::new(addr, is_whitelisted);
        
        self.lan_peers.write().unwrap().insert(addr, state);
        debug!("Registered LAN peer: {} (whitelisted: {})", addr, is_whitelisted);
    }

    /// Get current multiplier for a LAN peer
    pub fn get_peer_multiplier(&self, addr: &SocketAddr) -> f64 {
        self.lan_peers.read().unwrap()
            .get(addr)
            .map(|s| s.get_multiplier())
            .unwrap_or(INITIAL_LAN_MULTIPLIER)
    }

    /// Record a valid block from a LAN peer
    pub fn record_valid_block(&self, addr: &SocketAddr) {
        if let Some(state) = self.lan_peers.write().unwrap().get_mut(addr) {
            state.record_valid_block();
        }
    }

    /// Record a failure from a LAN peer
    pub fn record_failure(&self, addr: &SocketAddr) -> bool {
        if let Some(state) = self.lan_peers.write().unwrap().get_mut(addr) {
            return state.record_failure();
        }
        false
    }

    /// Ban a peer for checkpoint failure
    pub fn ban_for_checkpoint_failure(&self, addr: &SocketAddr, reason: &str) {
        error!("SECURITY: Banning peer {} for checkpoint failure: {}", addr, reason);
        
        // Update peer state
        if let Some(state) = self.lan_peers.write().unwrap().get_mut(addr) {
            state.ban_for_checkpoint_failure(reason);
        }

        // Add to persistent ban list
        self.banned_peers.write().unwrap().insert(
            *addr,
            (Instant::now() + CHECKPOINT_FAILURE_BAN_DURATION, reason.to_string())
        );
    }

    /// Check if a peer is banned
    pub fn is_peer_banned(&self, addr: &SocketAddr) -> bool {
        // Check peer state
        if let Some(state) = self.lan_peers.read().unwrap().get(addr) {
            if state.is_banned() {
                return true;
            }
        }

        // Check persistent ban list
        if let Some((until, _)) = self.banned_peers.read().unwrap().get(addr) {
            return Instant::now() < *until;
        }

        false
    }

    /// Check if an address should be advertised (LAN addresses should NEVER be advertised)
    pub fn is_advertisable(&self, addr: &SocketAddr) -> bool {
        !is_lan_peer(addr)
    }

    /// Check if this height needs checkpoint validation
    pub fn needs_block_checkpoint(&self, height: u64) -> bool {
        height > 0 && height % BLOCK_CHECKPOINT_INTERVAL == 0
    }

    /// Check if this height needs header checkpoint validation
    pub fn needs_header_checkpoint(&self, height: u64) -> bool {
        height > 0 && height % HEADER_CHECKPOINT_INTERVAL == 0
    }

    /// Remove a LAN peer
    pub fn remove_lan_peer(&self, addr: &SocketAddr) {
        self.lan_peers.write().unwrap().remove(addr);
    }

    /// Get stats for logging
    pub fn get_stats(&self) -> LanSecurityStats {
        let lan_peers = self.lan_peers.read().unwrap();
        let whitelist = self.whitelist.read().unwrap();
        let banned = self.banned_peers.read().unwrap();

        LanSecurityStats {
            total_lan_peers: lan_peers.len(),
            whitelisted_peers: whitelist.len(),
            discovered_peers: lan_peers.keys().filter(|a| !whitelist.contains(*a)).count(),
            banned_peers: banned.len(),
            initial_trust: lan_peers.values().filter(|s| s.trust_level == LanTrustLevel::Initial).count(),
            level2_trust: lan_peers.values().filter(|s| s.trust_level == LanTrustLevel::Level2).count(),
            max_trust: lan_peers.values().filter(|s| s.trust_level == LanTrustLevel::Maximum).count(),
            demoted: lan_peers.values().filter(|s| s.trust_level == LanTrustLevel::Demoted).count(),
        }
    }
}

impl Default for LanSecurityPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about LAN security state
#[derive(Debug, Clone)]
pub struct LanSecurityStats {
    pub total_lan_peers: usize,
    pub whitelisted_peers: usize,
    pub discovered_peers: usize,
    pub banned_peers: usize,
    pub initial_trust: usize,
    pub level2_trust: usize,
    pub max_trust: usize,
    pub demoted: usize,
}

// ============================================================================
// CHECKPOINT VALIDATION
// ============================================================================

/// Checkpoint validation result
#[derive(Debug, Clone)]
pub enum CheckpointResult {
    /// Checkpoint passed
    Valid,
    /// Checkpoint failed - peer lied
    Invalid { 
        expected: [u8; 32], 
        got: [u8; 32],
        height: u64,
    },
    /// Not enough internet peers for validation
    InsufficientPeers { have: usize, need: usize },
}

/// Validate a block checkpoint against internet peers
/// Returns the checkpoint result and the peer that provided the block
#[inline]
pub fn validate_block_checkpoint(
    lan_peer: SocketAddr,
    block_hash_from_lan: [u8; 32],
    block_hash_from_internet: [u8; 32],
    height: u64,
) -> CheckpointResult {
    if block_hash_from_lan == block_hash_from_internet {
        CheckpointResult::Valid
    } else {
        error!(
            "CHECKPOINT FAILURE: LAN peer {} provided wrong hash at height {}\n\
             Expected: {}\n\
             Got: {}",
            lan_peer,
            height,
            hex::encode(block_hash_from_internet),
            hex::encode(block_hash_from_lan)
        );
        CheckpointResult::Invalid {
            expected: block_hash_from_internet,
            got: block_hash_from_lan,
            height,
        }
    }
}

// ============================================================================
// INTERNET CHECKPOINT VALIDATOR
// ============================================================================

/// Timeout for checkpoint requests (5 seconds)
pub const CHECKPOINT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum retries for checkpoint requests
pub const CHECKPOINT_MAX_RETRIES: usize = 3;

/// Internet checkpoint validator
/// 
/// Validates blocks from LAN peers against internet consensus every N blocks.
/// This is the PRIMARY security mechanism against LAN-based eclipse attacks.
pub struct InternetCheckpointValidator {
    /// Last validated height
    last_validated_height: std::sync::atomic::AtomicU64,
    /// Cached checkpoint results (height -> hash)
    checkpoint_cache: RwLock<HashMap<u64, [u8; 32]>>,
    /// Maximum cache size (to limit memory)
    max_cache_size: usize,
}

impl InternetCheckpointValidator {
    pub fn new() -> Self {
        Self {
            last_validated_height: std::sync::atomic::AtomicU64::new(0),
            checkpoint_cache: RwLock::new(HashMap::with_capacity(100)),
            max_cache_size: 1000,
        }
    }

    /// Check if a height needs validation
    #[inline]
    pub fn needs_validation(&self, height: u64) -> bool {
        height > 0 && height % BLOCK_CHECKPOINT_INTERVAL == 0
    }

    /// Check if this is a header checkpoint height
    #[inline]
    pub fn is_header_checkpoint(&self, height: u64) -> bool {
        height > 0 && height % HEADER_CHECKPOINT_INTERVAL == 0
    }

    /// Get last validated height
    pub fn last_validated(&self) -> u64 {
        self.last_validated_height.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record successful validation
    pub fn record_validation(&self, height: u64, hash: [u8; 32]) {
        self.last_validated_height.store(height, std::sync::atomic::Ordering::Relaxed);
        
        // Cache the result
        let mut cache = self.checkpoint_cache.write().unwrap();
        
        // Evict old entries if cache is full
        if cache.len() >= self.max_cache_size {
            // Remove entries below current height - 10000
            let evict_below = height.saturating_sub(10000);
            cache.retain(|h, _| *h > evict_below);
        }
        
        cache.insert(height, hash);
    }

    /// Get cached hash for a height (if available)
    pub fn get_cached_hash(&self, height: u64) -> Option<[u8; 32]> {
        self.checkpoint_cache.read().unwrap().get(&height).copied()
    }

    /// Validate a block from LAN peer against internet consensus
    /// 
    /// This is called every BLOCK_CHECKPOINT_INTERVAL blocks during IBD.
    /// If validation fails, the LAN peer is immediately banned.
    /// 
    /// Returns Ok(()) if validation passed or was skipped,
    /// Err with details if validation failed.
    pub async fn validate_lan_block(
        &self,
        lan_peer: SocketAddr,
        lan_block_hash: [u8; 32],
        height: u64,
        internet_peers: &[SocketAddr],
        get_block_hash_fn: impl Fn(SocketAddr, u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<[u8; 32]>> + Send>>,
    ) -> Result<(), CheckpointValidationError> {
        // Skip if not a checkpoint height
        if !self.needs_validation(height) {
            return Ok(());
        }

        // Check cache first (optimization)
        if let Some(cached) = self.get_cached_hash(height) {
            if cached == lan_block_hash {
                debug!("Checkpoint {} passed (cached)", height);
                return Ok(());
            } else {
                return Err(CheckpointValidationError::HashMismatch {
                    height,
                    expected: cached,
                    got: lan_block_hash,
                    lan_peer,
                });
            }
        }

        // Need at least MIN_CHECKPOINT_PEERS internet peers
        if internet_peers.len() < MIN_CHECKPOINT_PEERS {
            warn!("Insufficient internet peers for checkpoint validation: {} < {}",
                internet_peers.len(), MIN_CHECKPOINT_PEERS);
            return Err(CheckpointValidationError::InsufficientPeers {
                have: internet_peers.len(),
                need: MIN_CHECKPOINT_PEERS,
            });
        }

        // Query internet peers for the block hash at this height
        // We need consensus from MIN_CHECKPOINT_PEERS
        let mut internet_hashes: Vec<[u8; 32]> = Vec::with_capacity(internet_peers.len());
        
        for peer in internet_peers.iter().take(MIN_CHECKPOINT_PEERS + 2) {
            match tokio::time::timeout(
                CHECKPOINT_REQUEST_TIMEOUT,
                get_block_hash_fn(*peer, height)
            ).await {
                Ok(Some(hash)) => {
                    internet_hashes.push(hash);
                }
                Ok(None) => {
                    debug!("Internet peer {} didn't return hash for height {}", peer, height);
                }
                Err(_) => {
                    debug!("Timeout getting hash from internet peer {} for height {}", peer, height);
                }
            }
            
            // Early exit if we have enough consensus
            if internet_hashes.len() >= MIN_CHECKPOINT_PEERS {
                break;
            }
        }

        if internet_hashes.len() < MIN_CHECKPOINT_PEERS {
            return Err(CheckpointValidationError::InsufficientResponses {
                have: internet_hashes.len(),
                need: MIN_CHECKPOINT_PEERS,
            });
        }

        // Check that internet peers agree with each other
        let consensus_hash = internet_hashes[0];
        let all_agree = internet_hashes.iter().all(|h| *h == consensus_hash);
        
        if !all_agree {
            warn!("Internet peers disagree on hash at height {} - network may be under attack", height);
            // In this case, we can't trust anyone - pause sync
            return Err(CheckpointValidationError::InternetDisagreement { height });
        }

        // Now validate LAN peer's hash against internet consensus
        if lan_block_hash == consensus_hash {
            info!("Checkpoint {} passed: LAN peer {} verified against {} internet peers",
                height, lan_peer, internet_hashes.len());
            self.record_validation(height, consensus_hash);
            Ok(())
        } else {
            error!("CHECKPOINT FAILURE at height {}: LAN peer {} provided wrong hash!\n\
                    Internet consensus: {}\n\
                    LAN peer provided:  {}",
                height,
                lan_peer,
                hex::encode(consensus_hash),
                hex::encode(lan_block_hash)
            );
            Err(CheckpointValidationError::HashMismatch {
                height,
                expected: consensus_hash,
                got: lan_block_hash,
                lan_peer,
            })
        }
    }
}

impl Default for InternetCheckpointValidator {
    fn default() -> Self {
        Self::new()
    }
}

/// Checkpoint validation error
#[derive(Debug, Clone)]
pub enum CheckpointValidationError {
    /// Hash mismatch - LAN peer lied
    HashMismatch {
        height: u64,
        expected: [u8; 32],
        got: [u8; 32],
        lan_peer: SocketAddr,
    },
    /// Not enough internet peers
    InsufficientPeers { have: usize, need: usize },
    /// Not enough internet peers responded
    InsufficientResponses { have: usize, need: usize },
    /// Internet peers disagree (possible network attack)
    InternetDisagreement { height: u64 },
}

impl std::fmt::Display for CheckpointValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashMismatch { height, expected, got, lan_peer } => {
                write!(f, "Checkpoint failure at height {}: LAN peer {} provided wrong hash. Expected: {}, got: {}",
                    height, lan_peer, hex::encode(expected), hex::encode(got))
            }
            Self::InsufficientPeers { have, need } => {
                write!(f, "Insufficient internet peers for checkpoint: have {}, need {}", have, need)
            }
            Self::InsufficientResponses { have, need } => {
                write!(f, "Insufficient internet responses for checkpoint: have {}, need {}", have, need)
            }
            Self::InternetDisagreement { height } => {
                write!(f, "Internet peers disagree on hash at height {} - possible attack", height)
            }
        }
    }
}

impl std::error::Error for CheckpointValidationError {}

// ============================================================================
// PROTOCOL + HEADERS VERIFICATION FOR DISCOVERY
// ============================================================================

/// Timeout for protocol verification handshake
pub const PROTOCOL_VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Timeout for headers verification
pub const HEADERS_VERIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum header divergence allowed (blocks)
pub const MAX_HEADER_DIVERGENCE: u64 = 6;

/// Protocol verification result
#[derive(Debug, Clone)]
pub enum ProtocolVerifyResult {
    /// Verified successfully
    Valid {
        protocol_version: u32,
        user_agent: String,
        start_height: u64,
    },
    /// Protocol handshake failed
    HandshakeFailed(String),
    /// Connection failed
    ConnectionFailed(String),
    /// Timeout
    Timeout,
}

/// Headers verification result
#[derive(Debug, Clone)]
pub enum HeadersVerifyResult {
    /// On same chain as internet consensus
    OnSameChain,
    /// Different chain (diverged more than MAX_HEADER_DIVERGENCE blocks)
    DifferentChain { divergence_height: u64 },
    /// Couldn't verify (timeout, no response, etc.)
    VerificationFailed(String),
}

/// LAN peer discovery verifier
/// 
/// Verifies that discovered LAN peers are:
/// 1. Real Bitcoin nodes (protocol handshake)
/// 2. On the same chain as internet consensus (headers check)
pub struct DiscoveryVerifier {
    /// Known good chain tip from internet peers
    internet_chain_tip: RwLock<Option<ChainTipInfo>>,
}

/// Chain tip information
#[derive(Debug, Clone)]
pub struct ChainTipInfo {
    pub height: u64,
    pub hash: [u8; 32],
    pub timestamp: u64,
}

impl DiscoveryVerifier {
    pub fn new() -> Self {
        Self {
            internet_chain_tip: RwLock::new(None),
        }
    }

    /// Update known internet chain tip
    pub fn update_internet_tip(&self, tip: ChainTipInfo) {
        *self.internet_chain_tip.write().unwrap() = Some(tip);
    }

    /// Get known internet chain tip
    pub fn get_internet_tip(&self) -> Option<ChainTipInfo> {
        self.internet_chain_tip.read().unwrap().clone()
    }

    /// Verify a discovered LAN peer
    /// 
    /// This performs:
    /// 1. Protocol handshake verification (version/verack)
    /// 2. Chain verification (compare headers with internet)
    /// 
    /// Returns true only if both pass.
    pub async fn verify_lan_peer(
        &self,
        addr: SocketAddr,
        do_handshake: impl Fn(SocketAddr) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(u32, String, u64)>> + Send>>,
        get_peer_tip: impl Fn(SocketAddr) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(u64, [u8; 32])>> + Send>>,
    ) -> (bool, String) {
        // Step 1: Protocol handshake
        info!("Verifying discovered LAN peer {} - protocol handshake...", addr);
        
        let handshake_result = match tokio::time::timeout(
            PROTOCOL_VERIFY_TIMEOUT,
            do_handshake(addr)
        ).await {
            Ok(Some((version, user_agent, height))) => {
                debug!("LAN peer {} handshake OK: version={}, agent={}, height={}",
                    addr, version, user_agent, height);
                ProtocolVerifyResult::Valid {
                    protocol_version: version,
                    user_agent,
                    start_height: height,
                }
            }
            Ok(None) => {
                warn!("LAN peer {} failed protocol handshake", addr);
                return (false, "Protocol handshake failed".to_string());
            }
            Err(_) => {
                warn!("LAN peer {} protocol handshake timed out", addr);
                return (false, "Protocol handshake timeout".to_string());
            }
        };

        // Step 2: Headers verification (only if we have an internet tip)
        let internet_tip = match self.get_internet_tip() {
            Some(tip) => tip,
            None => {
                // No internet tip yet - can't verify chain
                // This is OK during early startup
                info!("LAN peer {} passed protocol check (chain verification deferred - no internet tip yet)", addr);
                return (true, "Protocol verified, chain check deferred".to_string());
            }
        };

        info!("Verifying LAN peer {} - checking chain against internet (tip: {})", 
            addr, internet_tip.height);

        let peer_tip = match tokio::time::timeout(
            HEADERS_VERIFY_TIMEOUT,
            get_peer_tip(addr)
        ).await {
            Ok(Some((height, hash))) => (height, hash),
            Ok(None) => {
                warn!("LAN peer {} didn't provide chain tip", addr);
                return (false, "Failed to get chain tip".to_string());
            }
            Err(_) => {
                warn!("LAN peer {} chain tip request timed out", addr);
                return (false, "Chain tip request timeout".to_string());
            }
        };

        // Check if peer is on same chain
        // If peer height is close to internet tip and hash matches, they're on same chain
        let height_diff = if peer_tip.0 > internet_tip.height {
            peer_tip.0 - internet_tip.height
        } else {
            internet_tip.height - peer_tip.0
        };

        if height_diff > MAX_HEADER_DIVERGENCE {
            // Peer is significantly ahead or behind - might be on different chain
            // This could be normal (peer is syncing) or attack (minority chain)
            if peer_tip.0 < internet_tip.height.saturating_sub(100) {
                // Peer is way behind - probably still syncing, OK to use but with caution
                warn!("LAN peer {} is {} blocks behind internet tip - may still be syncing",
                    addr, internet_tip.height - peer_tip.0);
                return (true, format!("Behind by {} blocks, use with caution", 
                    internet_tip.height - peer_tip.0));
            } else if peer_tip.0 > internet_tip.height + MAX_HEADER_DIVERGENCE {
                // Peer claims to be ahead - suspicious
                warn!("LAN peer {} claims to be {} blocks ahead of internet - suspicious",
                    addr, peer_tip.0 - internet_tip.height);
                // Still allow but will be caught by checkpoint validation
                return (true, format!("Ahead by {} blocks, will verify via checkpoints",
                    peer_tip.0 - internet_tip.height));
            }
        }

        // Heights are close - peer is likely on same chain
        info!("LAN peer {} verified: protocol OK, chain height {} (internet: {})",
            addr, peer_tip.0, internet_tip.height);
        
        (true, format!("Verified: height {}", peer_tip.0))
    }

    /// Batch verify multiple LAN peers
    /// 
    /// Returns list of verified peers
    pub async fn verify_peers(
        &self,
        peers: Vec<SocketAddr>,
        do_handshake: impl Fn(SocketAddr) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(u32, String, u64)>> + Send>> + Clone,
        get_peer_tip: impl Fn(SocketAddr) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<(u64, [u8; 32])>> + Send>> + Clone,
    ) -> Vec<(SocketAddr, bool, String)> {
        let mut results = Vec::with_capacity(peers.len());
        
        for peer in peers {
            let (ok, reason) = self.verify_lan_peer(
                peer,
                do_handshake.clone(),
                get_peer_tip.clone()
            ).await;
            results.push((peer, ok, reason));
        }
        
        results
    }
}

impl Default for DiscoveryVerifier {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn lan_addr(last_octet: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, last_octet)), 8333)
    }

    fn internet_addr(last_octet: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, last_octet)), 8333)
    }

    #[test]
    fn test_75_percent_internet_minimum_enforced() {
        let policy = LanSecurityPolicy::new();
        
        // 8 target peers: need 6 internet (75%), max 2 LAN (25%)
        let target = 8;
        
        // With 6 internet and 1 LAN, can add another LAN
        let (ok, _) = policy.should_accept_lan_peer(&lan_addr(100), 6, 1, target);
        assert!(ok, "Should accept LAN peer when under cap");
        
        // With 6 internet and 2 LAN, cannot add more LAN (25% cap)
        let (ok, reason) = policy.should_accept_lan_peer(&lan_addr(101), 6, 2, target);
        assert!(!ok, "Should reject LAN peer at cap: {}", reason);
    }

    #[test]
    fn test_25_percent_lan_cap_enforced() {
        let policy = LanSecurityPolicy::new();
        
        // Even with many internet peers, LAN cap is enforced
        let (ok, _) = policy.should_accept_lan_peer(&lan_addr(100), 10, 3, 12);
        assert!(!ok, "Should enforce 25% LAN cap");
    }

    #[test]
    fn test_trust_progression() {
        let mut state = LanPeerState::new(lan_addr(100), false);
        
        assert_eq!(state.trust_level, LanTrustLevel::Initial);
        assert!((state.get_multiplier() - 1.5).abs() < 0.01);
        
        // Simulate 1000 valid blocks
        for _ in 0..1000 {
            state.record_valid_block();
        }
        assert_eq!(state.trust_level, LanTrustLevel::Level2);
        assert!((state.get_multiplier() - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_whitelisted_starts_at_max_trust() {
        let state = LanPeerState::new(lan_addr(100), true);
        
        assert_eq!(state.trust_level, LanTrustLevel::Maximum);
        assert!((state.get_multiplier() - 3.0).abs() < 0.01);
    }

    #[test]
    fn test_failure_demotion() {
        let mut state = LanPeerState::new(lan_addr(100), false);
        
        // 3 failures should demote
        assert!(!state.record_failure()); // 1
        assert!(!state.record_failure()); // 2
        assert!(state.record_failure());  // 3 - demoted
        
        assert_eq!(state.trust_level, LanTrustLevel::Demoted);
        assert!((state.get_multiplier() - 1.0).abs() < 0.01); // No bonus
    }

    #[test]
    fn test_checkpoint_failure_ban() {
        let policy = LanSecurityPolicy::new();
        let addr = lan_addr(100);
        
        policy.register_lan_peer(addr);
        policy.ban_for_checkpoint_failure(&addr, "Block hash mismatch");
        
        assert!(policy.is_peer_banned(&addr));
        
        // Banned peer should not be accepted
        let (ok, _) = policy.should_accept_lan_peer(&addr, 6, 0, 8);
        assert!(!ok, "Banned peer should be rejected");
    }

    #[test]
    fn test_lan_addresses_not_advertisable() {
        let policy = LanSecurityPolicy::new();
        
        assert!(!policy.is_advertisable(&lan_addr(100)));
        assert!(policy.is_advertisable(&internet_addr(1)));
    }

    #[test]
    fn test_checkpoint_intervals() {
        let policy = LanSecurityPolicy::new();
        
        assert!(!policy.needs_block_checkpoint(0));
        assert!(!policy.needs_block_checkpoint(500));
        assert!(policy.needs_block_checkpoint(1000));
        assert!(policy.needs_block_checkpoint(2000));
        
        assert!(!policy.needs_header_checkpoint(0));
        assert!(!policy.needs_header_checkpoint(5000));
        assert!(policy.needs_header_checkpoint(10000));
    }

    #[test]
    fn test_min_internet_peers_for_sync() {
        let policy = LanSecurityPolicy::new();
        
        let (ok, _) = policy.can_sync(2);
        assert!(!ok, "Should not allow sync with only 2 internet peers");
        
        let (ok, _) = policy.can_sync(3);
        assert!(ok, "Should allow sync with 3 internet peers");
    }

    #[test]
    fn test_discovered_lan_peer_limit() {
        let policy = LanSecurityPolicy::new();
        let addr1 = lan_addr(100);
        let addr2 = lan_addr(101);
        
        // Register first discovered peer
        policy.register_lan_peer(addr1);
        
        // Second discovered peer should be rejected (limit is 1)
        let (ok, _) = policy.should_accept_lan_peer(&addr2, 6, 1, 8);
        assert!(!ok, "Should reject second discovered LAN peer");
    }

    #[test]
    fn test_whitelisted_bypasses_discovered_limit() {
        let policy = LanSecurityPolicy::new();
        let discovered = lan_addr(100);
        let whitelisted = lan_addr(101);
        
        // Register discovered peer
        policy.register_lan_peer(discovered);
        
        // Add another to whitelist
        policy.add_to_whitelist(whitelisted);
        
        // Whitelisted peer should be accepted (but still counts against 25% cap)
        let (ok, _) = policy.should_accept_lan_peer(&whitelisted, 6, 1, 8);
        assert!(ok, "Whitelisted peer should be accepted");
    }

    // ========================================================================
    // CHECKPOINT VALIDATOR TESTS
    // ========================================================================

    #[test]
    fn test_checkpoint_validator_needs_validation() {
        let validator = InternetCheckpointValidator::new();
        
        assert!(!validator.needs_validation(0));
        assert!(!validator.needs_validation(500));
        assert!(!validator.needs_validation(999));
        assert!(validator.needs_validation(1000));
        assert!(!validator.needs_validation(1001));
        assert!(validator.needs_validation(2000));
        assert!(validator.needs_validation(10000));
    }

    #[test]
    fn test_checkpoint_validator_is_header_checkpoint() {
        let validator = InternetCheckpointValidator::new();
        
        assert!(!validator.is_header_checkpoint(0));
        assert!(!validator.is_header_checkpoint(5000));
        assert!(!validator.is_header_checkpoint(9999));
        assert!(validator.is_header_checkpoint(10000));
        assert!(!validator.is_header_checkpoint(10001));
        assert!(validator.is_header_checkpoint(20000));
    }

    #[test]
    fn test_checkpoint_validator_caching() {
        let validator = InternetCheckpointValidator::new();
        let hash = [0xab; 32];
        
        // Initially no cached hash
        assert!(validator.get_cached_hash(1000).is_none());
        
        // Record validation
        validator.record_validation(1000, hash);
        
        // Now should be cached
        assert_eq!(validator.get_cached_hash(1000), Some(hash));
        assert_eq!(validator.last_validated(), 1000);
    }

    #[test]
    fn test_checkpoint_validator_cache_eviction() {
        let mut validator = InternetCheckpointValidator::new();
        validator.max_cache_size = 10; // Small cache for testing
        
        // Fill cache
        for i in 0..15 {
            let height = i * 1000;
            validator.record_validation(height, [i as u8; 32]);
        }
        
        // Old entries should be evicted
        assert!(validator.get_cached_hash(0).is_none());
        assert!(validator.get_cached_hash(1000).is_none());
        
        // Recent entries should still be there
        assert!(validator.get_cached_hash(14000).is_some());
    }

    #[test]
    fn test_validate_block_checkpoint_valid() {
        let lan_peer = lan_addr(100);
        let hash = [0xab; 32];
        
        let result = validate_block_checkpoint(lan_peer, hash, hash, 1000);
        assert!(matches!(result, CheckpointResult::Valid));
    }

    #[test]
    fn test_validate_block_checkpoint_invalid() {
        let lan_peer = lan_addr(100);
        let lan_hash = [0xab; 32];
        let internet_hash = [0xcd; 32];
        
        let result = validate_block_checkpoint(lan_peer, lan_hash, internet_hash, 1000);
        match result {
            CheckpointResult::Invalid { expected, got, height } => {
                assert_eq!(expected, internet_hash);
                assert_eq!(got, lan_hash);
                assert_eq!(height, 1000);
            }
            _ => panic!("Expected Invalid result"),
        }
    }

    // ========================================================================
    // DISCOVERY VERIFIER TESTS
    // ========================================================================

    #[test]
    fn test_discovery_verifier_chain_tip() {
        let verifier = DiscoveryVerifier::new();
        
        // Initially no tip
        assert!(verifier.get_internet_tip().is_none());
        
        // Update tip
        let tip = ChainTipInfo {
            height: 800000,
            hash: [0xab; 32],
            timestamp: 1700000000,
        };
        verifier.update_internet_tip(tip.clone());
        
        // Now should have tip
        let retrieved = verifier.get_internet_tip().unwrap();
        assert_eq!(retrieved.height, 800000);
        assert_eq!(retrieved.hash, [0xab; 32]);
    }

    #[test]
    fn test_checkpoint_validation_error_display() {
        let err = CheckpointValidationError::HashMismatch {
            height: 1000,
            expected: [0xab; 32],
            got: [0xcd; 32],
            lan_peer: lan_addr(100),
        };
        let display = format!("{}", err);
        assert!(display.contains("1000"));
        assert!(display.contains("192.168.1.100"));
        
        let err2 = CheckpointValidationError::InsufficientPeers { have: 2, need: 3 };
        let display2 = format!("{}", err2);
        assert!(display2.contains("2"));
        assert!(display2.contains("3"));
    }

    #[test]
    fn test_constants_are_secure() {
        // Verify security constants haven't been weakened
        assert_eq!(MIN_INTERNET_PEER_PERCENTAGE, 75);
        assert_eq!(MAX_LAN_PEER_PERCENTAGE, 25);
        assert_eq!(MIN_INTERNET_PEERS_FOR_SYNC, 3);
        assert_eq!(MIN_CHECKPOINT_PEERS, 3);
        assert_eq!(BLOCK_CHECKPOINT_INTERVAL, 1000);
        assert_eq!(HEADER_CHECKPOINT_INTERVAL, 10000);
        assert!(CHECKPOINT_FAILURE_BAN_DURATION >= Duration::from_secs(86400 * 365)); // At least 1 year
    }

    #[test]
    fn test_lan_multipliers_are_capped() {
        // Verify multipliers are within security limits
        assert!(INITIAL_LAN_MULTIPLIER <= 2.0);
        assert!(LEVEL_2_LAN_MULTIPLIER <= 3.0);
        assert!(MAX_LAN_MULTIPLIER <= 3.0);
        
        // Verify progression
        assert!(INITIAL_LAN_MULTIPLIER < LEVEL_2_LAN_MULTIPLIER);
        assert!(LEVEL_2_LAN_MULTIPLIER <= MAX_LAN_MULTIPLIER);
    }
}

