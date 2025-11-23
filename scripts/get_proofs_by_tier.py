#!/usr/bin/env python3
"""Identify Kani proofs by tier (fast/medium/slow) based on unwind bounds."""
import re
import os
import sys

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
                                        # Check unwind bound (look ahead up to 15 lines from proof)
                                        unwind = None
                                        for k in range(i, min(len(lines), i+15)):
                                            if 'kani::unwind(' in lines[k]:
                                                # Try to match both direct numbers and constants
                                                unwind_match = re.search(r'unwind\((\d+)\)', lines[k])
                                                if unwind_match:
                                                    unwind = int(unwind_match.group(1))
                                                    break
                                                # Also check for unwind_bounds constants (these are typically fast)
                                                if 'unwind_bounds::' in lines[k]:
                                                    # Assume constants are fast/medium, treat as fast for now
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

if tier == 'fast':
    proofs = fast_proofs
elif tier == 'fast_medium':
    proofs = fast_proofs + medium_proofs
elif tier == 'all':
    proofs = fast_proofs + medium_proofs + slow_proofs
else:
    proofs = []

# Output as space-separated list for shell script
print(' '.join(sorted(proofs)))

