//! HTTP BIP70 Payment Protocol Handlers
//!
//! Provides HTTP endpoints for BIP70 payment protocol.
//! Requires `bip70-http` feature flag.

use crate::payment::processor::PaymentError;
#[cfg(feature = "bip70-http")]
use crate::payment::processor::PaymentProcessor;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::http::StatusCode;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Handle HTTP payment request creation
#[cfg(feature = "bip70-http")]
pub async fn handle_create_payment_request(
    processor: Arc<PaymentProcessor>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, PaymentError> {
    use bllvm_protocol::payment::{PaymentOutput, PaymentRequest};

    // Only accept POST
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::from("Method not allowed")))
            .unwrap());
    }

    // Read request body
    let body = req.collect().await.map_err(|e| {
        PaymentError::ProcessingError(format!("Failed to read request body: {}", e))
    })?;
    let body_bytes = body.to_bytes();

    // Parse payment request parameters (JSON or form data)
    // For now, expect JSON with outputs and optional merchant_data
    let params: serde_json::Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        PaymentError::ProcessingError(format!("Failed to parse request body: {}", e))
    })?;

    // Extract outputs
    let outputs: Vec<PaymentOutput> = params
        .get("outputs")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or_else(|| PaymentError::ProcessingError("Missing 'outputs' field".to_string()))?;

    // Extract optional merchant_data
    let merchant_data = params.get("merchant_data").and_then(|v| {
        if v.is_string() {
            Some(hex::decode(v.as_str().unwrap()).ok()?)
        } else if v.is_array() {
            Some(
                v.as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|n| n.as_u64().map(|u| u as u8))
                    .collect(),
            )
        } else {
            None
        }
    });

    // Create payment request
    let payment_request = processor
        .create_payment_request(outputs, merchant_data, None)
        .await?;

    // Serialize payment request (BIP70 format)
    let serialized = bincode::serialize(&payment_request).map_err(|e| {
        PaymentError::ProcessingError(format!("Failed to serialize payment request: {}", e))
    })?;

    // Return with proper content type
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/bitcoin-paymentrequest")
        .body(Full::new(Bytes::from(serialized)))
        .unwrap())
}

/// Handle HTTP payment request retrieval
#[cfg(feature = "bip70-http")]
pub async fn handle_get_payment_request(
    processor: Arc<PaymentProcessor>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, PaymentError> {
    // Only accept GET
    if req.method() != Method::GET {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::from("Method not allowed")))
            .unwrap());
    }

    // Extract payment ID from path (e.g., /api/v1/payment/request/{id})
    let path = req.uri().path();
    let payment_id = path
        .strip_prefix("/api/v1/payment/request/")
        .ok_or_else(|| PaymentError::ProcessingError("Invalid path".to_string()))?;

    // Get payment request
    let payment_request = processor.get_payment_request(payment_id).await?;

    // Serialize payment request
    let serialized = bincode::serialize(&payment_request).map_err(|e| {
        PaymentError::ProcessingError(format!("Failed to serialize payment request: {}", e))
    })?;

    // Return with proper content type
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/bitcoin-paymentrequest")
        .body(Full::new(Bytes::from(serialized)))
        .unwrap())
}

/// Handle HTTP payment submission
#[cfg(feature = "bip70-http")]
pub async fn handle_submit_payment(
    processor: Arc<PaymentProcessor>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, PaymentError> {
    use bllvm_protocol::payment::Payment;

    // Only accept POST
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::from("Method not allowed")))
            .unwrap());
    }

    // Extract payment ID from query string BEFORE reading body (which moves req)
    let payment_id = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|p| {
                if p.starts_with("payment_id=") {
                    Some(p.strip_prefix("payment_id=").unwrap().to_string())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| PaymentError::ProcessingError("Missing payment_id".to_string()))?;

    // Read request body
    let body = req.collect().await.map_err(|e| {
        PaymentError::ProcessingError(format!("Failed to read request body: {}", e))
    })?;
    let body_bytes = body.to_bytes();

    // Parse payment (BIP70 format)
    let payment: Payment = bincode::deserialize(&body_bytes)
        .map_err(|e| PaymentError::ProcessingError(format!("Failed to parse payment: {}", e)))?;

    // Process payment
    let ack = processor.process_payment(payment, payment_id, None).await?;

    // Serialize payment ACK
    let serialized = bincode::serialize(&ack).map_err(|e| {
        PaymentError::ProcessingError(format!("Failed to serialize payment ACK: {}", e))
    })?;

    // Return with proper content type
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/bitcoin-paymentack")
        .body(Full::new(Bytes::from(serialized)))
        .unwrap())
}

/// Handle payment routes (routes to appropriate handler)
#[cfg(feature = "bip70-http")]
pub async fn handle_payment_routes(
    processor: Arc<PaymentProcessor>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, PaymentError> {
    let path = req.uri().path();
    let method = req.method().clone();

    match (method, path) {
        (Method::POST, "/api/v1/payment/request") => {
            handle_create_payment_request(processor, req).await
        }
        (Method::GET, path) if path.starts_with("/api/v1/payment/request/") => {
            handle_get_payment_request(processor, req).await
        }
        (Method::POST, "/api/v1/payment") => handle_submit_payment(processor, req).await,
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not found")))
            .unwrap()),
    }
}

/// Stub implementation when feature not enabled
#[cfg(not(feature = "bip70-http"))]
pub async fn handle_payment_routes(
    _processor: Arc<PaymentProcessor>,
    _req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, PaymentError> {
    use http_body_util::Full;
    Ok(Response::builder()
        .status(StatusCode::NOT_IMPLEMENTED)
        .body(Full::new(Bytes::from(
            "HTTP BIP70 not enabled. Compile with --features bip70-http",
        )))
        .unwrap())
}
