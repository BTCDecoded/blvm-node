# Dandelion++ Relay Specification

**Node design (not consensus).** Dandelion++ is a relay strategy for improving transaction origin privacy. Nodes that do not implement it still interoperate. The base protocol (broadcast tx to peers) is consensus; *how* you choose to relay is a design choice.

This document is the full specification for blvm-node's Dandelion implementation. The Orange Paper Section 10.6 retains a short summary and Implementation Invariants for spec-lock verification.

---

## Adversary Model

Passive observer capable of monitoring network traffic, operating nodes, and performing graph analysis.

## k-Anonymity Definition

A transaction $tx$ satisfies k-anonymity if, from the adversary's perspective, $tx$ could have originated from at least $k$ distinct nodes with equal probability.

Formally, let:
- $O$ = set of nodes that could have originated $tx$ (from adversary's view)
- $P(O = N_i | \text{Evidence})$ = probability that node $N_i$ originated $tx$ given observed evidence

Then $tx$ has **k-anonymity** if:
- $|O| \geq k$
- $\forall N_i, N_j \in O: P(O = N_i | \text{Evidence}) = P(O = N_j | \text{Evidence})$

## Stem Phase Parameters

- $p_{\text{fluff}} \in [0, 1]$: Probability of transitioning to fluff at each hop (default: 0.1)
- $\text{max\_stem\_hops} \in \mathbb{N}$: Maximum number of stem hops before forced fluff (default: 2)
- $\text{stem\_timeout} \in \mathbb{R}^+$: Maximum duration (seconds) in stem phase before timeout fluff

## Stem Phase Algorithm

$$\text{stem\_phase\_relay}(tx, \text{current\_peer}, \text{peers}) \rightarrow \text{Option}<\text{Peer}>$$

1. If $tx$ already in stem phase:
   - If $\text{elapsed\_time}(tx) > \text{stem\_timeout}$: return $\bot$ (fluff via timeout)
   - If $\text{hop\_count}(tx) \geq \text{max\_stem\_hops}$: return $\bot$ (fluff via hop limit)
   - If $\text{random}() < p_{\text{fluff}}$: return $\bot$ (fluff via probability)
   - Otherwise: $\text{advance\_stem}(tx)$ → return next peer

2. Else: $\text{start\_stem\_phase}(tx)$ → return next peer

**Fluff Phase**: When algorithm returns $\bot$, transaction enters fluff phase and is broadcast to all peers (standard Bitcoin relay).

## Theorems

**Theorem 1 (Stem Phase Anonymity)**: During the stem phase, if the adversary observes a transaction at node $N_i$, the set of possible originators includes all nodes that have been on the stem path up to $N_i$.

**Proof Sketch**: The adversary cannot distinguish between:
1. $tx$ originated at $N_i$ and is in its first stem hop
2. $tx$ originated at any previous node $N_j$ ($j < i$) and is being forwarded

The random peer selection ensures uniform probability distribution over all possible originators in the path.

**Theorem 2 (Minimum k-Anonymity)**: For a stem path of length $h$ hops, the minimum k-anonymity is $k \geq h + 1$.

**Proof**: A stem path $N_0 \rightarrow N_1 \rightarrow \ldots \rightarrow N_h$ contains $h + 1$ nodes. From the adversary's perspective at $N_h$, any of these $h + 1$ nodes could have originated $tx$. Therefore, $k \geq h + 1$.

**Corollary**: With $\text{max\_stem\_hops} = 2$, we guarantee $k \geq 3$ (3-anonymity).

**Theorem 3 (Timeout Guarantee)**: Even if the adversary controls all peers except the originator, the stem phase will terminate within $\text{stem\_timeout}$ seconds.

**Proof**: The timeout check ensures $tx$ transitions to fluff phase within $\text{stem\_timeout}$ seconds regardless of peer behavior.

**Theorem 4 (No Premature Broadcast)**: During the stem phase, a transaction is never broadcast to multiple peers simultaneously.

**Proof**: The algorithm returns either a single peer (stem relay) or $\bot$ (transition to fluff). The fluff phase is the only mechanism for broadcast.

## Verification

blvm-node `dandelion.rs` has `#[spec_locked("10.6")]` on the key functions. The Orange Paper Section 10.6 retains the Implementation Invariants for spec-lock contract enrichment.
