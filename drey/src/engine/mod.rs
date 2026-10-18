//! A bounded, single-owner engine. Mutation batches are validated against an
//! affected-record staging graph, then committed with their retry receipt.
//! No engine read can observe an intermediate batch. Any publication/I/O error
//! poisons the owner until explicit reopen; recovery chooses a complete seal.
mod query;
mod types;
mod write;
use crate::{Error, Graph, GraphConfig, NodeType, Properties, Result, Value};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};
pub use types::*;

const META: &str = "__drey_meta";
const BODY: &str = "__drey_document";

pub struct Engine {
    graph: Graph,
    config: DatabaseConfig,
    receipts: BTreeMap<String, ([u8; 32], Receipt)>,
    sequence: u64,
    data_charge: usize,
    poisoned: bool,
}
fn bad(m: impl Into<String>) -> Error {
    Error::InvalidRequest(m.into())
}
fn limit(m: impl Into<String>) -> Error {
    Error::ResourceLimit(m.into())
}
fn encode<T: Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| bad(e.to_string()))
}
fn decode<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| Error::Codec(e.to_string()))
}
fn public(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 256 || s.starts_with("__drey") {
        Err(bad("empty, reserved, or overlong name"))
    } else {
        Ok(())
    }
}

impl Engine {
    pub fn create(path: impl AsRef<Path>, config: DatabaseConfig) -> Result<Self> {
        validate_config(&config)?;
        let mut graph = Graph::create(path, graph_config(&config))?;
        graph.register_node_type(NodeType::new(META), None)?;
        for (name, space) in &config.types {
            graph.register_node_type(NodeType::new(name), space.as_ref().map(|s| s.dimensions))?;
        }
        let mut props = Properties::new();
        props.insert("config".into(), Value::String(encode(&config)?));
        props.insert("version".into(), Value::I64(ENGINE_VERSION as i64));
        graph.add_node(NodeType::new(META), props)?;
        graph.commit()?;
        let data_charge = write::charge(&graph);
        Ok(Self {
            graph,
            config,
            receipts: BTreeMap::new(),
            sequence: 0,
            data_charge,
            poisoned: false,
        })
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // Lock throughout discovery, index rebuild, recovery and service life.
        let mut graph = Graph::open(
            path,
            GraphConfig {
                file_lock: true,
                ..Default::default()
            },
        )?;
        let meta = graph.nodes_by_type(&NodeType::new(META))?;
        let mut config = None;
        let mut receipts = BTreeMap::new();
        let mut sequence = 0;
        for id in meta {
            let n = graph
                .node(id)?
                .ok_or_else(|| Error::Codec("missing engine metadata".into()))?;
            if let Some(Value::String(s)) = n.properties.get("config") {
                if config.is_some()
                    || n.properties.get("version") != Some(&Value::I64(ENGINE_VERSION as i64))
                {
                    return Err(Error::Codec("invalid engine metadata/version".into()));
                }
                config = Some(decode::<DatabaseConfig>(s)?);
            } else if let (Some(Value::String(s)), Some(Value::Bytes(hash))) =
                (n.properties.get("receipt"), n.properties.get("digest"))
            {
                let receipt: Receipt = decode(s)?;
                let digest: [u8; 32] = hash
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Codec("bad receipt digest".into()))?;
                sequence = sequence.max(receipt.sequence);
                if receipts
                    .insert(receipt.token.clone(), (digest, receipt))
                    .is_some()
                {
                    return Err(Error::Codec("duplicate receipt token".into()));
                }
            } else {
                return Err(Error::Codec("invalid engine metadata record".into()));
            }
        }
        let config = config.ok_or_else(|| Error::Codec("not an engine database".into()))?;
        validate_config(&config)?;
        graph.config = graph_config(&config);
        graph.store.indexed = config.indexes.iter().cloned().collect();
        graph.store.rebuild_property_indexes();
        let data_charge = write::charge(&graph);
        let engine = Self {
            graph,
            config,
            receipts,
            sequence,
            data_charge,
            poisoned: false,
        };
        engine.check_capacity(
            engine.graph.counts().0,
            engine.graph.counts().1,
            data_charge,
            engine.receipts.len(),
        )?;
        Ok(engine)
    }
    pub fn config(&self) -> &DatabaseConfig {
        &self.config
    }
    pub fn status(&self) -> Result<Status> {
        self.healthy()?;
        Ok(Status {
            version: ENGINE_VERSION,
            sequence: self.sequence,
            nodes: self.graph.counts().0 - self.receipts.len() - 1,
            edges: self.graph.counts().1,
            receipts: self.receipts.len(),
            data_charge_bytes: self.data_charge,
        })
    }
    pub fn checkpoint(&mut self) -> Result<()> {
        self.healthy()?;
        if let Err(e) = self.graph.snapshot() {
            self.poisoned = true;
            return Err(e);
        }
        Ok(())
    }
    fn healthy(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Unavailable(
                "storage outcome uncertain; close and reopen, then retry the same token".into(),
            ))
        } else {
            Ok(())
        }
    }
    fn check_capacity(
        &self,
        nodes: usize,
        edges: usize,
        bytes: usize,
        receipts: usize,
    ) -> Result<()> {
        let l = &self.config.limits;
        if nodes > l.max_nodes.saturating_add(receipts).saturating_add(1)
            || edges > l.max_edges
            || bytes > l.max_data_bytes
            || receipts > l.max_receipts
        {
            return Err(limit("database capacity"));
        }
        Ok(())
    }
}
fn graph_config(c: &DatabaseConfig) -> GraphConfig {
    let mut g = GraphConfig {
        file_lock: true,
        max_embedding_dim: Some(4096),
        ..Default::default()
    };
    for (t, k) in &c.indexes {
        g = g.with_indexed_property(NodeType::new(t), k);
    }
    g
}
fn validate_config(c: &DatabaseConfig) -> Result<()> {
    if c.types.is_empty() || c.types.len() > 128 || c.indexes.len() > 256 {
        return Err(limit("schema size"));
    }
    if c.limits.max_data_bytes < 4096usize.saturating_add(encode(c)?.len().saturating_mul(4)) {
        return Err(limit("data budget cannot hold engine metadata"));
    }
    for (name, space) in &c.types {
        public(name)?;
        if let Some(s) = space {
            validate_space(s)?;
        }
    }
    for (t, k) in &c.indexes {
        public(k)?;
        if !c.types.contains_key(t) {
            return Err(bad("index type not registered"));
        }
    }
    let l = &c.limits;
    if l.max_batch_bytes == 0
        || l.max_batch_bytes > 16 * 1024 * 1024
        || l.max_result_bytes == 0
        || l.max_result_bytes > 16 * 1024 * 1024
        || l.max_operations == 0
        || l.max_operations > 4096
        || l.max_candidates == 0
        || l.max_query_work == 0
        || l.max_data_bytes == 0
        || l.max_receipts == 0
    {
        return Err(bad("invalid limits"));
    }
    Ok(())
}
fn validate_space(s: &Space) -> Result<()> {
    if s.dimensions == 0
        || s.dimensions > 4096
        || s.model.is_empty()
        || s.model.len() > 256
        || s.revision.is_empty()
        || s.revision.len() > 256
    {
        return Err(bad("invalid embedding space"));
    }
    Ok(())
}
fn validate_vector(v: &Vector) -> Result<()> {
    validate_space(&v.space)?;
    if v.values.len() != v.space.dimensions || v.values.iter().any(|x| !x.is_finite()) {
        return Err(bad("embedding dimensions/finiteness"));
    }
    let norm = v.values.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
    if v.space.unit_normalized && (norm - 1.0).abs() > 1e-4 {
        return Err(bad("embedding is not unit normalized"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::fail;
    #[test]
    fn failed_batch_never_serves_uncertain_state_and_retry_recovers_exactly_once() {
        for (i, point) in [
            &fail::WAL_WRITE,
            &fail::WAL_SYNC,
            &fail::SEAL_WRITE,
            &fail::SEAL_PUBLISHED,
        ]
        .into_iter()
        .enumerate()
        {
            let dir = std::env::temp_dir().join(format!(
                "drey_engine_fault_{}_{}",
                std::process::id(),
                i
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let config = DatabaseConfig {
                types: BTreeMap::from([("node".into(), None)]),
                indexes: vec![],
                limits: Limits::default(),
            };
            let mut e = Engine::create(&dir, config).unwrap();
            let batch = Batch {
                token: "retry".into(),
                operations: vec![Write::CreateNode {
                    key: "a".into(),
                    node_type: "node".into(),
                    properties: Properties::new(),
                    document: None,
                    embedding: None,
                }],
            };
            fail::arm(point);
            assert!(e.write(batch.clone()).is_err());
            assert!(e.status().is_err());
            assert!(e.write(batch.clone()).is_err());
            drop(e);
            let mut e = Engine::open(&dir).unwrap();
            let receipt = e.write(batch.clone()).unwrap();
            assert_eq!(e.write(batch).unwrap(), receipt);
            assert_eq!(e.status().unwrap().nodes, 1);
            assert_eq!(e.status().unwrap().sequence, 1);
        }
    }
}
