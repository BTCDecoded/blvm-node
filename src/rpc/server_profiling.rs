//! Temporary profiling instrumentation for RPC server
//! 
//! This file contains profiling code that can be conditionally compiled
//! to measure where time is spent in the RPC request/response cycle.
//! 
//! Usage: Compile with `--features profiling` to enable timing measurements.
//! 
//! This is TEMPORARY - remove after profiling is complete.

#[cfg(feature = "profiling")]
use std::time::Instant;

#[cfg(feature = "profiling")]
pub struct RpcTiming {
    pub http_parse_start: Instant,
    pub http_parse_end: Option<Instant>,
    pub json_parse_start: Option<Instant>,
    pub json_parse_end: Option<Instant>,
    pub method_start: Option<Instant>,
    pub method_end: Option<Instant>,
    pub json_serialize_start: Option<Instant>,
    pub json_serialize_end: Option<Instant>,
    pub http_build_start: Option<Instant>,
    pub http_build_end: Option<Instant>,
}

#[cfg(feature = "profiling")]
impl RpcTiming {
    pub fn new() -> Self {
        Self {
            http_parse_start: Instant::now(),
            http_parse_end: None,
            json_parse_start: None,
            json_parse_end: None,
            method_start: None,
            method_end: None,
            json_serialize_start: None,
            json_serialize_end: None,
            http_build_start: None,
            http_build_end: None,
        }
    }

    pub fn report(&self, method: &str) {
        let total = self.http_build_end
            .unwrap_or_else(Instant::now)
            .duration_since(self.http_parse_start)
            .as_micros();

        let http_parse = self.http_parse_end
            .map(|end| end.duration_since(self.http_parse_start).as_micros())
            .unwrap_or(0);

        let json_parse = self.json_parse_start
            .and_then(|start| self.json_parse_end.map(|end| end.duration_since(start).as_micros()))
            .unwrap_or(0);

        let method_exec = self.method_start
            .and_then(|start| self.method_end.map(|end| end.duration_since(start).as_micros()))
            .unwrap_or(0);

        let json_serialize = self.json_serialize_start
            .and_then(|start| self.json_serialize_end.map(|end| end.duration_since(start).as_micros()))
            .unwrap_or(0);

        let http_build = self.http_build_start
            .and_then(|start| self.http_build_end.map(|end| end.duration_since(start).as_micros()))
            .unwrap_or(0);

        eprintln!(
            "RPC_TIMING|{}|total={}µs|http_parse={}µs|json_parse={}µs|method={}µs|json_serialize={}µs|http_build={}µs",
            method, total, http_parse, json_parse, method_exec, json_serialize, http_build
        );
    }
}

#[cfg(not(feature = "profiling"))]
pub struct RpcTiming;

#[cfg(not(feature = "profiling"))]
impl RpcTiming {
    pub fn new() -> Self {
        Self
    }

    pub fn report(&self, _method: &str) {
        // No-op when profiling is disabled
    }
}

