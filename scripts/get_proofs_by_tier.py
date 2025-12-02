#!/usr/bin/env python3
"""Identify Kani proofs by tier (strong/fast/medium/slow) based on criticality and unwind bounds.

Strong tier: Critical node proofs that MUST run on every push.
Fast tier: unwind <= 3 or no unwind
Medium tier: unwind 4-9
Slow tier: unwind >= 10
"""
import re
import os
import sys

# STRONG TIER: Critical proofs that run on EVERY push
# These are the minimum set for "formally verified" node status
# Any change to these would break Bitcoin node correctness
STRONG_TIER_PROOFS = {
    # Protocol Message Parsing (DoS prevention, security)
    'verify_message_header_parsing',  # Header field extraction (critical for message parsing)
    'verify_checksum_rejection',  # Invalid checksum rejection (prevents corrupted messages)
    'verify_message_size_limits',  # Size limit enforcement (DoS prevention)
    
    # Cryptographic Operations (security foundation)
    'verify_sha256_determinism',  # SHA256 determinism (critical for hashing)
    'verify_sha256_length',  # SHA256 output length (critical for hash validation)
    'verify_double_sha256_determinism',  # Double SHA256 determinism (Bitcoin standard)
    'verify_double_sha256_length',  # Double SHA256 output length (Bitcoin standard)
    
    # UTXO Store Operations (consensus correctness)
    'verify_utxo_uniqueness',  # UTXO uniqueness (prevents double-spending)
    'verify_add_remove_consistency',  # Add/remove consistency (UTXO set integrity)
    'verify_spent_output_tracking',  # Spent output tracking (prevents double-spending)
}

fast_proofs = []  # unwind <= 3 or no unwind
medium_proofs = []  # unwind 4-9
slow_proofs = []  # unwind >= 10

for root, dirs, files in os.walk('src'):
    for file in files:
        if file.endswith('.rs'):
            path = os.path.join(root, file)
            try:
                with open(path, 'r') as f:
                    lines = f.readlines()
                    for i, line in enumerate(lines):
                        if '#[kani::proof]' in line:
                            # Look for function name in next few lines
                            for j in range(i, min(len(lines), i+10)):
                                if 'fn ' in lines[j]:
                                    # Match any function name after #[kani::proof]
                                    func_match = re.search(r'fn\s+(\w+)', lines[j])
                                    if func_match:
                                        proof_name = func_match.group(1)
                                        
                                        # Skip if this is a strong tier proof (handled separately)
                                        if proof_name in STRONG_TIER_PROOFS:
                                            break
                                        
                                        # Check unwind bound (look ahead up to 15 lines from proof)
                                        unwind = None
                                        for k in range(i, min(len(lines), i+15)):
                                            if 'kani::unwind(' in lines[k]:
                                                # Try to match both direct numbers and constants
                                                unwind_match = re.search(r'unwind\((\d+)\)', lines[k])
                                                if unwind_match:
                                                    unwind = int(unwind_match.group(1))
                                                    break
                                                # Also check for unwind_bounds constants
                                                if 'unwind_bounds::' in lines[k]:
                                                    # Parse constant name to estimate tier
                                                    const_match = re.search(r'unwind_bounds::(\w+)', lines[k])
                                                    if const_match:
                                                        const_name = const_match.group(1).upper()
                                                        # Categorize based on constant name patterns
                                                        if 'SIMPLE' in const_name and ('HASH' in const_name or 'UTXO' in const_name):
                                                            # SIMPLE_HASH, SIMPLE_UTXO are typically 3 (fast)
                                                            unwind = 3
                                                        elif 'HEADER' in const_name or 'CHECKSUM' in const_name:
                                                            # HEADER_PARSING, CHECKSUM are typically 3 (fast)
                                                            unwind = 3
                                                        elif 'SIMPLE' in const_name and ('MESSAGE' in const_name or 'STATE' in const_name or 'RPC' in const_name or 'MEMPOOL' in const_name):
                                                            # SIMPLE_MESSAGE, SIMPLE_STATE, SIMPLE_RPC, SIMPLE_MEMPOOL are typically 5 (medium)
                                                            unwind = 5
                                                        elif 'COMPLEX' in const_name:
                                                            # COMPLEX_MESSAGE, COMPLEX_STATE, COMPLEX_RPC, COMPLEX_HASH, COMPLEX_UTXO, COMPLEX_MEMPOOL are typically 10+ (slow)
                                                            unwind = 10
                                                        elif 'UTXO_SET' in const_name:
                                                            # UTXO_SET is 15 (slow)
                                                            unwind = 15
                                                        else:
                                                            # Default to fast for unknown constants
                                                            unwind = 3
                                                    break
                                        
                                        if unwind is None:
                                            fast_proofs.append(proof_name)
                                        elif unwind <= 3:
                                            fast_proofs.append(proof_name)
                                        elif unwind <= 9:
                                            medium_proofs.append(proof_name)
                                        else:
                                            slow_proofs.append(proof_name)
                                        break
            except Exception:
                pass

tier = sys.argv[1] if len(sys.argv) > 1 else 'all'

if tier == 'strong':
    # Strong tier: Critical proofs only (always run)
    proofs = sorted(STRONG_TIER_PROOFS)
elif tier == 'fast':
    proofs = fast_proofs
elif tier == 'fast_medium':
    proofs = fast_proofs + medium_proofs
elif tier == 'all':
    # All tier includes strong tier + fast + medium + slow
    proofs = sorted(STRONG_TIER_PROOFS) + fast_proofs + medium_proofs + slow_proofs
else:
    proofs = []

# Output as space-separated list for shell script
print(' '.join(sorted(proofs)))
