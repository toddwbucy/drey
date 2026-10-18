use super::*;
use crate::mutation::{PropertyPatch, RemoveNodeMode, WeightUpdate};
use crate::store::{EdgeRecord, NodeRecord};
use crate::{EdgeId, EdgeType, Embedding, NodeId};
use std::collections::BTreeSet;

fn props_charge(p: &Properties) -> usize {
    p.iter()
        .map(|(k, v)| {
            256 + 3 * k.len()
                + match v {
                    Value::String(s) => 3 * s.len(),
                    Value::Bytes(b) => 2 * b.len(),
                    Value::List(a) => a
                        .iter()
                        .map(|s| {
                            64 + match s {
                                crate::Scalar::String(s) => 2 * s.len(),
                                _ => 0,
                            }
                        })
                        .sum(),
                    _ => 0,
                }
        })
        .sum()
}
fn node_charge(n: &NodeRecord) -> usize {
    512 + props_charge(&n.properties) + n.embedding.as_ref().map_or(0, |e| e.len() * 8)
}
fn edge_charge(e: &EdgeRecord) -> usize {
    512 + props_charge(&e.properties)
}
pub(super) fn charge(g: &Graph) -> usize {
    g.store.nodes.values().map(node_charge).sum::<usize>()
        + g.store.edges.values().map(edge_charge).sum::<usize>()
}

impl Engine {
    /// An acknowledged batch is durable as a whole, including its retry receipt.
    /// Repeating the exact token/payload returns the original IDs after restart.
    pub fn write(&mut self, batch: Batch) -> Result<Receipt> {
        self.healthy()?;
        public(&batch.token)?;
        let encoded = encode(&batch)?;
        if encoded.len() > self.config.limits.max_batch_bytes
            || batch.operations.is_empty()
            || batch.operations.len() > self.config.limits.max_operations
        {
            return Err(limit("batch size"));
        }
        let digest: [u8; 32] = Sha256::digest(encoded.as_bytes()).into();
        if let Some((old, receipt)) = self.receipts.get(&batch.token) {
            return if old == &digest {
                Ok(receipt.clone())
            } else {
                Err(Error::Conflict("retry token payload differs".into()))
            };
        }
        if self.receipts.len() >= self.config.limits.max_receipts {
            return Err(limit("receipt capacity; tokens are never silently evicted"));
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| limit("sequence exhausted"))?;
        let mut staged = self.stage(&batch)?;
        let old_charge = charge(&staged);
        let old_counts = staged.counts();
        let receipt = apply(&mut staged, &self.config, &batch, sequence)?;
        add_receipt(&mut staged, &receipt, digest)?;
        let new_charge = charge(&staged);
        let bytes = self
            .data_charge
            .checked_sub(old_charge)
            .and_then(|n| n.checked_add(new_charge))
            .ok_or_else(|| limit("data charge overflow"))?;
        let count = staged.counts();
        let current = self.graph.counts();
        self.check_capacity(
            current.0 - old_counts.0 + count.0,
            current.1 - old_counts.1 + count.1,
            bytes,
            self.receipts.len() + 1,
        )?;
        drop(staged);
        // The sole owner cannot serve a read during publication. Preflight has
        // exercised the identical operations against the full affected subgraph.
        // Unexpected errors fail closed: no consumer observes uncertain state.
        let published = (|| {
            let actual = apply(&mut self.graph, &self.config, &batch, sequence)?;
            if actual != receipt {
                return Err(Error::IndexCorruption("staging IDs diverged".into()));
            }
            add_receipt(&mut self.graph, &receipt, digest)?;
            self.graph.commit()
        })();
        if let Err(e) = published {
            self.poisoned = true;
            return Err(e);
        }
        self.sequence = sequence;
        self.data_charge = bytes;
        self.receipts.insert(batch.token, (digest, receipt.clone()));
        Ok(receipt)
    }

    fn stage(&self, batch: &Batch) -> Result<Graph> {
        let mut nodes = BTreeSet::new();
        let mut edges = BTreeSet::new();
        for op in &batch.operations {
            match op {
                Write::ReplaceNode { id, .. } | Write::DeleteNode { id } => {
                    nodes.insert(id.0);
                }
                Write::CreateEdge { from, to, .. } => {
                    for r in [from, to] {
                        if let NodeRef::Id(id) = r {
                            nodes.insert(id.0);
                        }
                    }
                }
                Write::ReplaceEdge { id, .. } | Write::DeleteEdge { id } => {
                    edges.insert(id.0);
                }
                _ => {}
            }
        }
        // Include all incident edges only for nodes being removed. This
        // makes restrict-delete validation exact without cloning the database.
        let deleted: BTreeSet<_> = batch
            .operations
            .iter()
            .filter_map(|op| match op {
                Write::DeleteNode { id } => Some(id.0),
                _ => None,
            })
            .collect();
        for id in &deleted {
            for adj in [&self.graph.store.out_adj, &self.graph.store.in_adj] {
                if let Some(types) = adj.get(id) {
                    for ids in types.values() {
                        for edge in ids {
                            edges.insert(*edge);
                            if edges.len() > self.config.limits.max_query_work {
                                return Err(limit("affected edge staging"));
                            }
                        }
                    }
                }
            }
        }
        for id in &edges {
            let e = self
                .graph
                .store
                .edges
                .get(id)
                .ok_or(Error::EdgeNotFound(EdgeId(*id)))?;
            nodes.insert(e.from);
            nodes.insert(e.to);
        }
        let mut staged = Graph::in_memory(self.graph.config.clone());
        staged.store.node_types = self.graph.store.node_types.clone();
        staged.store.edge_types = self.graph.store.edge_types.clone();
        staged.store.embedding_dim = self.graph.store.embedding_dim.clone();
        staged.store.next_node_id = self.graph.store.next_node_id;
        staged.store.next_edge_id = self.graph.store.next_edge_id;
        let mut bytes = 0;
        for id in nodes {
            let n = self
                .graph
                .store
                .nodes
                .get(&id)
                .ok_or(Error::NodeNotFound(NodeId(id)))?;
            if self.graph.store.node_types.label(n.node_type) == Some(META) {
                return Err(bad("private engine record"));
            }
            bytes += node_charge(n);
            if bytes > self.config.limits.max_batch_bytes.saturating_mul(8) {
                return Err(limit("affected record staging"));
            }
            staged.store.insert_node_raw(id, n.clone());
        }
        for id in edges {
            let e = &self.graph.store.edges[&id];
            bytes += edge_charge(e);
            if bytes > self.config.limits.max_batch_bytes.saturating_mul(8) {
                return Err(limit("affected record staging"));
            }
            staged.store.insert_edge_raw(id, e.clone());
        }
        staged.store.sort_indexes();
        Ok(staged)
    }
}

fn validate_props(p: &Properties) -> Result<()> {
    for (k, v) in p {
        public(k)?;
        let finite = match v {
            Value::F64(x) => x.is_finite(),
            Value::List(v) => v
                .iter()
                .all(|s| !matches!(s,crate::Scalar::F64(x) if !x.is_finite())),
            _ => true,
        };
        if !finite || !v.is_valid() {
            return Err(bad("invalid property"));
        }
    }
    Ok(())
}
fn contents(p: &Properties, document: &Option<String>) -> Result<Properties> {
    validate_props(p)?;
    let mut out = p.clone();
    if let Some(s) = document {
        let _: serde_json::Value =
            serde_json::from_str(s).map_err(|e| bad(format!("document JSON: {e}")))?;
        out.insert(BODY.into(), Value::String(s.clone()));
    }
    Ok(out)
}
fn vector(
    g: &mut Graph,
    c: &DatabaseConfig,
    id: NodeId,
    t: &str,
    v: &Option<Vector>,
) -> Result<()> {
    if let Some(v) = v {
        validate_vector(v)?;
        if c.types.get(t).and_then(Option::as_ref) != Some(&v.space) {
            return Err(bad("embedding space does not match the node type"));
        }
        g.set_node_embedding(id, Embedding::new(v.values.clone()))
    } else {
        g.clear_node_embedding(id)
    }
}
fn patch(old: &Properties, new: Properties) -> PropertyPatch {
    let mut p = PropertyPatch(old.keys().map(|k| (k.clone(), None)).collect());
    for (k, v) in new {
        p.0.insert(k, Some(v));
    }
    p
}
fn apply(g: &mut Graph, c: &DatabaseConfig, b: &Batch, sequence: u64) -> Result<Receipt> {
    let mut r = Receipt {
        token: b.token.clone(),
        sequence,
        nodes: BTreeMap::new(),
        edges: BTreeMap::new(),
    };
    for op in &b.operations {
        match op {
            Write::CreateNode {
                key,
                node_type,
                properties,
                document,
                embedding,
            } => {
                public(key)?;
                public(node_type)?;
                if r.nodes.contains_key(key) {
                    return Err(bad("duplicate created-node key"));
                }
                if !c.types.contains_key(node_type) {
                    return Err(bad("unregistered node type"));
                }
                let id = g.add_node(NodeType::new(node_type), contents(properties, document)?)?;
                vector(g, c, id, node_type, embedding)?;
                r.nodes.insert(key.clone(), id);
            }
            Write::ReplaceNode {
                id,
                properties,
                document,
                embedding,
            } => {
                let n = g.node(*id)?.ok_or(Error::NodeNotFound(*id))?;
                public(n.node_type.as_str())?;
                let properties = contents(properties, document)?;
                vector(g, c, *id, n.node_type.as_str(), embedding)?;
                g.update_node_properties(*id, patch(&n.properties, properties))?;
            }
            Write::DeleteNode { id } => {
                g.remove_node(*id, RemoveNodeMode::RejectIfEdgesExist)?;
            }
            Write::CreateEdge {
                key,
                from,
                to,
                edge_type,
                weight,
                properties,
            } => {
                public(key)?;
                public(edge_type)?;
                validate_props(properties)?;
                // Interned labels survive edge deletion, so bound the lifetime
                // vocabulary independently of the current edge-count quota.
                if g.store.edge_types.get(edge_type).is_none()
                    && g.store.edge_types.labels().len() >= 128
                {
                    return Err(limit("edge type vocabulary"));
                }
                if r.edges.contains_key(key) {
                    return Err(bad("duplicate created-edge key"));
                }
                let resolve = |n: &NodeRef| -> Result<NodeId> {
                    match n {
                        NodeRef::Id(id) => Ok(*id),
                        NodeRef::Created(k) => r
                            .nodes
                            .get(k)
                            .copied()
                            .ok_or_else(|| bad("created node must precede its edge")),
                    }
                };
                let id = g.add_edge(
                    resolve(from)?,
                    resolve(to)?,
                    EdgeType::new(edge_type),
                    *weight,
                    properties.clone(),
                )?;
                r.edges.insert(key.clone(), id);
            }
            Write::ReplaceEdge {
                id,
                weight,
                properties,
            } => {
                validate_props(properties)?;
                let e = g.edge(*id)?.ok_or(Error::EdgeNotFound(*id))?;
                g.update_edge_weight(*id, WeightUpdate::set(*weight))?;
                g.update_edge_properties(*id, patch(&e.properties, properties.clone()))?;
            }
            Write::DeleteEdge { id } => g.remove_edge(*id)?,
        }
    }
    Ok(r)
}
fn add_receipt(g: &mut Graph, r: &Receipt, digest: [u8; 32]) -> Result<()> {
    let p = BTreeMap::from([
        ("receipt".into(), Value::String(encode(r)?)),
        ("digest".into(), Value::Bytes(digest.to_vec())),
    ]);
    g.add_node(NodeType::new(META), p)?;
    Ok(())
}
