//! Arc utility tests
//!
//! Tests for Arc cloning helpers.

use bllvm_node::utils::arc::{arc_clone_many, arc_clone_pair};
use std::sync::Arc;

#[test]
fn test_arc_clone_pair() {
    let arc1 = Arc::new(42);
    let arc2 = Arc::new("test".to_string());

    let (cloned1, cloned2) = arc_clone_pair((&arc1, &arc2));

    assert_eq!(*cloned1, 42);
    assert_eq!(*cloned2, "test");

    // Verify they're separate Arc instances pointing to same data
    assert_eq!(Arc::strong_count(&arc1), 2); // original + cloned1
    assert_eq!(Arc::strong_count(&arc2), 2); // original + cloned2
}

#[test]
fn test_arc_clone_many() {
    let arc1 = Arc::new(1);
    let arc2 = Arc::new(2);
    let arc3 = Arc::new(3);

    let (cloned1, cloned2, cloned3) = arc_clone_many((&arc1, &arc2, &arc3));

    assert_eq!(*cloned1, 1);
    assert_eq!(*cloned2, 2);
    assert_eq!(*cloned3, 3);

    // Verify reference counts
    assert_eq!(Arc::strong_count(&arc1), 2);
    assert_eq!(Arc::strong_count(&arc2), 2);
    assert_eq!(Arc::strong_count(&arc3), 2);
}

#[test]
fn test_arc_clone_pair_different_types() {
    let arc1 = Arc::new(vec![1, 2, 3]);
    let arc2 = Arc::new(true);

    let (cloned1, cloned2) = arc_clone_pair((&arc1, &arc2));

    assert_eq!(*cloned1, vec![1, 2, 3]);
    assert_eq!(*cloned2, true);
}

#[test]
fn test_arc_clone_many_different_types() {
    let arc1 = Arc::new(100u64);
    let arc2 = Arc::new("hello".to_string());
    let arc3 = Arc::new(vec![1.0, 2.0, 3.0]);

    let (cloned1, cloned2, cloned3) = arc_clone_many((&arc1, &arc2, &arc3));

    assert_eq!(*cloned1, 100u64);
    assert_eq!(*cloned2, "hello");
    assert_eq!(*cloned3, vec![1.0, 2.0, 3.0]);
}
