//! M1 exit criteria and PRD §17 correctness invariants, exercised in memory.

use drey::config::GraphConfig;
use drey::export::{FeatureSpec, GraphFeatureExport};
use drey::mutation::{EdgeFilter, PropertyPatch, RemoveNodeMode, WeightUpdate};
use drey::query::{PropertyQuery, ScalarPredicate};
use drey::similarity::{SimilarityMetric, SimilarityQuery};
use drey::traverse::{CostMode, NeighborOptions, ShortestPathOptions, TraversalOptions};
use drey::types::{Embedding, NodeType, Scalar, Value};
use drey::{Direction, EdgeType, Error, Graph, NodeId};
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
    assert!(g
        .update_edge_weight(e, WeightUpdate::set(0.5).with_bounds(f32::NAN, 1.0))
        .is_err());
    assert!(g
        .update_edge_weight(e, WeightUpdate::add(0.1).with_bounds(2.0, 1.0))
        .is_err());
    // A well-formed bound still works.
    assert_eq!(
        g.update_edge_weight(e, WeightUpdate::set(5.0).with_bounds(0.0, 2.0))
            .unwrap(),
        2.0
    );
}

#[test]
fn similarity_rejects_non_finite_embeddings_and_query() {
    use drey::similarity::{SimilarityMetric, SimilarityQuery};
    let mut g = base_graph();
    let x = g.add_node(person(), props(&[])).unwrap();
    // A NaN embedding component is rejected at write time.
    assert!(g
        .set_node_embedding(x, Embedding::new(vec![f32::NAN, 0.0, 0.0, 0.0]))
        .is_err());
    g.set_node_embedding(x, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    // A non-finite query vector is rejected.
    let q = SimilarityQuery::new(
        Embedding::new(vec![f32::INFINITY, 0.0, 0.0, 0.0]),
        SimilarityMetric::Cosine,
        5,
    );
    assert!(g.similar_nodes(q).is_err());
}

#[test]
fn export_node_type_ids_align_and_features_are_rectangular() {
    use drey::export::{FeatureSpec, GraphFeatureExport};
    let t2 = NodeType::new("tag");
    let mut g = base_graph();
    g.register_node_type(t2.clone(), None).unwrap(); // no embedding
    let a = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();
    let b = g.add_node(t2.clone(), props(&[])).unwrap(); // no embedding
    let map = g.node_index_map();
    // node_type_ids aligned to the map, distinct per type.
    let tids = g.node_type_ids(&map).unwrap();
    assert_eq!(tids.len(), 2);
    assert_ne!(
        tids[map.index_of(a).unwrap()],
        tids[map.index_of(b).unwrap()]
    );
    // Feature rows are rectangular even though b has no embedding (zero-padded).
    let feats = g
        .node_features(
            &map,
            &FeatureSpec {
                include_embedding: true,
                numeric_properties: vec![],
            },
        )
        .unwrap();
    assert_eq!(feats[0].len(), feats[1].len());
    assert_eq!(feats[map.index_of(b).unwrap()], vec![0.0, 0.0, 0.0, 0.0]);
}

#[test]
fn property_index_has_no_stale_hits_after_remove_or_update() {
    // The scalar index must drop a node's entry on remove and on value change,
    // so a query never returns a removed node or an old value (regression lock
    // for the prop-index tombstone-leak fix).
    let mut g = base_graph();
    let a = g
        .add_node(person(), props(&[("age", Value::I64(50))]))
        .unwrap();
    let b = g
        .add_node(person(), props(&[("age", Value::I64(50))]))
        .unwrap();
    let eq = |v: i64| PropertyQuery {
        node_type: person(),
        key: "age".into(),
        predicate: ScalarPredicate::Eq(Scalar::I64(v)),
    };
    g.remove_node(a, RemoveNodeMode::RejectIfEdgesExist)
        .unwrap();
    assert_eq!(
        g.nodes_by_property(eq(50)).unwrap(),
        vec![b],
        "removed node still indexed"
    );
    // Update b's value: the old value must no longer match, the new one must.
    g.update_node_properties(b, PropertyPatch::new().set("age", Value::I64(60)))
        .unwrap();
    assert!(
        g.nodes_by_property(eq(50)).unwrap().is_empty(),
        "stale index entry for the pre-update value"
    );
    assert_eq!(g.nodes_by_property(eq(60)).unwrap(), vec![b]);
}

#[test]
fn shortest_path_from_equals_to_is_zero_length() {
    // A node reaches itself with a trivial zero-cost path, and that self-target
    // is found even under a zero step budget (the target check precedes the
    // budget check).
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let p = g
        .shortest_path(a, a, ShortestPathOptions::default())
        .unwrap()
        .expect("a node reaches itself");
    assert_eq!(p.nodes, vec![a]);
    assert!(p.edges.is_empty());
    assert_eq!(p.cost, 0.0);
    assert!(g
        .shortest_path(
            a,
            a,
            ShortestPathOptions {
                max_steps: Some(0),
                ..Default::default()
            },
        )
        .unwrap()
        .is_some());
}

#[test]
fn shortest_path_max_steps_zero_reaches_no_neighbor() {
    // A zero step budget permits no expansion, so an adjacent target is
    // unreachable — None, not a panic or an off-by-one hit.
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
    let opts = ShortestPathOptions {
        max_steps: Some(0),
        ..Default::default()
    };
    assert!(g.shortest_path(a, b, opts).unwrap().is_none());
}

#[test]
fn signed_zero_f64_matches_ieee_equal_index_and_scan() {
    // -0.0 and +0.0 are IEEE-equal, so an Eq(F64(0.0)) query must match a stored
    // F64(-0.0) — on both the indexed path (ScalarKey Ord) and the scan path
    // (ScalarPredicate::matches), which share Scalar::total_order.
    let mut g = Graph::in_memory(GraphConfig::default().with_indexed_property(person(), "score"));
    g.register_node_type(person(), None).unwrap();
    let n = g
        .add_node(
            person(),
            props(&[
                ("score", Value::F64(-0.0)), // indexed
                ("raw", Value::F64(-0.0)),   // unindexed -> scan path
            ]),
        )
        .unwrap();
    let eq0 = |key: &str| PropertyQuery {
        node_type: person(),
        key: key.into(),
        predicate: ScalarPredicate::Eq(Scalar::F64(0.0)),
    };
    assert_eq!(g.nodes_by_property(eq0("score")).unwrap(), vec![n], "index");
    assert_eq!(g.nodes_by_property(eq0("raw")).unwrap(), vec![n], "scan");
}

#[test]
fn similar_nodes_missing_reachability_anchor_errors() {
    use drey::similarity::{ReachabilityFilter, SimilarityMetric, SimilarityQuery};
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    // A within-anchor that does not exist must error like every other traversal
    // anchor (neighbors/traverse/shortest_path), not silently return Ok(empty).
    let q = SimilarityQuery {
        within: Some(ReachabilityFilter {
            from: NodeId(9999),
            max_hops: 3,
            edge_types: vec![],
            min_weight: None,
            direction: Direction::Outbound,
        }),
        ..SimilarityQuery::new(
            Embedding::new(vec![1.0, 0.0, 0.0, 0.0]),
            SimilarityMetric::Cosine,
            5,
        )
    };
    match g.similar_nodes(q) {
        Err(Error::NodeNotFound(_)) => {}
        other => panic!("expected NodeNotFound for a missing anchor, got {other:?}"),
    }
}

#[test]
fn direction_enum_is_usable_in_public_options() {
    // Regression for the DirectionOpt/Direction split: the PRD-sanctioned
    // Direction enum (§9.1/§9.4) constructs the public options structs directly,
    // with no separate type to convert through. Compile-level guarantee.
    let _n = NeighborOptions {
        direction: Direction::Both,
        ..Default::default()
    };
    let _t = TraversalOptions {
        direction: Direction::Inbound,
        ..Default::default()
    };
    let _s = ShortestPathOptions {
        direction: Direction::Outbound,
        ..Default::default()
    };
}

#[test]
fn decay_rejecting_non_finite_result_leaves_batch_unapplied() {
    // A finite weight × finite factor can overflow to +inf; the whole decay
    // batch must be rejected before any edge is mutated (no partial apply).
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let e = g.add_edge(a, b, knows(), f32::MAX, props(&[])).unwrap();
    assert!(
        g.decay_edges(EdgeFilter::new(), 10.0).is_err(),
        "overflow to +inf must be rejected"
    );
    // Weight is untouched: the batch aborted before applying anything.
    assert_eq!(g.edge(e).unwrap().unwrap().weight, f32::MAX);
}

#[test]
fn traverse_max_hops_zero_returns_no_paths() {
    // A path needs at least one edge, so a 0-hop traversal returns nothing —
    // not a spurious length-0 path to the start node.
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();
    let paths = g
        .traverse(
            a,
            TraversalOptions {
                max_hops: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(paths.is_empty());
}

// ---- Regression coverage added by the repo ultrareview ----

#[test]
fn nan_weight_update_with_bounds_is_rejected_not_coerced() {
    // `NaN.max(min).min(max)` is `min` (f32 min/max drop NaN), so a bounded
    // NaN update must be rejected explicitly rather than silently coerced to a
    // finite in-bounds weight that passes the `is_finite` guard.
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let e = g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();

    let err = g.update_edge_weight(e, WeightUpdate::set(f32::NAN).with_bounds(0.0, 1.0));
    assert!(
        matches!(err, Err(Error::InvalidPropertyValue(_))),
        "bounded NaN update must be rejected, got {err:?}"
    );
    // No phantom write: the weight is unchanged.
    assert_eq!(g.edge(e).unwrap().unwrap().weight, 1.0);
    // A finite bounded update still clamps as before.
    let w = g
        .update_edge_weight(e, WeightUpdate::add(10.0).with_bounds(0.0, 2.0))
        .unwrap();
    assert_eq!(w, 2.0);
}

#[test]
fn self_loop_not_double_counted_and_allow_revisit_terminates() {
    use drey::traverse::CyclePolicy;
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let self_e = g.add_edge(a, a, knows(), 1.0, props(&[])).unwrap();

    // A self-loop lives in both out_adj and in_adj of its node; under `Both` it
    // must be emitted exactly once, not twice.
    let both = g
        .neighbors(
            a,
            NeighborOptions {
                direction: Direction::Both,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        both.len(),
        1,
        "self-loop double-counted under Direction::Both"
    );
    assert_eq!(both[0].via, self_e);
    assert_eq!(both[0].node, a);

    // AllowRevisit walks the self-loop but stays bounded by max_hops (one
    // revisiting path per hop-depth, longest using max_hops edges).
    let paths = g
        .traverse(
            a,
            TraversalOptions {
                max_hops: Some(3),
                cycle_policy: CyclePolicy::AllowRevisit,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        paths.len(),
        3,
        "expected one revisiting path per hop up to max_hops"
    );
    assert!(paths.iter().all(|p| p.nodes.iter().all(|n| *n == a)));
    assert_eq!(paths.iter().map(|p| p.edges.len()).max(), Some(3));
}

#[test]
fn overflowed_similarity_score_does_not_rank_first() {
    // Finite inputs can still produce a non-finite score (f32 dot overflow to
    // +inf). That must be treated as the worst rank, not sorted to the top by
    // `total_cmp` (which orders NaN/inf as the largest value).
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    g.set_node_embedding(b, Embedding::new(vec![f32::MAX, 0.0, 0.0, 0.0]))
        .unwrap();

    // dot(query, a) == f32::MAX (finite); dot(query, b) == f32::MAX² -> +inf.
    let q = SimilarityQuery::new(
        Embedding::new(vec![f32::MAX, 0.0, 0.0, 0.0]),
        SimilarityMetric::Dot,
        10,
    );
    let hits = g.similar_nodes(q).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].0, a,
        "finite-scored node must outrank the overflowed one"
    );
    assert_eq!(hits[1].0, b);
}

#[test]
fn similarity_scan_ceiling_bounds_unfiltered_scan() {
    // The scan ceiling is the concrete enforcement of "no accidental full vector
    // sweep" (PRD §13.1): an unfiltered candidate set over the ceiling is
    // rejected unless the caller opts into a full scan.
    let mut config = GraphConfig::default();
    config.scan_ceiling.max_candidates = 2;
    let mut g = Graph::in_memory(config);
    g.register_node_type(person(), Some(2)).unwrap();
    for _ in 0..5 {
        let n = g.add_node(person(), props(&[])).unwrap();
        g.set_node_embedding(n, Embedding::new(vec![1.0, 0.0]))
            .unwrap();
    }
    let q = SimilarityQuery::new(Embedding::new(vec![1.0, 0.0]), SimilarityMetric::Cosine, 10);
    assert!(
        matches!(g.similar_nodes(q.clone()), Err(Error::UnsupportedQuery(_))),
        "5 candidates over a ceiling of 2 must be rejected"
    );
    let q2 = SimilarityQuery {
        allow_full_scan: true,
        ..q
    };
    assert_eq!(
        g.similar_nodes(q2).unwrap().len(),
        5,
        "allow_full_scan lifts the bound"
    );
}

#[test]
fn similarity_query_dimension_mismatch_returns_empty() {
    // A query whose dimension matches no stored embedding returns Ok(empty), not
    // an error — mixed-dimension graphs are legal, so a dim that misses one type
    // is not globally invalid (documented contract on `similar_nodes`).
    let mut g = base_graph(); // embeddings are dim 4
    let a = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(a, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    let q = SimilarityQuery::new(Embedding::new(vec![1.0, 0.0]), SimilarityMetric::Cosine, 10);
    assert_eq!(g.similar_nodes(q).unwrap(), vec![]);
}

#[test]
fn traversal_respects_edge_type_filter_including_unknown_type() {
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    let c = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, EdgeType::new("knows"), 1.0, props(&[]))
        .unwrap();
    g.add_edge(a, c, EdgeType::new("likes"), 1.0, props(&[]))
        .unwrap();

    let knows_only = g
        .neighbors(
            a,
            NeighborOptions {
                edge_types: vec![EdgeType::new("knows")],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(knows_only.len(), 1);
    assert_eq!(knows_only[0].node, b);

    // An unknown edge type must exclude all edges (empty filter set), not fall
    // back to matching everything.
    let unknown = g
        .neighbors(
            a,
            NeighborOptions {
                edge_types: vec![EdgeType::new("nonexistent")],
                ..Default::default()
            },
        )
        .unwrap();
    assert!(unknown.is_empty(), "unknown edge type must match no edges");
}

#[test]
fn similarity_node_type_filter_excludes_other_types() {
    let mut g = Graph::in_memory(GraphConfig::default());
    g.register_node_type(NodeType::new("person"), Some(2))
        .unwrap();
    g.register_node_type(NodeType::new("robot"), Some(2))
        .unwrap();
    let p = g.add_node(NodeType::new("person"), props(&[])).unwrap();
    let r = g.add_node(NodeType::new("robot"), props(&[])).unwrap();
    g.set_node_embedding(p, Embedding::new(vec![1.0, 0.0]))
        .unwrap();
    g.set_node_embedding(r, Embedding::new(vec![1.0, 0.0]))
        .unwrap();

    let q = SimilarityQuery {
        node_types: Some(vec![NodeType::new("person")]),
        ..SimilarityQuery::new(Embedding::new(vec![1.0, 0.0]), SimilarityMetric::Cosine, 10)
    };
    let ids: Vec<NodeId> = g
        .similar_nodes(q)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(
        ids,
        vec![p],
        "robot must be excluded by the node_types filter"
    );
}

// ---- Regression coverage: issue #22 + 2026-07 repo review ----

#[test]
fn mixed_variant_numeric_predicates_compare_by_value_on_both_paths() {
    // Review finding: cross-variant Range/Eq used to be decided by variant
    // rank, so Range{min: I64(0), max: F64(10.0)} admitted F64(-5.0). Both the
    // index path (indexed property) and the scan path (unindexed) must agree
    // on value-based answers.
    let config = GraphConfig::default().with_indexed_property(person(), "score");
    let mut g = Graph::in_memory(config);
    g.register_node_type(person(), None).unwrap();
    let neg = g
        .add_node(person(), props(&[("score", Value::F64(-5.0))]))
        .unwrap();
    let five_int = g
        .add_node(person(), props(&[("score", Value::I64(5))]))
        .unwrap();
    let five_float = g
        .add_node(person(), props(&[("score", Value::F64(5.0))]))
        .unwrap();
    let big = g
        .add_node(person(), props(&[("score", Value::F64(50.0))]))
        .unwrap();

    // "score" is indexed; "shadow" (same values, different key) is not — the
    // scan path must give the same answers.
    for n in [neg, five_int, five_float, big] {
        let v = g.node(n).unwrap().unwrap().properties["score"].clone();
        g.update_node_properties(n, PropertyPatch::new().set("shadow", v))
            .unwrap();
    }
    for key in ["score", "shadow"] {
        let range = g
            .nodes_by_property(PropertyQuery {
                node_type: person(),
                key: key.into(),
                predicate: ScalarPredicate::Range {
                    min: Some(Scalar::I64(0)),
                    max: Some(Scalar::F64(10.0)),
                },
            })
            .unwrap();
        assert_eq!(
            range,
            vec![five_int, five_float],
            "({key}) mixed-variant range must admit exactly the in-range values"
        );
        let eq = g
            .nodes_by_property(PropertyQuery {
                node_type: person(),
                key: key.into(),
                predicate: ScalarPredicate::Eq(Scalar::F64(5.0)),
            })
            .unwrap();
        assert_eq!(
            eq,
            vec![five_int, five_float],
            "({key}) Eq(F64(5.0)) must match a stored I64(5)"
        );
    }
}

#[test]
fn nan_property_equality_is_bit_pattern_independent() {
    // A runtime 0.0/0.0 produces a negative-quiet NaN on x86; querying with
    // the positive f64::NAN constant must still match it — all NaN payloads
    // are one value under the index's total order.
    let mut g = base_graph();
    // Pinned bit pattern, not a runtime 0.0/0.0: the division constant-folds
    // to the canonical positive NaN at opt-level > 0, which would silently
    // drop the negative-NaN coverage from `cargo test --release`.
    let negative_quiet_nan = f64::from_bits(0xFFF8_0000_0000_0000);
    let stored = g
        .add_node(person(), props(&[("x", Value::F64(negative_quiet_nan))]))
        .unwrap();
    let hits = g
        .nodes_by_property(PropertyQuery {
            node_type: person(),
            key: "x".into(),
            predicate: ScalarPredicate::Eq(Scalar::F64(f64::NAN)),
        })
        .unwrap();
    assert_eq!(hits, vec![stored]);
}

#[test]
fn nan_min_weight_is_rejected_on_every_surface() {
    // NaN makes `weight < min` and `weight >= min` both false, so the two
    // filter surfaces would silently disagree (traversal passes everything,
    // scans pass nothing). Every entry accepting min_weight must reject it.
    let mut g = base_graph();
    let a = g.add_node(person(), props(&[])).unwrap();
    let b = g.add_node(person(), props(&[])).unwrap();
    g.add_edge(a, b, knows(), 1.0, props(&[])).unwrap();

    assert!(g
        .neighbors(
            a,
            NeighborOptions {
                min_weight: Some(f32::NAN),
                ..NeighborOptions::default()
            }
        )
        .is_err());
    assert!(g
        .traverse(
            a,
            TraversalOptions {
                min_weight: Some(f32::NAN),
                ..TraversalOptions::default()
            }
        )
        .is_err());
    assert!(g
        .shortest_path(
            a,
            b,
            ShortestPathOptions {
                min_weight: Some(f32::NAN),
                ..ShortestPathOptions::default()
            }
        )
        .is_err());
    assert!(g
        .decay_edges(EdgeFilter::new().with_min_weight(f32::NAN), 0.5)
        .is_err());
    assert!(g
        .edge_weights(&EdgeFilter::new().with_min_weight(f32::NAN))
        .is_err());
    let q = SimilarityQuery {
        within: Some(drey::similarity::ReachabilityFilter {
            from: a,
            max_hops: 2,
            edge_types: vec![],
            min_weight: Some(f32::NAN),
            direction: Direction::Outbound,
        }),
        ..SimilarityQuery::new(
            Embedding::new(vec![1.0, 0.0, 0.0, 0.0]),
            SimilarityMetric::Cosine,
            3,
        )
    };
    assert!(g.similar_nodes(q).is_err());
}

#[test]
fn similarity_ceiling_bounds_probed_candidates_not_scored_vectors() {
    // Issue #22 item 6: the ceiling bounds the candidates the scan must
    // EXAMINE. An unfiltered query over a graph larger than the ceiling is
    // rejected even when only a handful of nodes carry embeddings (the old
    // check counted scored vectors, so this query walked the whole node set).
    let mut config = GraphConfig::default();
    config.scan_ceiling.max_candidates = 3;
    let mut g = Graph::in_memory(config);
    g.register_node_type(person(), Some(2)).unwrap();
    let mut with_embedding = None;
    for i in 0..10 {
        let n = g.add_node(person(), props(&[])).unwrap();
        if i == 0 {
            g.set_node_embedding(n, Embedding::new(vec![1.0, 0.0]))
                .unwrap();
            with_embedding = Some(n);
        }
    }
    let q = SimilarityQuery::new(Embedding::new(vec![1.0, 0.0]), SimilarityMetric::Cosine, 5);
    assert!(
        matches!(g.similar_nodes(q.clone()), Err(Error::UnsupportedQuery(_))),
        "10 probed candidates over a ceiling of 3 must be rejected despite 1 scorable vector"
    );
    // The explicit override still works and returns the single scorable node.
    let full = g
        .similar_nodes(SimilarityQuery {
            allow_full_scan: true,
            ..q.clone()
        })
        .unwrap();
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].0, with_embedding.unwrap());
    // A type-filtered query with an over-ceiling candidate set is equally
    // bounded — the filter result is what gets probed.
    let filtered = SimilarityQuery {
        node_types: Some(vec![person()]),
        ..q
    };
    assert!(matches!(
        g.similar_nodes(filtered),
        Err(Error::UnsupportedQuery(_))
    ));
}

#[test]
fn empty_node_type_allow_list_matches_nothing() {
    // Some(vec![]) is an empty allow-list, deliberately distinct from None
    // (no constraint): documented policy, pinned here.
    let mut g = base_graph();
    let n = g.add_node(person(), props(&[])).unwrap();
    g.set_node_embedding(n, Embedding::new(vec![1.0, 0.0, 0.0, 0.0]))
        .unwrap();
    let q = SimilarityQuery {
        node_types: Some(vec![]),
        ..SimilarityQuery::new(
            Embedding::new(vec![1.0, 0.0, 0.0, 0.0]),
            SimilarityMetric::Cosine,
            3,
        )
    };
    assert!(g.similar_nodes(q).unwrap().is_empty());
}

#[test]
fn node_index_map_accessor_is_sorted_and_consistent() {
    // NodeIndexMap's inverse lookup uses binary_search; the accessor exposes
    // the mapping read-only, so consumers can no longer un-sort it (issue #22
    // item 1 — the field used to be public and mutable).
    let mut g = base_graph();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(g.add_node(person(), props(&[])).unwrap());
    }
    g.remove_node(ids[2], RemoveNodeMode::RejectIfEdgesExist)
        .unwrap();
    let map = g.node_index_map();
    let nodes = map.nodes();
    assert!(
        nodes.windows(2).all(|w| w[0] < w[1]),
        "mapping must be sorted"
    );
    for (dense, id) in nodes.iter().enumerate() {
        assert_eq!(map.index_of(*id), Some(dense));
    }
    assert_eq!(map.index_of(ids[2]), None);
}
