use super::*;
use crate::{Direction, NodeId, Scalar};
use std::collections::{BTreeSet, VecDeque};

type WalkResult = (BTreeSet<u64>, Vec<(u64, u64, usize)>);

struct Work {
    used: usize,
    max: usize,
}
impl Work {
    fn tick(&mut self) -> Result<()> {
        self.used += 1;
        if self.used > self.max {
            Err(limit("query work"))
        } else {
            Ok(())
        }
    }
}
fn matches(value: &Scalar, p: &Predicate) -> bool {
    match p {
        Predicate::Eq(s) => value.total_order(s).is_eq(),
        Predicate::Range { min, max } => {
            min.as_ref().is_none_or(|m| !value.total_order(m).is_lt())
                && max.as_ref().is_none_or(|m| !value.total_order(m).is_gt())
        }
    }
}
fn validate_walk(w: &Walk) -> Result<()> {
    if w.max_hops > 8 || w.edge_types.len() > 128 || w.min_weight.is_some_and(|w| !w.is_finite()) {
        return Err(bad("walk limits/weight"));
    }
    for t in &w.edge_types {
        public(t)?;
    }
    Ok(())
}
impl Engine {
    /// One resident operation: selection, predicates, optional ranking, winner
    /// limit, relationship expansion, then projection. Bounds fail the request
    /// explicitly rather than returning an incomplete success.
    pub fn query(&self, q: Query) -> Result<QueryResult> {
        self.healthy()?;
        if q.limit == 0 || q.limit > self.config.limits.max_candidates || q.filters.len() > 32 {
            return Err(bad("query limit/filter count"));
        }
        for p in [&q.projection, &q.related_projection] {
            if let Some(fields) = &p.fields {
                if fields.len() > 256 {
                    return Err(limit("projection fields"));
                }
                for f in fields {
                    public(f)?;
                }
            }
        }
        for f in &q.filters {
            public(&f.key)?;
            let scalars: Vec<&Scalar> = match &f.predicate {
                Predicate::Eq(s) => vec![s],
                Predicate::Range { min, max } => min.iter().chain(max.iter()).collect(),
            };
            if scalars
                .iter()
                .any(|s| matches!(s,Scalar::F64(x) if !x.is_finite()))
            {
                return Err(bad("non-finite predicate"));
            }
        }
        if let Some(r) = &q.rank {
            validate_vector(&r.vector)?;
            if r.k == 0 || r.k > self.config.limits.max_candidates {
                return Err(bad("rank k"));
            }
            if !self
                .config
                .types
                .values()
                .any(|s| s.as_ref() == Some(&r.vector.space))
            {
                return Err(bad("unknown query embedding space"));
            }
        }
        if let Some(w) = &q.expand {
            validate_walk(w)?;
        }
        let mut work = Work {
            used: 0,
            max: q
                .work_limit
                .unwrap_or(self.config.limits.max_query_work)
                .min(self.config.limits.max_query_work),
        };
        let mut candidates = BTreeSet::new();
        let mut consider = |id: u64, work: &mut Work| -> Result<()> {
            work.tick()?;
            let n = self
                .graph
                .store
                .nodes
                .get(&id)
                .ok_or(Error::NodeNotFound(NodeId(id)))?;
            if self.graph.store.node_types.label(n.node_type) == Some(META) {
                return Err(bad("private engine record"));
            }
            if q.filters.iter().all(|f| {
                n.properties
                    .get(&f.key)
                    .and_then(Value::as_scalar)
                    .is_some_and(|v| matches(&v, &f.predicate))
            }) {
                candidates.insert(id);
                if candidates.len() > self.config.limits.max_candidates {
                    return Err(limit("query candidates"));
                }
            }
            Ok(())
        };
        match &q.selection {
            Selection::Type { node_type } => {
                public(node_type)?;
                if !self.config.types.contains_key(node_type) {
                    return Err(bad("unregistered query type"));
                }
                let tid = self
                    .graph
                    .store
                    .require_registered(&NodeType::new(node_type))?;
                let indexed = q.filters.iter().find(|f| {
                    self.graph
                        .store
                        .indexed
                        .contains(&(node_type.clone(), f.key.clone()))
                });
                if let Some(f) = indexed {
                    if let Some(tree) = self.graph.store.prop_index.get(&(tid, f.key.clone())) {
                        for (key, ids) in tree {
                            work.tick()?;
                            if matches(&key.0, &f.predicate) {
                                for id in ids {
                                    consider(*id, &mut work)?;
                                }
                            }
                        }
                    }
                } else if let Some(ids) = self.graph.store.nodes_by_type.get(&tid) {
                    for id in ids {
                        consider(*id, &mut work)?;
                    }
                }
            }
            Selection::Ids { ids } => {
                if ids.len() > self.config.limits.max_query_work {
                    return Err(limit("selected IDs"));
                }
                for id in ids {
                    consider(id.0, &mut work)?;
                }
            }
            Selection::Reachable { from, walk } => {
                validate_walk(walk)?;
                let (nodes, _) = self.walk(*from, walk, &mut work)?;
                for id in nodes {
                    consider(id, &mut work)?;
                }
            }
        }
        let count = candidates.len();
        let mut ranked = Vec::new();
        for id in candidates {
            let n = &self.graph.store.nodes[&id];
            let score = if let Some(r) = &q.rank {
                let Some(v) = &n.embedding else { continue };
                let ty = self.graph.store.node_types.label(n.node_type).unwrap();
                if self.config.types.get(ty).and_then(Option::as_ref) != Some(&r.vector.space) {
                    return Err(bad("candidate belongs to an incompatible embedding space"));
                }
                Some(score(&r.vector.values, v, &r.vector.space.metric))
            } else {
                None
            };
            ranked.push((id, score));
        }
        if let Some(r) = &q.rank {
            ranked.sort_unstable_by(|(a, x), (b, y)| {
                let order = x.unwrap().total_cmp(&y.unwrap());
                let order = if r.vector.space.metric == Metric::Euclidean {
                    order
                } else {
                    order.reverse()
                };
                order.then(a.cmp(b))
            });
            ranked.truncate(r.k);
        }
        ranked.truncate(q.limit);
        let mut hits = Vec::new();
        let mut output = 128usize;
        for (id, score) in ranked {
            let document = self.project(id, &q.projection)?;
            output += encode(&document)?.len() + 128;
            if output > self.config.limits.max_result_bytes {
                return Err(limit("query output"));
            }
            let mut related = Vec::new();
            if let Some(w) = &q.expand {
                let (_, steps) = self.walk(NodeId(id), w, &mut work)?;
                for (edge, to, depth) in steps {
                    let e = &self.graph.store.edges[&edge];
                    let item = Related {
                        document: self.project(to, &q.related_projection)?,
                        via: Relationship {
                            id: crate::EdgeId(edge),
                            from: NodeId(e.from),
                            to: NodeId(e.to),
                            edge_type: self
                                .graph
                                .store
                                .edge_types
                                .label(e.edge_type)
                                .unwrap()
                                .into(),
                            weight: e.weight,
                            properties: e.properties.clone(),
                        },
                        depth,
                    };
                    output += encode(&item)?.len() + 32;
                    if output > self.config.limits.max_result_bytes {
                        return Err(limit("query output"));
                    }
                    related.push(item);
                }
            }
            hits.push(Hit {
                document,
                score,
                related,
            });
        }
        let result = QueryResult {
            sequence: self.sequence,
            work: work.used,
            candidates: count,
            hits,
        };
        if encode(&result)?.len() > self.config.limits.max_result_bytes {
            return Err(limit("query output"));
        }
        Ok(result)
    }
    fn project(&self, id: u64, p: &Projection) -> Result<Document> {
        let n = self
            .graph
            .store
            .nodes
            .get(&id)
            .ok_or(Error::NodeNotFound(NodeId(id)))?;
        let node_type = self.graph.store.node_types.label(n.node_type).unwrap();
        public(node_type)?;
        let properties = n
            .properties
            .iter()
            .filter(|(k, _)| {
                !k.starts_with("__drey") && p.fields.as_ref().is_none_or(|f| f.contains(k))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let document = if p.document {
            match n.properties.get(BODY) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            }
        } else {
            None
        };
        Ok(Document {
            id: NodeId(id),
            node_type: node_type.into(),
            properties,
            document,
        })
    }
    // Breadth-first, sorted adjacency IDs; visit each node once and retain each
    // traversed edge once, including cycles and multiple supporting edges.
    fn walk(&self, from: NodeId, w: &Walk, work: &mut Work) -> Result<WalkResult> {
        let n = self
            .graph
            .store
            .nodes
            .get(&from.0)
            .ok_or(Error::NodeNotFound(from))?;
        if self.graph.store.node_types.label(n.node_type) == Some(META) {
            return Err(bad("private engine record"));
        }
        let mut nodes = BTreeSet::from([from.0]);
        let mut edges = BTreeSet::new();
        let mut steps = Vec::new();
        let mut queue = VecDeque::from([(from.0, 0)]);
        while let Some((id, depth)) = queue.pop_front() {
            work.tick()?;
            if depth == w.max_hops {
                continue;
            }
            for (out, adj) in [
                (true, &self.graph.store.out_adj),
                (false, &self.graph.store.in_adj),
            ] {
                if (out && w.direction == Direction::Inbound)
                    || (!out && w.direction == Direction::Outbound)
                {
                    continue;
                }
                if let Some(types) = adj.get(&id) {
                    for (tid, ids) in types {
                        work.tick()?;
                        let label = self.graph.store.edge_types.label(*tid).unwrap();
                        if !w.edge_types.is_empty() && !w.edge_types.iter().any(|t| t == label) {
                            continue;
                        }
                        for edge in ids {
                            work.tick()?;
                            let e = &self.graph.store.edges[edge];
                            if w.min_weight.is_some_and(|m| e.weight < m) || !edges.insert(*edge) {
                                continue;
                            }
                            let to = if out { e.to } else { e.from };
                            steps.push((*edge, to, depth + 1));
                            if nodes.insert(to) {
                                queue.push_back((to, depth + 1));
                            }
                        }
                    }
                }
            }
        }
        Ok((nodes, steps))
    }
}
// Accumulate in f64: finite f32 vectors at the declared dimension bound cannot
// overflow, including vectors whose f32 dot products would be infinite.
fn score(a: &[f32], b: &[f32], m: &Metric) -> f64 {
    match m {
        Metric::Dot => a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum(),
        Metric::Euclidean => a
            .iter()
            .zip(b)
            .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
            .sum::<f64>()
            .sqrt(),
        Metric::Cosine => {
            let na = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            let nb = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            if na == 0.0 || nb == 0.0 {
                0.0
            } else {
                score(a, b, &Metric::Dot) / (na * nb)
            }
        }
    }
}
