//! Tests for circuit breaker

use bllvm_node::utils::circuit_breaker::{CircuitBreaker, CircuitState};
use std::time::Duration;
use std::thread;

#[test]
fn test_circuit_breaker_creation() {
    let breaker = CircuitBreaker::new(5, Duration::from_secs(10));
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn test_circuit_breaker_with_success_threshold() {
    let breaker = CircuitBreaker::with_success_threshold(
        5,
        3,
        Duration::from_secs(10),
    );
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn test_circuit_breaker_closed_state() {
    let breaker = CircuitBreaker::new(5, Duration::from_secs(10));
    assert_eq!(breaker.state(), CircuitState::Closed);
    assert!(breaker.allow_request());
}

#[test]
fn test_circuit_breaker_record_success() {
    let breaker = CircuitBreaker::new(5, Duration::from_secs(10));
    
    // Should start closed
    assert_eq!(breaker.state(), CircuitState::Closed);
    
    // Record success should keep it closed
    breaker.record_success();
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn test_circuit_breaker_record_failure() {
    let breaker = CircuitBreaker::new(5, Duration::from_secs(10));
    
    // Record failures up to threshold
    for _ in 0..4 {
        breaker.record_failure();
        assert_eq!(breaker.state(), CircuitState::Closed);
    }
    
    // 5th failure should open circuit
    breaker.record_failure();
    assert_eq!(breaker.state(), CircuitState::Open);
}

#[test]
fn test_circuit_breaker_open_state_blocks_requests() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(1));
    
    // Open the circuit
    for _ in 0..3 {
        breaker.record_failure();
    }
    
    assert_eq!(breaker.state(), CircuitState::Open);
    assert!(!breaker.allow_request());
}

#[test]
fn test_circuit_breaker_timeout_transition_to_half_open() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(1));
    
    // Open the circuit
    for _ in 0..3 {
        breaker.record_failure();
    }
    
    assert_eq!(breaker.state(), CircuitState::Open);
    
    // Wait for timeout
    thread::sleep(Duration::from_secs(2));
    
    // Should transition to half-open
    assert!(breaker.allow_request());
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
}

#[test]
fn test_circuit_breaker_half_open_to_closed() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(1));
    
    // Open the circuit
    for _ in 0..3 {
        breaker.record_failure();
    }
    
    // Wait for timeout
    thread::sleep(Duration::from_secs(2));
    breaker.allow_request(); // Transition to half-open
    
    // Record success should close circuit
    breaker.record_success();
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn test_circuit_breaker_half_open_to_open() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(1));
    
    // Open the circuit
    for _ in 0..3 {
        breaker.record_failure();
    }
    
    // Wait for timeout
    thread::sleep(Duration::from_secs(2));
    breaker.allow_request(); // Transition to half-open
    
    // Record failure in half-open should open again
    breaker.record_failure();
    assert_eq!(breaker.state(), CircuitState::Open);
}

#[test]
fn test_circuit_breaker_success_threshold() {
    let breaker = CircuitBreaker::with_success_threshold(
        3,
        2, // Need 2 successes to close
        Duration::from_secs(1),
    );
    
    // Open the circuit
    for _ in 0..3 {
        breaker.record_failure();
    }
    
    // Wait for timeout
    thread::sleep(Duration::from_secs(2));
    breaker.allow_request(); // Transition to half-open
    
    // One success should not close (need 2)
    breaker.record_success();
    assert_eq!(breaker.state(), CircuitState::HalfOpen);
    
    // Second success should close
    breaker.record_success();
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn test_circuit_breaker_reset() {
    let breaker = CircuitBreaker::new(3, Duration::from_secs(10));
    
    // Open the circuit
    for _ in 0..3 {
        breaker.record_failure();
    }
    
    assert_eq!(breaker.state(), CircuitState::Open);
    
    // Reset should close circuit
    breaker.reset();
    assert_eq!(breaker.state(), CircuitState::Closed);
}

#[test]
fn test_circuit_breaker_state_variants() {
    // Test all state variants
    let states = vec![
        CircuitState::Closed,
        CircuitState::Open,
        CircuitState::HalfOpen,
    ];

    for state in states {
        match state {
            CircuitState::Closed => assert!(true),
            CircuitState::Open => assert!(true),
            CircuitState::HalfOpen => assert!(true),
        }
    }
}

