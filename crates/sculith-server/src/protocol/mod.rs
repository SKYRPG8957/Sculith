pub mod bedrock;
pub mod catalog;
mod http_client;
pub mod java;

use serde::Deserialize;

pub const REFERENCES_JSON: &str = include_str!("../../../../protocol-targets.json");

#[derive(Debug, Deserialize)]
pub struct References {
    pub java: Reference,
    pub bedrock: Reference,
}

#[derive(Debug, Deserialize)]
pub struct Reference {
    pub version: String,
    pub protocol: i32,
}

pub fn references() -> References {
    serde_json::from_str(REFERENCES_JSON).expect("bundled protocol references must be valid")
}

#[cfg(test)]
mod chunk_bench;
