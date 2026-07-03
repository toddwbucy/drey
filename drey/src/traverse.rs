//! Neighbor listing, bounded traversal, and shortest path (PRD §9.3).
//!
//! All three run entirely against the in-memory adjacency indexes (PRD §10.1)
//! and are internally deterministic. The ordering contract is fixed but not a
//! global edge-id sort: neighbors are yielded in `(edge_type, edge_id)` order
//! (the adjacency is a `BTreeMap` over edge types, each type's edge list kept
//! id-sorted), and for `Direction::Both` all outbound steps precede all inbound.
//! That order is stable across runs, so with a `max_paths` cap the returned
//! paths are reproducible — the property the harness's counter repeatability
//! depends on (spec §4.1 / §5.4). No public API guarantees a particular order;
//! only that it is deterministic.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::types::{Direction, EdgeId, EdgeType, NodeId};

/// Hard upper bound on traversal depth, so a large caller `max_hops` cannot
/// drive unbounded DFS recursion (stack safety). Also clamps reachability
/// search depth.
pub(crate) const MAX_TRAVERSAL_HOPS: usize = 64;

/// One step out of a node (PRD §9.3).
#[derive(Clone, Debug, PartialEq)]
pub struct Neighbor {
    pub node: NodeId,
    pub via: EdgeId,
    pub edge_type: EdgeType,
    pub weight: f32,
}

/// Options for neighbor listing (PRD §9.3, §5.1 in-scope).
#[derive(Clone, Default, Debug)]
pub struct NeighborOptions {
    pub direction: DirectionOpt,
    /// Restrict to these edge types (empty = all).
    pub edge_types: Vec<EdgeType>,
    pub min_weight: Option<f32>,
}

/// Direction wrapper with a sensible default (`Outbound`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DirectionOpt {
    #[default]
    Outbound,
    Inbound,
    Both,
}

impl From<DirectionOpt> for Direction {
    fn from(d: DirectionOpt) -> Self {
        match d {
            DirectionOpt::Outbound => Direction::Outbound,
            DirectionOpt::Inbound => Direction::Inbound,
            DirectionOpt::Both => Direction::Both,
        }
    }
}

/// What happens when traversal revisits a node (PRD §9.3 `cycle_policy`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CyclePolicy {
    /// Do not extend a path through a node already on that path (default).
    #[default]
    NoRevisit,
    /// Allow revisiting; the `max_hops` bound still terminates traversal.
    AllowRevisit,
}

/// Cost model for shortest path (PRD §9.3 `cost_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CostMode {
    /// Fewest hops. Edge weights ignored.
    #[default]
    UnweightedHops,
    /// Minimize summed edge weight (weights treated as non-negative costs).
    WeightedCost,
}

/// Options for bounded traversal (PRD §9.3).
#[derive(Clone, Debug)]
pub struct TraversalOptions {
    /// Maximum hops. `None` uses the graph's configured `default_max_hops`
    /// (PRD §9.1). Any value is still clamped to [`MAX_TRAVERSAL_HOPS`].
    pub max_hops: Option<usize>,
    pub direction: DirectionOpt,
    pub edge_types: Vec<EdgeType>,
    pub min_weight: Option<f32>,
    pub max_paths: usize,
    pub cycle_policy: CyclePolicy,
}

impl Default for TraversalOptions {
    fn default() -> Self {
        TraversalOptions {
            max_hops: None,
            direction: DirectionOpt::Outbound,
            edge_types: Vec::new(),
            min_weight: None,
            max_paths: 1000,
            cycle_policy: CyclePolicy::NoRevisit,
        }
    }
}

/// Options for shortest path (PRD §9.3).
#[derive(Clone, Debug, Default)]
pub struct ShortestPathOptions {
    pub direction: DirectionOpt,
    pub edge_types: Vec<EdgeType>,
    pub min_weight: Option<f32>,
    pub cost_mode: CostMode,
    /// Optional exploration budget: the maximum number of nodes the search may
    /// expand before giving up and returning `None`. Bounds worst-case latency
    /// on large or disconnected graphs, where an unbounded search can walk an
    /// entire component (M3 finding F1). `None` (the default) is unbounded.
    pub max_steps: Option<usize>,
}

/// A path: alternating nodes and the edges taken between them
/// (`edges.len() == nodes.len() - 1`). `cost` is hop count in unweighted mode,
/// summed weight in weighted mode.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub nodes: Vec<NodeId>,
    pub edges: Vec<EdgeId>,
    pub cost: f32,
}

impl Graph {
    /// Directed steps out of `node`, in deterministic `(edge_type, edge_id)`
    /// order (outbound before inbound for `Both`), after applying
    /// the edge-type and weight filters. `(edge_id, other_endpoint)`.
    pub(crate) fn steps(
        &self,
        node: u64,
        direction: Direction,
        type_ids: &Option<HashSet<u32>>,
        min_weight: Option<f32>,
    ) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::new();
        // O(degree): look up the node's own adjacency, then only the requested
        // edge types — never a scan of the whole index.
        let mut push_from = |adj: &HashMap<u64, std::collections::BTreeMap<u32, Vec<u64>>>,
                             take_to: bool| {
            let Some(by_type) = adj.get(&node) else {
                return;
            };
            for (etype, edges) in by_type {
                if let Some(types) = type_ids {
                    if !types.contains(etype) {
                        continue;
                    }
                }
                for e in edges {
                    let rec = &self.store.edges[e];
                    if let Some(min) = min_weight {
                        if rec.weight < min {
                            continue;
                        }
                    }
                    let other = if take_to { rec.to } else { rec.from };
                    out.push((*e, other));
                }
            }
        };
        match direction {
            Direction::Outbound => push_from(&self.store.out_adj, true),
            Direction::Inbound => push_from(&self.store.in_adj, false),
            Direction::Both => {
                push_from(&self.store.out_adj, true);
                push_from(&self.store.in_adj, false);
            }
        }
        // Already deterministic without a sort: the adjacency yields
        // `(edge_type, edge_id)` order (BTreeMap over types, each type's edge
        // list kept id-sorted), and for `Both` all outbound steps precede all
        // inbound. Dropping the sort makes expanding a high-degree hub O(degree)
        // instead of O(degree·log degree) — the cost that dominated the M3
        // `shortest_path` / hub-`neighbors` tails.
        out
    }

    pub(crate) fn resolve_type_ids(&self, types: &[EdgeType]) -> Option<HashSet<u32>> {
        if types.is_empty() {
            return None;
        }
        Some(
            types
                .iter()
                .filter_map(|t| self.store.edge_types.get(t.as_str()))
                .collect(),
        )
    }

    /// List a node's neighbors (PRD §9.3).
    pub fn neighbors(&self, node: NodeId, opts: NeighborOptions) -> Result<Vec<Neighbor>> {
        if !self.store.nodes.contains_key(&node.0) {
            return Err(Error::NodeNotFound(node));
        }
        let type_ids = self.resolve_type_ids(&opts.edge_types);
        let steps = self.steps(node.0, opts.direction.into(), &type_ids, opts.min_weight);
        Ok(steps
            .into_iter()
            .map(|(e, other)| {
                let rec = &self.store.edges[&e];
                Neighbor {
                    node: NodeId(other),
                    via: EdgeId(e),
                    edge_type: EdgeType(
                        self.store
                            .edge_types
                            .label(rec.edge_type)
                            .unwrap()
                            .to_string(),
                    ),
                    weight: rec.weight,
                }
            })
            .collect())
    }

    /// Bounded n-hop traversal returning paths (PRD §9.3). Depth-first with
    /// `(edge_type, edge_id)`-ordered expansion; stops at `max_hops` and at `max_paths`.
    pub fn traverse(&self, from: NodeId, mut opts: TraversalOptions) -> Result<Vec<Path>> {
        if !self.store.nodes.contains_key(&from.0) {
            return Err(Error::NodeNotFound(from));
        }
        // Resolve the effective hop budget: the caller's value, or the graph's
        // configured `default_max_hops` when unset (PRD §9.1). Then hard-cap it —
        // the DFS recurses one frame per hop, so an unbounded value could blow the
        // stack; a graph deeper than this is out of scope for bounded traversal.
        opts.max_hops = Some(
            opts.max_hops
                .unwrap_or(self.config.default_max_hops)
                .min(MAX_TRAVERSAL_HOPS),
        );
        let type_ids = self.resolve_type_ids(&opts.edge_types);
        let mut paths: Vec<Path> = Vec::new();
        let mut nodes_stack = vec![from.0];
        let mut edges_stack: Vec<u64> = Vec::new();
        let mut on_path: HashSet<u64> = HashSet::from([from.0]);
        self.dfs(
            &opts,
            &type_ids,
            &mut nodes_stack,
            &mut edges_stack,
            &mut on_path,
            &mut paths,
        );
        Ok(paths)
    }

    #[allow(clippy::too_many_arguments)]
    fn dfs(
        &self,
        opts: &TraversalOptions,
        type_ids: &Option<HashSet<u32>>,
        nodes_stack: &mut Vec<u64>,
        edges_stack: &mut Vec<u64>,
        on_path: &mut HashSet<u64>,
        paths: &mut Vec<Path>,
    ) {
        if paths.len() >= opts.max_paths {
            return;
        }
        // A path of length ≥ 1 hop is a result.
        if !edges_stack.is_empty() {
            paths.push(Path {
                nodes: nodes_stack.iter().map(|n| NodeId(*n)).collect(),
                edges: edges_stack.iter().map(|e| EdgeId(*e)).collect(),
                cost: edges_stack.len() as f32,
            });
            if paths.len() >= opts.max_paths {
                return;
            }
        }
        // `traverse` resolves max_hops to `Some(effective)` before invoking dfs.
        if edges_stack.len() >= opts.max_hops.unwrap_or(0) {
            return;
        }
        let current = *nodes_stack.last().unwrap();
        for (edge, other) in self.steps(current, opts.direction.into(), type_ids, opts.min_weight) {
            if opts.cycle_policy == CyclePolicy::NoRevisit && on_path.contains(&other) {
                continue;
            }
            nodes_stack.push(other);
            edges_stack.push(edge);
            let inserted = on_path.insert(other);
            self.dfs(opts, type_ids, nodes_stack, edges_stack, on_path, paths);
            if inserted {
                on_path.remove(&other);
            }
            nodes_stack.pop();
            edges_stack.pop();
            if paths.len() >= opts.max_paths {
                return;
            }
        }
    }

    /// Shortest path from `from` to `to` (PRD §9.3). Unweighted mode is BFS
    /// (fewest hops); weighted mode is Dijkstra over summed edge weight.
    pub fn shortest_path(
        &self,
        from: NodeId,
        to: NodeId,
        opts: ShortestPathOptions,
    ) -> Result<Option<Path>> {
        if !self.store.nodes.contains_key(&from.0) {
            return Err(Error::NodeNotFound(from));
        }
        if !self.store.nodes.contains_key(&to.0) {
            return Err(Error::NodeNotFound(to));
        }
        let type_ids = self.resolve_type_ids(&opts.edge_types);
        match opts.cost_mode {
            CostMode::UnweightedHops => Ok(self.bfs_path(from.0, to.0, &opts, &type_ids)),
            CostMode::WeightedCost => Ok(self.dijkstra_path(from.0, to.0, &opts, &type_ids)),
        }
    }

    fn bfs_path(
        &self,
        from: u64,
        to: u64,
        opts: &ShortestPathOptions,
        type_ids: &Option<HashSet<u32>>,
    ) -> Option<Path> {
        use std::collections::VecDeque;
        let mut prev: HashMap<u64, (u64, u64)> = HashMap::new(); // node -> (prev_node, via_edge)
        let mut seen: HashSet<u64> = HashSet::from([from]);
        let mut q = VecDeque::from([from]);
        let mut steps = 0usize;
        while let Some(cur) = q.pop_front() {
            if cur == to {
                return Some(self.reconstruct(from, to, &prev, CostMode::UnweightedHops));
            }
            steps += 1;
            if opts.max_steps.is_some_and(|max| steps > max) {
                return None; // exploration budget exhausted (M3 F1)
            }
            for (edge, other) in self.steps(cur, opts.direction.into(), type_ids, opts.min_weight) {
                if seen.insert(other) {
                    prev.insert(other, (cur, edge));
                    q.push_back(other);
                }
            }
        }
        None
    }

    fn dijkstra_path(
        &self,
        from: u64,
        to: u64,
        opts: &ShortestPathOptions,
        type_ids: &Option<HashSet<u32>>,
    ) -> Option<Path> {
        let mut dist: HashMap<u64, f32> = HashMap::from([(from, 0.0)]);
        let mut prev: HashMap<u64, (u64, u64)> = HashMap::new();
        let mut heap: BinaryHeap<DijkstraState> = BinaryHeap::from([DijkstraState {
            cost: 0.0,
            node: from,
        }]);
        let mut steps = 0usize;
        while let Some(DijkstraState { cost, node }) = heap.pop() {
            if node == to {
                return Some(self.reconstruct(from, to, &prev, CostMode::WeightedCost));
            }
            if cost > *dist.get(&node).unwrap_or(&f32::INFINITY) {
                continue; // stale heap entry — not a real expansion, do not count
            }
            steps += 1;
            if opts.max_steps.is_some_and(|max| steps > max) {
                return None; // exploration budget exhausted (M3 F1)
            }
            for (edge, other) in self.steps(node, opts.direction.into(), type_ids, opts.min_weight)
            {
                let w = self.store.edges[&edge].weight.max(0.0);
                let next = cost + w;
                if next < *dist.get(&other).unwrap_or(&f32::INFINITY) {
                    dist.insert(other, next);
                    prev.insert(other, (node, edge));
                    heap.push(DijkstraState {
                        cost: next,
                        node: other,
                    });
                }
            }
        }
        None
    }

    fn reconstruct(
        &self,
        from: u64,
        to: u64,
        prev: &HashMap<u64, (u64, u64)>,
        cost_mode: CostMode,
    ) -> Path {
        let mut nodes = vec![to];
        let mut edges = Vec::new();
        let mut cur = to;
        while cur != from {
            let (p, e) = prev[&cur];
            nodes.push(p);
            edges.push(e);
            cur = p;
        }
        nodes.reverse();
        edges.reverse();
        let cost = match cost_mode {
            CostMode::UnweightedHops => edges.len() as f32,
            CostMode::WeightedCost => edges
                .iter()
                .map(|e| self.store.edges[e].weight.max(0.0))
                .sum(),
        };
        Path {
            nodes: nodes.into_iter().map(NodeId).collect(),
            edges: edges.into_iter().map(EdgeId).collect(),
            cost,
        }
    }
}

/// Min-heap state for Dijkstra (ordered so `BinaryHeap` pops the lowest cost).
struct DijkstraState {
    cost: f32,
    node: u64,
}
impl PartialEq for DijkstraState {
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost && self.node == other.node
    }
}
impl Eq for DijkstraState {}
impl Ord for DijkstraState {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse cost for min-heap; tie-break on node id for determinism.
        other
            .cost
            .total_cmp(&self.cost)
            .then_with(|| other.node.cmp(&self.node))
    }
}
impl PartialOrd for DijkstraState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
