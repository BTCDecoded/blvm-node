//! Arc utilities for reducing boilerplate
//!
//! Provides helpers for common Arc patterns.

use std::sync::Arc;

/// Clone multiple Arcs at once
///
/// # Example
/// ```rust
/// use crate::utils::arc_clone_many;
///
/// let (a, b, c) = arc_clone_many((&arc1, &arc2, &arc3));
/// ```
pub fn arc_clone_many<T1, T2, T3>(
    arcs: (&Arc<T1>, &Arc<T2>, &Arc<T3>),
) -> (Arc<T1>, Arc<T2>, Arc<T3>) {
    (Arc::clone(arcs.0), Arc::clone(arcs.1), Arc::clone(arcs.2))
}

/// Clone two Arcs at once
pub fn arc_clone_pair<T1, T2>(arcs: (&Arc<T1>, &Arc<T2>)) -> (Arc<T1>, Arc<T2>) {
    (Arc::clone(arcs.0), Arc::clone(arcs.1))
}
