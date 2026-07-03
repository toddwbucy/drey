//! M1 exit criteria and PRD §17 correctness invariants, exercised in memory.

use drey::config::GraphConfig;
use drey::export::{FeatureSpec, GraphFeatureExport};
use drey::mutation::{EdgeFilter, RemoveNodeMode, WeightUpdate};
use drey::query::{PropertyQuery, ScalarPredicate};
use drey::similarity::{SimilarityMetric, SimilarityQuery};
use drey::traverse::{CostMode, NeighborOptions, ShortestPathOptions, TraversalOptions};
use drey::types::{Embedding, NodeType, Scalar, Value};
use drey::{EdgeType, Graph, NodeId};
use std::collections::BTreeMap;

fn person() -> NodeType {
    NodeType::new("person")
}
fn knows() -> EdgeType {
    EdgeType::new("knows")
}

fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn base_graph() -> Graph {
    let config = GraphConfig::default().with_indexed_property(person(), "age");
    let mut g = Graph::in_memory(config);
    g.register_node_type(person(), Some(4)).unwrap();
    g
}

#[test]
fn add_and_lookup_roundtrip() {
    let mut g = base_graph();
    let a = g
        .add_node(person(), props(&[("age", Value::I64(30))]))
        .unwrap();
    let node = g.node(a).unwrap().unwrap();
    assert_eq!(node.node_type, person());
    assert_eq!(node.properties.get("age"), Some(&Value::I64(30)));
}

#[test]
fn empty_type_query_is_empty_not_error() {
    let g = base_graph(); // registered, zero members
    assert!(g.nodes_by_type(&person()).unwrap().is_empty());
    // Unregistered type errors.
    assert!(g.nodes_by_type(&NodeType::new("ghost")).is_err());
}

#[test]
fn property_index_equality_and_range() {
    let mut g = base_graph();
    let mut ids = Vec::new();
    for age in [20i64, 25, 30, 35, 40] {
        ids.push(
            g.add_node(person(), props(&[("age", Value::I64(age))]))
                .unwrap(),
        );
    }
    // Equality
    let eq = g
        .nodes_by_property(PropertyQuery {
            node_type: person(),
            key: "age".into(),
            predicate: ScalarPredicate::Eq(Scalar::I64(30)),
        })
        .unwrap();
    assert_eq!(eq, vec![ids[2]]);
    // Range [25, 35]
    let range = g
        .nodes_by_property(PropertyQuery {
            node_type: person(),
            key: "age".into(),
            predicate: ScalarPredicate::Range {
                min: Some(Scalar::I64(25)),
                max: Some(Scalar::I64(35)),
            },
        })
        .unwrap();
    assert_eq!(range, vec![ids[1], ids[2], ids[3]]);

    // Inverted bounds (min > max) match nothing on the indexed path — and must
    // not panic (BTreeMap::range panics on inverted bounds).
    let inverted = g
        .nodes_by_property(PropertyQuery {
            node_type: person(),
            key: "age".into(),
            predicate: ScalarPredicate::Range {
                min: Some(Scalar::I64(35)),
                max: Some(Scalar::I64(25)),
            },
        })
        .unwrap();
    assert!(inverted.is_empty());
}

#[test]
fn unindexed_property_falls_back_to_scan() {
    // "name" is not indexed; the query must still be correct.
    let mut g = base_graph();
    let a = g
        .add_node(person(), props(&[("name", Value::String("ada".into()))]))
        .unwrap();
    let _ = g
        .add_node(person(), props(&[("name", Value::String("bob".into()))]))
        .unwrap();
    let hits = g
        .nodes_by_property(PropertyQuery {
            node_type: person(),
            key: "name".into(),
            predicate: ScalarPredicate::Eq(Scalar::String("ada".into())),
        })
        .unwrap();
    assert_eq!(hits, vec![a]);
}

#[test]
fn neighbors_and_traversal_respect_filters() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let c = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, knows(), 0.9, props(&[])).unwrap();
    g.add_edge(b, c, knows(), 0.2, props(&[])).unwrap();

    // Neighbors of a, outbound
    let ns = g.neighbors(a, NeighborOptions::default()).unwrap();
    assert_eq!(ns.len(), 1);
    assert_eq!(ns[0].node, b);

    // 2-hop traversal reaches c
    let paths = g
        .traverse(
            a,
            TraversalOptions {
                max_hops: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(paths.iter().any(|p| p.nodes.last() == Some(&c)));

    // min_weight 0.5 prunes the b->c edge, so c is unreachable
    let pruned = g
        .traverse(
            a,
            TraversalOptions {
                max_hops: Some(2),
                min_weight: Some(0.5),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!pruned.iter().any(|p| p.nodes.last() == Some(&c)));
}

#[test]
fn shortest_path_unweighted_and_weighted() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let c = g.add_node(person(), props(&[])).unwrap();
    // Two routes a->c: direct (cost 5) and via b (cost 1+1=2).
    g.add_edge(a, c, knows(), 5.0, props(&[])).unwrap();
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
    g.add_edge(b, c, knows(), 1.0, props(&[])).unwrap();

    let hops = g
        .shortest_path(a, c, ShortestPathOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(hops.nodes, vec![a, c]); // fewest hops = direct

    let weighted = g
        .shortest_path(
            a,
            c,
            ShortestPathOptions {
                cost_mode: CostMode::WeightedCost,
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert_eq!(weighted.nodes, vec![a, b, c]); // cheapest cost = via b
    assert_eq!(weighted.cost, 2.0);

    // Disconnected pair returns None.
    let d = g.add_node(person(), props(&[])).unwrap();
    assert!(g
        .shortest_path(a, d, ShortestPathOptions::default())
        .unwrap()
        .is_none());
}

#[test]
fn shortest_path_respects_step_budget() {
    // Chain a -> b -> c -> d; reaching d requires expanding a, b, c.
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let c = g.add_node(person(), props(&[])).unwrap();
    let d = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
    g.add_edge(b, c, knows(), 1.0, props(&[])).unwrap();
    g.add_edge(c, d, knows(), 1.0, props(&[])).unwrap();

    // Reaching d requires expanding a, b, c (3 expansions); then d is popped and
    // returned. So max_steps=3 is the exact boundary: it succeeds, and one less
    // (2) fails. The tight pair catches a `>` → `>=` off-by-one regression.
    let too_small = g
        .shortest_path(
            a,
            d,
            ShortestPathOptions {
                max_steps: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        too_small.is_none(),
        "2 expansions cannot reach a 3-expansion target"
    );

    let exact = g
        .shortest_path(
            a,
            d,
            ShortestPathOptions {
                max_steps: Some(3),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    // The bounded result at the exact boundary matches the default (unbounded) search.
    let unbounded = g
        .shortest_path(a, d, ShortestPathOptions::default())
        .unwrap()
        .unwrap();
    assert_eq!(exact.nodes, vec![a, b, c, d]);
    assert_eq!(exact.nodes, unbounded.nodes);

    // The bound applies to weighted mode too.
    let weighted_bounded = g
        .shortest_path(
            a,
            d,
            ShortestPathOptions {
                max_steps: Some(1),
                cost_mode: CostMode::WeightedCost,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(weighted_bounded.is_none());
}

#[test]
fn similarity_composes_with_filters_and_enforces_dimension() {
    let mut g = base_graph();
    let a = g
        .add_node(person(), props(&[("age", Value::I64(30))]))
        .unwrap();
    let b = g
        .add_node(person(), props(&[("age", Value::I64(30))]))
        .unwrap();
    let c = g
        .add_node(person(), props(&[("age", Value::I64(99))]))
        .unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    g.set_node_embedding(b, Embedding::new(vec![0.9, 0.1, 0.0, 0.0]))
        .unwrap();
    g.set_node_embedding(c, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();

    // Query near a's vector, restricted to age=30 → c (age 99) excluded even
    // though its vector is identical to the query.
    let q = SimilarityQuery {
        property_filter: Some(PropertyQuery {
            node_type: person(),
            key: "age".into(),
            predicate: ScalarPredicate::Eq(Scalar::I64(30)),
        }),
        ..SimilarityQuery::new(
            Embedding::new(vec![1.0, 0.0, 0.0, 0.0]),
            SimilarityMetric::Cosine,
            10,
        )
    };
    let hits = g.similar_nodes(q).unwrap();
    let ids: Vec<NodeId> = hits.iter().map(|(n, _)| *n).collect();
    assert!(ids.contains(&a) && ids.contains(&b));
    assert!(!ids.contains(&c));

    // Wrong-dimension embedding is a dimension error.
    assert!(g
        .set_node_embedding(a, Embedding::new(vec![1.0, 2.0]))
        .is_err());
}

#[test]
fn similarity_within_reachability_is_bounded_by_hops() {
    use drey::similarity::ReachabilityFilter;
    use drey::traverse::DirectionOpt;
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let c = g.add_node(person(), props(&[])).unwrap();
    let d = g.add_node(person(), props(&[])).unwrap(); // unreachable from a
    for n in [a, b, c, d] {
        g.set_node_embedding(n, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
            .unwrap();
    }
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap(); // a -> b (1 hop)
    g.add_edge(b, c, knows(), 1.0, props(&[])).unwrap(); // b -> c (2 hops)

    // Within 1 hop of a (outbound): only a (0 hops) and b (1 hop) qualify.
    let q = SimilarityQuery {
        within: Some(ReachabilityFilter {
            from: a,
            max_hops: 1,
            edge_types: vec![],
            min_weight: None,
            direction: DirectionOpt::Outbound,
        }),
        ..SimilarityQuery::new(
            Embedding::new(vec![1.0, 0.0, 0.0, 0.0]),
            SimilarityMetric::Cosine,
            10,
        )
    };
    let ids: Vec<NodeId> = g
        .similar_nodes(q)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert!(ids.contains(&a) && ids.contains(&b));
    assert!(!ids.contains(&c), "c is 2 hops away, must be excluded");
    assert!(!ids.contains(&d), "d is unreachable, must be excluded");
}

#[test]
fn remove_node_mode_default_rejects_incident_edges() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();

    // Default mode refuses to orphan edges.
    assert!(g
        .remove_node(a, RemoveNodeMode::RejectIfEdgesExist)
        .is_err());
    // Explicit cascade succeeds and leaves no dangling edge.
    g.remove_node(a, RemoveNodeMode::RemoveIncidentEdges)
        .unwrap();
    assert!(g.node(a).unwrap().is_none());
    assert_eq!(
        g.neighbors(
            b,
            NeighborOptions {
                direction: drey::traverse::DirectionOpt::Inbound,
                ..Default::default()
            }
        )
        .unwrap()
        .len(),
        0
    );
}

#[test]
fn weight_update_with_bounds_and_decay() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let e = g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();

    let w = g
        .update_edge_weight(e, WeightUpdate::add(10.0).with_bounds(0.0, 2.0))
        .unwrap();
    assert_eq!(w, 2.0); // clamped

    let report = g.decay_edges(EdgeFilter::new(), 0.5).unwrap();
    assert_eq!(report.edges_decayed, 1);
    assert_eq!(g.edge(e).unwrap().unwrap().weight, 1.0);
}

#[test]
fn durable_ids_are_not_reused_after_removal() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    g.remove_node(a, RemoveNodeMode::RejectIfEdgesExist)
        .unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    assert_ne!(a, b); // monotonic allocator, no reuse (PRD §7.4)
}

#[test]
fn feature_export_is_deterministic() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();
    g.set_node_embedding(b, Embedding::new(vec![5.0, 6.0, 7.0, 8.0]))
        .unwrap();
    g.add_edge(a, b, knows(), 0.5, props(&[])).unwrap();

    let map = g.node_index_map();
    assert_eq!(map.len(), 2);
    let spec = FeatureSpec {
        include_embedding: true,
        numeric_properties: vec![],
    };
    let feats = g.node_features(&map, &spec).unwrap();
    // Row order follows the deterministic index map (sorted by NodeId).
    assert_eq!(feats[map.index_of(a).unwrap()], vec![1.0, 2.0, 3.0, 4.0]);
    let ei = g.edge_index(&map, &EdgeFilter::new()).unwrap();
    assert_eq!(
        ei,
        vec![(map.index_of(a).unwrap(), map.index_of(b).unwrap())]
    );
}

#[test]
fn read_only_rejects_mutation() {
    let mut g = Graph::in_memory(GraphConfig::default().read_only());
    assert!(g.register_node_type(person(), None).is_err());
}

#[test]
fn weight_update_rejects_malformed_bounds_instead_of_panicking() {
    use drey::mutation::WeightUpdate;
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let e = g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
    // NaN bound and inverted bound both return an error, not a clamp panic.
    assert!(g.update_edge_weight(e, WeightUpdate::set(0.5).with_bounds(f32::NAN, 1.0)).is_err());
    assert!(g.update_edge_weight(e, WeightUpdate::add(0.1).with_bounds(2.0, 1.0)).is_err());
    // A well-formed bound still works.
    assert_eq!(g.update_edge_weight(e, WeightUpdate::set(5.0).with_bounds(0.0, 2.0)).unwrap(), 2.0);
}

#[test]
fn similarity_rejects_non_finite_embeddings_and_query() {
    use drey::similarity::{SimilarityMetric, SimilarityQuery};
    let mut g = base_graph();
    let x = g.add_node(person(), props(&[])).unwrap();
    // A NaN embedding component is rejected at write time.
    assert!(g.set_node_embedding(x, Embedding::new(vec![f32::NAN, 0.0, 0.0, 0.0])).is_err());
    g.set_node_embedding(x, Embedding::new(vec![1.0, 0.0, 0.0, 0.0])).unwrap();
    // A non-finite query vector is rejected.
    let q = SimilarityQuery::new(Embedding::new(vec![f32::INFINITY, 0.0, 0.0, 0.0]), SimilarityMetric::Cosine, 5);
    assert!(g.similar_nodes(q).is_err());
}

#[test]
fn export_node_type_ids_align_and_features_are_rectangular() {
    use drey::export::{FeatureSpec, GraphFeatureExport};
    let t2 = NodeType::new("tag");
    let mut g = base_graph();
    g.register_node_type(t2.clone(), None).unwrap(); // no embedding
    let a = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 2.0, 3.0, 4.0])).unwrap();
    let b = g.add_node(t2.clone(), props(&[])).unwrap(); // no embedding
    let map = g.node_index_map();
    // node_type_ids aligned to the map, distinct per type.
    let tids = g.node_type_ids(&map).unwrap();
    assert_eq!(tids.len(), 2);
    assert_ne!(tids[map.index_of(a).unwrap()], tids[map.index_of(b).unwrap()]);
    // Feature rows are rectangular even though b has no embedding (zero-padded).
    let feats = g
        .node_features(&map, &FeatureSpec { include_embedding: true, numeric_properties: vec![] })
        .unwrap();
    assert_eq!(feats[0].len(), feats[1].len());
    assert_eq!(feats[map.index_of(b).unwrap()], vec![0.0, 0.0, 0.0, 0.0]);
}
