# drey: Embedded Graph Substrate Crate

Status: PRD draft v0.8, 2026-07-02. Independent project. The crate is named
`drey` (renamed 2026-07-02 from the working name `weaver-graph`). The crate is
developed on its own track and integrated by consumers, WeaverTools among
them, when it reaches a good point. Companion to the Guiding Statement and
Working Rules.

Changes from v0.7: incorporated requirements derived from the reference
consumer (Bucy, 2026), sorted against the mechanic-versus-motive line so only
substrate concerns entered the crate. External addressing removed entirely:
no `external_id`, internal IDs are the sole identity, durable across reload,
and consumers hold them natively (open question 4 closed as removed). Property
query gains equality-and-range over scalars in the core surface, with a
`PropertyQuery` and `ScalarPredicate` definition, and the scalar index is
specified to serve both. Empty-not-error added for registered-empty-type
queries. Byte-exact round-trip for `Bytes` and `f32` embeddings made explicit.
Read-only open promoted from optional to contractual. Binary on-disk encoding
committed as a design decision justified by fidelity and cold-start decode,
with the codec choice and file structure left measurement-gated for M2. Import
defined as exact ID-space restoration. A fsync-backed durability floor recorded
as consumer input to the M2 durability decision. Two new open questions: the
binary codec choice, and uncommitted-mutation discard semantics. Monotonicity
clarified as a uniqueness-and-allocation property only, with recency left to a
consumer sequence property.

The reference consumer's contract is authored on the consumer side as an
executable conformance suite, not in this PRD. The crate carries only the
substrate clauses above, no knowledge of the consumer.

Changes from v0.6: internal-consistency pass (Bucy, 2026), propagating the
v0.6 cuts to every place they had not reached. Open question 3 replaced with
the timestamp-removal record. Scope and data-model property descriptions
tightened from "arbitrary" to the v0.1 value set. The metadata composition
pattern corrected from cross-graph edges (which would break one-process-one-
graph) to a metadata subgraph inside the same graph linked by ordinary typed
edges. Export prose generalized from naming GraphSAGE and RGCN as the consumer
to "structural-embedding pipeline," consistent with their example-only status
in section 14.

Changes from v0.5: incorporated second external review (Bucy, 2026). Restored
the section 16.5 measurement dimensions, lost to a heading-renumber in v0.5.
`GraphConfig` responsibilities named. M2 now must state the durability level
`commit` guarantees. Rust concurrency claim softened to account for
consumer-side synchronization wrappers. Nested property values (`Map`, nested
`List`) cut from v0.1, with the metadata-graph composition pattern given as
the supported alternative. Core timestamps (`created_at`, `updated_at`) cut,
consumers store them as properties. Open question 5 generalized from GraphSAGE
to structural-embedding export. Timestamp and nesting cuts are recorded as
boundary decisions, not deferred measurements: both carry a motive that
belongs to the consumer, and measurement would not move that.

Changes from v0.4 (retained): category softened to underserved, SQLite-class
as operational shape, mandatory similarity budget, reconstruction-only
persistence invariant and recovery matrix, type interning, `RemoveNodeMode`
with safe default, budget-table template, single `commit` verb.

Two v0.5 review suggestions declined with rationale in open questions:
`open_or_create` (a convenience verb carrying consumer policy) and a
pre-emptive `commit`/`flush` split (would decide the persistence design by
naming before measurement).

Changes from v0.3 (retained): independent-crate reframe, consumer profile,
WeaverTools as reference case, migration as consumer concern, M0 fallback.

Changes from v0.2 (retained): conveyance removed, ArangoDB comparison and
hardware baselines removed, memory-primary commitment, vectors in scope with
generation out, budgets replace comparison gates.

## 1. Summary

`drey` is an embedded Rust crate that provides a local property-graph
substrate to any process that links it. The product identity is SQLite-class:
a library, not a server. SQLite-class describes operational shape, not storage
strategy: a linked library, local file ownership, no daemon, no listener, no
service account, and no separate process to operate. Unlike SQLite, which is
disk-primary, `drey` is memory-primary. It links into the consuming
process, holds the working graph in memory, persists durably to local disk,
and provides typed nodes, typed directed weighted edges, property lookup,
bounded traversal, shortest path, vector similarity search over stored
embeddings, and graph-feature export for structural-embedding workflows.

The crate is an independent project. It is developed and versioned on its own
track and carries no dependency on, or reference to, any particular consumer.
An agent harness that needs durable local graph state is one consumer profile.
A process serving an upstream model is another. WeaverTools is the reference
consumer and will integrate the crate when it matures, but the crate is not
built for WeaverTools and must not encode anything specific to it.

The crate is a primitive, not a reasoning layer. It stores, indexes, persists,
mutates, traverses, and searches graph data. Consumers decide what nodes and
edges mean, when weights change, how decay is scheduled, what gets embedded,
what similarity signifies, and whether any query result should influence
downstream behavior.

The product succeeds if it gives a consuming process a small, fast, local
graph substrate that keeps all graph state inside that process, removes any
shared database daemon from the process inventory, and exposes stable
contracts for higher-level consumers. Where the consumer is an OS-isolated
agent, this preserves kernel-level isolation. The property holds for any
consumer, because the substrate never leaves the process that links it.

## 2. Problem

The category this crate targets is underserved. Rust has mature embedded
relational storage in SQLite and several mature embedded key-value stores, but
there is no obvious SQLite-class property-graph crate that combines weighted
directed edges, bounded traversal, durable local persistence, and composable
vector similarity in one small in-process substrate. Projects that need those
mechanics locally reach for a server-class graph database and carry it as a
resident daemon.

The reference case is an agent harness backed by ArangoDB, which is where the
gap was first felt and which grounds the argument in a concrete workload. That
arrangement fails on two grounds. First, process weight: the workload
exercises a narrow slice of the product, typed edges, multi-hop traversal,
property lookup, and hybrid semantic-relational queries, while the rest of the
daemon, cluster coordination, query planning breadth, Foxx services, and user
management, is carried but never used. Second, isolation: a shared daemon
enforces its boundary with application-layer database credentials, whereas a
consumer that wants its state to live and die inside its own process gets no
such guarantee from a separate daemon. Running one daemon per consuming
process restores the boundary but multiplies a heavyweight process, which
fails the first ground harder.

Process weight and process-local containment are the whole justification. No
further argument is needed or made. The reference case is one instance of the
gap, not its definition. Any project that wants embedded graph mechanics
without a resident daemon has the same problem.

## 3. Goals

### 3.1 Product goals

1. Provide an embedded, in-process property-graph store for a single
   consuming process, with kernel-level isolation available to consumers that
   run OS-isolated.
2. Support typed nodes, typed directed edges, mutable edge weights, property
   maps over the v0.1 value set, and bounded traversal.
3. Store consumer-supplied embedding vectors as first-class node data and
   provide similarity search over them, composable with structural and
   property predicates in a single query.
4. Operate memory-primary: the working graph lives in RAM, disk provides
   durability, and the two are reconciled through explicit persistence
   operations.
5. Persist graph state locally without a daemon, network listener, cluster
   mode, or internal multi-tenant access-control model.
6. Export graph topology and features through a stable trait so external
   structural-embedding implementations can consume graph data without making
   this crate depend on a GNN library.
7. Provide a query integration seam, preferably for a read-only Cypher
   subset, without committing the crate to owning a bespoke query language.
8. Make correctness, latency, and footprint tradeoffs measurable before
   committing to a final persistence design or API stabilization.

### 3.2 Non-goals

1. This crate does not encode any consumer's policy, identity model, belief
   semantics, axioms, smells, gates, or governance mechanisms. Those live in
   consumers.
2. This crate does not provide multi-tenant access control. The intended
   deployment model is one graph instance inside one consuming process.
3. This crate does not provide cluster coordination, sharding, replication,
   backup orchestration, or distributed consistency.
4. This crate does not implement a general-purpose graph database server and
   does not chase feature parity with any server-class product.
5. This crate does not generate embeddings. Vector computation belongs to the
   consumer's semantic processing layer. The crate stores vectors it is
   handed and searches over them.
6. This crate does not own model inference, ranking policy, or semantic
   interpretation of similarity results.
7. This crate does not provide migration tooling from any existing database.
   Moving a consumer's data off its current store is that consumer's concern.
   A one-shot fixture export used to build test and benchmark data is
   permitted as deliberately throwaway apparatus and must not be promoted to a
   supported migration path without a separate decision.

## 4. Users and consumers

### 4.1 Consumer profile

The crate is built for a consumer profile, not one named consumer: a single
process that wants durable graph state held inside its own process boundary,
with weighted edges, bounded traversal, and vector similarity in one query
surface, on laptop-class or server-class hardware, with no assumption beyond a
filesystem and enough RAM for the working graph. The consuming process owns
the graph file or directory on disk according to OS permissions.

Consumers that fit the profile include:

- An agent harness needing persistent local state. WeaverTools is the
  reference instance and the first case the crate will be measured against.
- A process serving an upstream model that wants a co-resident graph without
  a separate service.
- Graph-backed retrieval components embedded in a larger tool.
- Structural-embedding exporters that produce GraphSAGE-compatible feature
  tensors or edge-index layouts.
- Debugging and inspection tools that need read-only access to snapshots.

### 4.2 Explicitly unsupported consumers

The crate is not built for a web service exposing graph access to multiple
tenants, a general graph database server, an analytics engine for massive
offline graphs, or a reasoning engine that interprets graph semantics
internally.

## 5. Scope

### 5.1 In scope

- Embedded, in-process graph store, memory-primary.
- No daemon and no network listener.
- Typed nodes with scalar, bytes, and list-of-scalar property maps.
- Typed, weighted, directed edges with scalar, bytes, and list-of-scalar
  property maps.
- Stable node and edge identifiers within a graph instance.
- Mutable edge weight as a first-class field.
- Explicit edge-weight update operations with optional bounds.
- Explicit batch edge-decay operations.
- First-class embedding vectors on nodes, with declared dimensionality per
  node type.
- Similarity search over stored vectors, expressed as a predicate composable
  with node type, property, reachability, and edge-weight filters.
- Neighbor listing filtered by edge type, direction, and optional minimum
  weight.
- Bounded n-hop traversal filtered by edge type, direction, and optional
  minimum weight.
- Shortest path over unweighted hops and weighted cost mode.
- Property lookup over node type, edge type, and selected indexed properties.
- Durable local persistence to a file or directory with explicit commit
  semantics.
- Single process, single writer.
- Snapshot, export, and import primitives for testing and interoperability.
- Trait-based graph-feature export for GNN consumers.
- Query integration seam for a small external query layer, read-only
  initially.

### 5.2 Out of scope

- Built-in schemas for agent cognition or governance.
- Embedding generation, in any form.
- Built-in scheduling policy for weight decay.
- Background autonomous mutation unless explicitly invoked by the consumer.
- Cross-process graph sharing.
- Server mode and cloud deployment.
- Query-language ownership.
- Approximate-nearest-neighbor index structures in v0.1. See section 13.
- Transaction isolation beyond the single-process requirements selected for
  v0.1.

## 6. Design principles

1. **Mechanic, not motive.** The crate provides graph mechanics only. It must
   not encode why a fact matters, whether an edge is trusted, what an
   embedding represents, or how an agent should reason over a result.
2. **One process, one graph.** Isolation belongs outside the crate, at the
   OS and process boundary.
3. **SQLite-class weight.** The crate is judged by what it adds to the
   consuming process: link size, resident memory, open time, and zero
   operational surface. Heaviness anywhere is a defect even when performance
   is adequate.
4. **Memory-primary.** The working graph is a RAM structure. Disk is
   durability, not the query path. Traversal and search never touch storage.
5. **Measured minimalism.** The feature set is derived from the consumer's
   demonstrated query and update patterns, not from any other database's
   feature surface.
6. **Stable contracts, replaceable internals.** Consumers depend on the graph
   API and export traits, not on the persistence design.
7. **Readability before cleverness.** The initial implementation should be
   inspectable and easy to measure. Premature compression, custom binary
   layout, or unsafe indexing is deferred until profiling justifies it.
8. **Policy stays above.** Decay triggers, pruning decisions, schema
   interpretation, ranking, and belief updates belong to the consumer.
9. **Rust-native first.** Prefer pure Rust dependencies where performance is
   adequate. FFI-backed dependencies are acceptable only if measurement shows
   a clear need.

## 7. Data model

### 7.1 Node

A node is an addressable typed record.

Fields:

- `NodeId`: stable identifier within a graph instance.
- `NodeType`: caller-defined type label.
- `properties`: caller-defined map over the v0.1 value set, scalars, bytes,
  and lists of scalars.
- `embedding`: optional fixed-dimension vector, dimensionality declared per
  node type at type registration. Absent by default.

The crate does not carry `created_at` or `updated_at` as core fields. A
substrate-generated timestamp encodes mutation-time semantics, which is a
motive that belongs to the consumer. A consumer that wants timestamps stores
them as ordinary properties. This is a boundary decision, not a deferred
measurement: measurement would not change who owns the meaning of a timestamp.

The crate does not interpret `NodeType`. It may validate that the type is
syntactically valid, but it must not reserve names for cognitive or
governance semantics. The crate does not interpret embedding contents. It
enforces only dimensionality against the node type's declaration.

### 7.2 Edge

An edge is an addressable directed relationship from one node to another.

Fields:

- `EdgeId`: stable identifier within a graph instance.
- `from: NodeId` and `to: NodeId`.
- `EdgeType`: caller-defined type label.
- `weight: f32` or `f64`, final precision selected by measurement and export
  needs.
- `properties`: caller-defined map over the v0.1 value set, scalars, bytes,
  and lists of scalars.

Edges carry no core timestamps either, for the reason given under nodes.

Edges are directed. Undirected behavior is represented by two directed edges
or by a consumer-level convention. The crate does not add hidden reciprocal
edges unless the API explicitly requests them. Edges do not carry first-class
embedding vectors. A consumer that needs a vector on an edge stores it as an
ordinary property, unindexed, until a demonstrated query pattern justifies
promotion.

### 7.3 Property values

Property value options for v0.1: `Null`, `Bool`, `I64`, `F64`, `String`,
`Bytes`, and `List` of scalars. Nested structures are not supported: no `Map`
value, and no `List` of `List` or `List` of `Map`. Indexes cover scalar
values only.

Nesting is excluded on the same mechanic-versus-motive ground as timestamps.
A nested map is a way to organize data hierarchically, and how a consumer
organizes its data is the consumer's reasoning, not the substrate's. Nesting
would also enlarge the correctness surface exactly where the recovery matrix
must hold: recursive serialization, deep equality, and order preservation are
where round-trip divergence hides.

The supported pattern for hierarchical metadata is composition, not nesting.
A consumer that needs structured metadata builds a metadata subgraph inside
the same graph and links it to primary nodes with ordinary typed edges, or
serializes the structure to `Bytes` and owns its deserialization. Either keeps
the graph schema focused and queryable and keeps the substrate out of the
consumer's organizing choices. Nesting may return in a later version if a
consumer proves a need the composition pattern cannot meet, decided then, not
carried now.

### 7.4 Identifier policy

Internal IDs are the sole identity and the sole addressing scheme. They are
monotonic primary keys, and they are durable across persistence and reload: a
node keeps the same `NodeId` across a close and reopen, and an edge keeps the
same `EdgeId`. This is a structural invariant, not a measurement-gated choice.
Edges are stored as ID pairs, so a non-durable ID would be a broken edge on
reload. The persistence design must honor durable IDs, it does not get to
choose them away. The encoding therefore stores each ID explicitly, never by
array position or load-order index, since position-based IDs are a
renumber-on-load trap that durable IDs exist to avoid.

The crate exposes no external addressing scheme. There is no `external_id`. A
consumer runs in the same process and holds `NodeId`s and `EdgeId`s natively,
mapping whatever keys it cares about to those IDs in its own structures and
persisting that mapping on its own side. Absorbing a consumer's string-key
addressing as an indexed crate feature would be the crate taking on a consumer
motive. The consumer owns its addressing, the crate owns identity.

Monotonicity is a uniqueness-and-allocation property only. The allocator hands
out increasing IDs, and the crate promises uniqueness and durability, not that
insertion order is a recency signal. A consumer that wants recency stores a
scalar sequence number as a property at write time and range-sorts on it, per
section 8. Leaning on internal-ID order as a recency proxy would couple the
consumer to an allocation accident, which is the dependency the current
ArangoDB path carries and this crate does not reproduce.

## 8. Indexing requirements

The minimal index set is derived from the demonstrated workload.

Required indexes for v0.1:

- Node by `NodeId`.
- Edge by `EdgeId`.
- Outbound adjacency: `(from, edge_type) -> [EdgeId]`.
- Inbound adjacency: `(to, edge_type) -> [EdgeId]`.
- Node type: `node_type -> [NodeId]`.
- Edge type: `edge_type -> [EdgeId]`.
- Scalar property index supporting equality and range:
  `(node_type, property_key, property_value) -> [NodeId]`, ordered so that
  both `property_value = x` and `property_value` in a range resolve through
  the index rather than a scan.

Deferred indexes:

- Compound property indexes.
- Full-text indexes.
- Temporal indexes.
- Approximate-nearest-neighbor vector indexes. See section 13.
- Community or precomputed path indexes.

The crate exposes index configuration explicitly and does not index every
property by default. Vector similarity in v0.1 is served by exhaustive scan,
not an index, per section 13.

## 9. API surface

The API is organized in four layers: mutation, query and traversal,
similarity, and export.

### 9.1 Core types

```rust
pub struct Graph;
pub struct GraphConfig;
pub struct NodeId(u64);
pub struct EdgeId(u64);
pub struct NodeType(String);
pub struct EdgeType(String);
pub enum Direction { Outbound, Inbound, Both }
pub enum Value { Null, Bool(bool), I64(i64), F64(f64), String(String),
    Bytes(Vec<u8>), List(Vec<Scalar>) }
pub enum Scalar { Bool(bool), I64(i64), F64(f64), String(String) }
pub type Properties = BTreeMap<String, Value>;
pub struct Embedding(Vec<f32>);
```

`GraphConfig` names the graph's construction and operating policy. It is
referenced throughout the API and carries, at minimum:

- Persistence mode, once section 10.3 resolves the durability design.
- The similarity scan ceiling: a candidate limit or latency budget, per
  section 13.
- A default maximum traversal step budget.
- Which properties are indexed, per section 8.
- Read-only open mode. Contractual, not optional: an inspection consumer must
  be able to open a graph file read-only, and the consumer lifecycle depends
  on it (a stopped agent's graph read for analysis while nothing writes it).
- Optional file-lock behavior against concurrent process writers.
- Embedding limits, such as a maximum dimensionality accepted at type
  registration.

The concrete struct is fixed during M1 once the durability and scan-ceiling
locations are decided. The list above names the responsibilities the config
owns, not a frozen field set.

### 9.2 Mutation API

```rust
impl Graph {
    pub fn open(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self>;
    pub fn create(path: impl AsRef<Path>, config: GraphConfig) -> Result<Self>;

    pub fn register_node_type(&mut self, node_type: NodeType,
        embedding_dim: Option<usize>) -> Result<()>;

    pub fn add_node(&mut self, node_type: NodeType,
        properties: Properties) -> Result<NodeId>;
    pub fn set_node_embedding(&mut self, node: NodeId,
        embedding: Embedding) -> Result<()>;
    pub fn update_node_properties(&mut self, node: NodeId,
        patch: PropertyPatch) -> Result<()>;
    pub fn remove_node(&mut self, node: NodeId,
        mode: RemoveNodeMode) -> Result<()>;

    pub fn add_edge(&mut self, from: NodeId, to: NodeId, edge_type: EdgeType,
        weight: f32, properties: Properties) -> Result<EdgeId>;
    pub fn update_edge_weight(&mut self, edge: EdgeId,
        update: WeightUpdate) -> Result<f32>;
    pub fn update_edge_properties(&mut self, edge: EdgeId,
        patch: PropertyPatch) -> Result<()>;
    pub fn remove_edge(&mut self, edge: EdgeId) -> Result<()>;

    pub fn decay_edges(&mut self, filter: EdgeFilter,
        factor: f32) -> Result<DecayReport>;
    pub fn commit(&mut self) -> Result<()>;
}
```

The v0.1 API exposes a single durability verb, `commit`, which makes all prior
mutations part of the durable graph state. Whether a second verb is needed to
separate logical commit from filesystem flush depends on the persistence
design chosen in section 10.3 (write-ahead log versus embedded KV durability
layer), so the durability vocabulary is fixed when that decision resolves at
M2, not before. Naming both verbs now would decide the persistence design by
its API surface ahead of measurement.

Weight updates name the operation and carry optional bounds as a field rather
than a peer variant, so that clamping is a constraint on an update instead of
an operation with no independent meaning:

```rust
pub struct WeightUpdate {
    pub op: WeightOp,
    pub bounds: Option<(f32, f32)>,
}

pub enum WeightOp {
    Set(f32),
    Add(f32),
    Multiply(f32),
}
```

Node removal names its edge-handling mode explicitly, and the default is the
safe one: reject removal while incident edges exist, so a caller cannot orphan
edges by accident:

```rust
pub enum RemoveNodeMode {
    RejectIfEdgesExist, // default
    RemoveIncidentEdges,
}
```

Caller-facing node and edge types are strings. The implementation may intern
them into compact numeric IDs for adjacency indexing, filtering, and export.
Those internal IDs are not stable public API and are never exposed, per the
stable-contracts-replaceable-internals principle.

### 9.3 Query and traversal API

```rust
impl Graph {
    pub fn node(&self, id: NodeId) -> Result<Option<Node>>;
    pub fn edge(&self, id: EdgeId) -> Result<Option<Edge>>;

    pub fn nodes_by_type(&self, node_type: &NodeType) -> Result<Vec<NodeId>>;
    pub fn nodes_by_property(&self, query: PropertyQuery) -> Result<Vec<NodeId>>;

    pub fn neighbors(&self, node: NodeId,
        opts: NeighborOptions) -> Result<Vec<Neighbor>>;
    pub fn traverse(&self, from: NodeId,
        opts: TraversalOptions) -> Result<Vec<Path>>;
    pub fn shortest_path(&self, from: NodeId, to: NodeId,
        opts: ShortestPathOptions) -> Result<Option<Path>>;
}
```

Traversal options include `max_hops`, `direction`, an `edge_types` allowlist,
`min_weight`, `max_paths`, `cycle_policy`, `cost_mode`, and a step budget if
needed.

The core property query supports equality and range over scalar values, the
two predicate shapes the required scalar index resolves without a scan.
Anything richer, set exclusion, compound sorts, arbitrary boolean
combinations, is not in the core surface. A consumer composes those in its own
Rust by filtering the returned set. This is the split: indexed equality and
range live in the crate, arbitrary predicates live in the consumer.

```rust
pub struct PropertyQuery {
    pub node_type: NodeType,
    pub key: String,
    pub predicate: ScalarPredicate,
}

pub enum ScalarPredicate {
    Eq(Scalar),
    Range { min: Option<Scalar>, max: Option<Scalar> }, // half-open bounds allowed
}
```

`nodes_by_property` and `nodes_by_type` return an empty vector, never an error,
when the queried type is registered and has zero matching members. A consumer
registers all its node types at open, so a cold-start read against a
not-yet-populated type is normal and must not fault. Querying an unregistered
type is a distinct case and may error.

### 9.4 Similarity API

Similarity is a predicate that composes with the structural and property
filters, not a separate search subsystem. The hybrid query is the reason the
capability lives in the crate at all, so the API expresses composition
directly:

```rust
pub struct SimilarityQuery {
    pub vector: Embedding,
    pub metric: SimilarityMetric,
    pub k: usize,
    pub node_types: Option<Vec<NodeType>>,
    pub property_filter: Option<PropertyQuery>,
    pub within: Option<ReachabilityFilter>,
}

pub enum SimilarityMetric { Cosine, Dot, Euclidean }

pub struct ReachabilityFilter {
    pub from: NodeId,
    pub max_hops: usize,
    pub edge_types: Option<Vec<EdgeType>>,
    pub min_weight: Option<f32>,
    pub direction: Direction,
}

impl Graph {
    pub fn similar_nodes(&self, query: SimilarityQuery)
        -> Result<Vec<(NodeId, f32)>>;
}
```

Evaluation order applies structural and property filters first, then scans
the surviving candidates' vectors exhaustively. The crate never ranks beyond
raw metric score. Interpretation of scores is consumer policy.

### 9.5 Export API

```rust
pub trait GraphFeatureExport {
    type NodeFeature;
    type EdgeFeature;

    fn node_count(&self) -> usize;
    fn edge_count(&self) -> usize;
    fn node_features(&self, spec: &FeatureSpec) -> Result<Vec<Self::NodeFeature>>;
    fn edge_index(&self, filter: EdgeFilter) -> Result<Vec<(usize, usize)>>;
    fn edge_weights(&self, filter: EdgeFilter) -> Result<Vec<f32>>;
    fn edge_types(&self, filter: EdgeFilter) -> Result<Vec<u32>>;
}
```

The crate does not choose a training library. It exposes predictable topology
and feature arrays that an external structural-embedding pipeline can consume.
Stored embeddings are exportable as node features through `FeatureSpec`.

## 10. Architecture: memory-primary with durable persistence

### 10.1 Commitment

The working graph is an in-memory structure. All query, traversal, and
similarity operations run against RAM. Disk exists for durability and
restart, reached through explicit persistence operations. This is a
commitment, not a backend detail: it rules out designs where traversal
performs storage reads, and it sizes the crate's target at graphs that fit
comfortably in a consuming process's memory budget.

The persisted representation is a reconstruction mechanism, not an alternate
source of truth during query execution. It rebuilds the in-memory graph at
open time and is never part of the query path in v0.1. Any design that would
consult disk to answer a query has left the memory-primary commitment.

### 10.1.1 Binary encoding

Nodes and edges are encoded to disk as binary, not as text. This is a design
commitment stated now, not a measurement-gated choice, and it is justified by
fidelity rather than preference. Embeddings are vectors of `f32` and the
`Bytes` value type holds opaque blobs. A binary codec writes and reads those
bytes exactly, which is what the recovery matrix's no-divergence promise and
the byte-exact `Bytes` requirement demand. Text encoding of floats forces
formatting and parse-time reconstruction where precision drifts, and it makes
scalar types ambiguous on the way back in. Binary also decodes faster than
text parses, and full-graph decode sits on the cold-start budget.

Two encoding choices remain measurement-gated and belong to M2, because their
answers depend on numbers the fixture supplies. First, which binary codec: a
self-describing one such as MessagePack or CBOR, which eases schema evolution
against the format-versioning requirement at some size and speed cost, versus a
tight schema-bound one such as bincode, which is smaller and faster but makes
forward-compatibility the crate's own problem through explicit version gates.
Second, the on-disk file structure, which is the separate section 10.3
decision. Committing to binary does not decide the file structure, because
both candidate designs in section 10.3 store binary. "We store binary" fixes
the encoding, not the structure, and must not be read as settling 10.3.

The encoding stores each node and edge ID explicitly, per the section 7.4
durable-ID guarantee. Position-based or load-order IDs are excluded because
durable IDs require that identity survive reconstruction rather than be
reassigned at load.

### 10.2 Persistence requirements

The persistence layer must support:

- Local file or directory persistence.
- Crash-safe writes or a clearly documented durability mode.
- Full-graph load at open into the in-memory structure.
- Explicit commit semantics per section 9.2.
- Byte-exact round-trip of `Bytes` values and `f32` embeddings, asserted, not
  assumed.
- Durable IDs across reload per section 7.4, so import is exact ID-space
  restoration, not key-matching reassignment.
- Snapshot and export for test reproducibility.
- Format versioning from the first release.
- Forward-compatible migration hooks, even though migration tooling is out
  of scope.

### 10.2.1 Recovery matrix

The durability mode must produce these behaviors, and the M2 tests assert
them:

| Failure | Required behavior |
|---------|-------------------|
| Crash before commit | The last committed graph loads. Uncommitted mutations are lost. |
| Crash during commit | Either the prior committed graph or the new committed graph loads, never a partial blend. |
| Corrupt log or snapshot tail | The last valid snapshot or log prefix loads, or open fails explicitly. Never a silent partial load. |
| Format version mismatch | Open fails with `VersionMismatch` unless an export or import path performs conversion. |

### 10.3 Candidate designs

1. **In-memory graph structure plus write-ahead log with periodic
   snapshots.** The leading candidate. Mutations append to a log, snapshots
   compact it, open replays snapshot plus tail. Maximum control over the
   in-memory layout, simple durability story, and the hot path never touches
   storage machinery.
2. **In-memory graph structure plus an embedded KV store as the durability
   layer.** redb or similar holds the persistent image, the graph loads
   fully at open. Less code than a hand-rolled log, at the cost of carrying
   a storage engine whose query machinery goes unused.
3. **KV store as the primary structure with caching.** Rejected by the
   memory-primary commitment. Recorded here so the rejection is explicit.

The public API sits behind a persistence trait either way, and the choice
between designs one and two is made by measuring load time, commit cost, and
code weight on the representative fixture.

## 11. Concurrency model

v0.1 is single process, single writer, synchronous API, no internal async
runtime dependency. The crate does not impose an executor on consumers. The
core API follows Rust ownership: reads borrow `&self`, mutations borrow
`&mut self`, so ordinary safe use does not permit a read to overlap a write
without an explicit consumer-side synchronization wrapper. Internal
locking is avoided in v0.1 unless measurement shows a need for shared
concurrent access behind `Arc`. Whether concurrent readers are needed inside
one consuming process is answerable by measurement: instrument whether
anything reads the graph concurrently today, and let that observation close
the question rather than carrying it as an open design axis. If a consumer
later needs non-blocking behavior, an optional async wrapper follows the
synchronous core.

## 12. Query-layer integration

A query layer remains a seam, not a v0.1 core dependency. Cypher is the
leading candidate for that layer but not a settled destination.

### 12.1 Query posture

The crate exposes a read adapter trait rather than owning a query language:

```rust
pub trait PropertyGraphRead {
    fn get_node(&self, id: NodeId) -> Result<Option<Node>>;
    fn get_edge(&self, id: EdgeId) -> Result<Option<Edge>>;
    fn scan_nodes(&self, filter: NodeFilter)
        -> Result<Box<dyn Iterator<Item = NodeId> + '_>>;
    fn scan_edges(&self, filter: EdgeFilter)
        -> Result<Box<dyn Iterator<Item = EdgeId> + '_>>;
    fn expand(&self, from: NodeId, pattern: ExpandPattern)
        -> Result<Vec<EdgeTraversal>>;
}
```

A Cypher adapter can compile a limited query subset to this trait.

### 12.2 Candidate read-only pattern-query subset

This is a candidate subset, not a commitment to Cypher as the destination. The
survey in open question 6 may land on a GQL subset or a smaller internal
pattern-query adapter instead. Read-only: `MATCH` with node labels and
relationship types, directional edge patterns, bounded variable-length paths
where they map cleanly to traversal options, `WHERE` over scalar equality and
range predicates, `RETURN` of nodes, edges, paths, and scalar properties, and
`LIMIT`. Mutation clauses, aggregation, arbitrary expressions, and query
planning are deferred.

### 12.3 Decision gate

Query support is not a release blocker. The v0.1 core ships with the
Rust-native API. A query layer becomes v0.2 only after a survey proves an
existing parser and executor can be adapted without dragging server semantics
back into the primitive.

## 13. Vector similarity: scan first, index seam later

### 13.1 Posture

v0.1 serves similarity queries by exhaustive scan over the candidate set that
survives structural and property filtering. At agent-scale graphs the
filtered candidate set is small, a SIMD-friendly scan over f32 vectors is
fast, and exhaustive scan composes exactly with the hybrid query shape
because filters run first and only survivors are scored.

The scan is bounded. Every similarity query obeys a configurable candidate
limit or latency budget carried in `GraphConfig` or the query itself. A query
with no structural or property narrowing still obeys that ceiling. An
unfiltered full-graph vector scan is permitted only when the caller or a test
harness requests it explicitly. This prevents a caller from turning
`similar_nodes` into an accidental full vector-database scan and then charging
the cost to the crate.

Approximate-nearest-neighbor structures are excluded from v0.1 by the same
reasoning that defers Cypher. An ANN index earns its complexity on large
unfiltered candidate sets, brings awkward incremental deletion, must persist
and reload coherently with the graph, and works against memory-primary
simplicity. The exclusion is a measured posture, not a permanent one.

### 13.2 Index seam

Similarity evaluation sits behind an internal trait so an ANN structure can
replace the scan without API change. The trait graduates from seam to
implementation only when measurement on the representative fixture shows the
scan exceeding its latency budget. If it graduates, the crate integrates an
existing Rust ANN crate rather than implementing one.

### 13.3 Embedding lifecycle boundary

The crate stores, persists, exports, and scores vectors. It never computes
them. Dimensionality is enforced per node type. Everything else about an
embedding, model, version, meaning, staleness, is consumer metadata, stored
in ordinary properties if the consumer wants it tracked.

## 14. GNN and structural-embedding integration

The crate supports export, not training. GraphSAGE and RGCN appear in this
document only as example downstream pipelines. The crate commits to neither.
The choice of embedding pipeline, and specifically whether it is inductive
(such as GraphSAGE, which embeds nodes unseen at training time by aggregating
over their neighborhoods) or transductive (retrained as the graph changes),
carries a motive that belongs to the consumer running its consolidation, not
to the substrate. The crate's obligation is to export topology and features in
a layout either kind of pipeline can consume. That decision lives in a
consumer's spec, on its own track, with its own rationale.

Required export forms:

- Dense node index mapping: `NodeId` to contiguous `usize` and back.
- Edge index as two aligned arrays or a vector of `(src, dst)` pairs.
- Edge weights aligned to the edge index.
- Edge type IDs aligned to the edge index.
- Node type IDs.
- Optional node feature matrix produced from selected properties and stored
  embeddings.

Constraints: export is deterministic for reproducibility, filterable by edge
type and minimum weight, framework-agnostic, and able to stream or chunk if
graph size exceeds a memory budget. RGCN-specific batching, neighbor
sampling APIs, GPU tensor ownership, and direct ML-framework dependencies
are deferred.

## 15. Observability and debugging

The crate exposes lightweight instrumentation without a full observability
stack: node and edge counts overall and by type, storage size, resident
in-memory size, query and traversal latency histogram hooks, traversal steps
visited, paths returned, similarity candidates scanned, weight updates
applied, decay operation counts, and snapshot and export duration.
Implementation posture: optional `tracing` spans behind a feature flag, a
`GraphStats` struct, and harness output in JSON so results compare over
time.

## 16. Performance requirements

### 16.1 Posture

Performance targets are budgets derived from a consumer's workload, not
comparisons against any other database. The crate sits inside the consuming
process's hot path. In the reference agent workload, traversal and similarity
queries happen at decision points, weight updates and decay happen during
consolidation passes, and graph open happens at process start. Each such
position imposes a budget, and the budgets are what the crate is measured
against. A different consumer profile will impose different positions and
different budgets, and the method is the same.

### 16.2 Budget derivation, part of M0

Budgets come from a reference workload. There are two ways to get one, and the
crate's development never blocks waiting on either.

Preferred: capture from a real consumer, the reference agent harness first.
Capture representative graph size (node count, edge count, edge type
cardinality, degree distribution, embedding dimensionality and coverage),
query shapes and rates (which traversals run where, how often, with what
filters, and what latency the consumer tolerates before it stalls), mutation
shapes and rates (weight-update and decay batch sizes, and the throughput the
consolidation window requires), open-time tolerance, and a resident-memory
budget as a fraction of the consuming process's memory.

Fallback: if no real consumer capture is cheaply available, derive the same
quantities from a synthetic fixture with parameters chosen to span the profile
in section 4.1. This keeps the crate independent and unblocked, at a stated
cost: synthetic budgets carry less authority than a captured workload, they
are provisional, and the first real consumer to supply a capture supersedes
them. The fixture's parameters are recorded so the gap between synthetic and
captured is visible rather than hidden.

This project must not delay or depend on any one consumer's roadmap. If
capturing the reference harness would require instrumentation that consumer is
not ready to add, the fallback path is taken and the capture is folded in
later.

### 16.3 v0.1 gates

The crate satisfies these before API stabilization, with thresholds filled
in from the M0 capture:

1. Traversal and similarity latency within the decision-point budget at
   representative graph size.
2. Mutation throughput within the consolidation-window budget.
3. Graph open time within the process-start budget.
4. Resident memory within the per-process budget at representative size, and
   link-size addition to the consuming binary recorded and reviewed.
5. No external daemon process and no network listener, verified by
   inspection.
6. Snapshot and export succeed deterministically on representative graph
   sizes.

### 16.4 Budget table template

M0 fills this table, one row per hot-path operation, with the fixture source
(captured or synthetic) recorded alongside. Cells are TBD until measured. The
table is the M3 pass/fail record.

| Operation | Representative size | p50 | p95 | p99 | Budget | Pass/fail |
|-----------|--------------------|-----|-----|-----|--------|-----------|
| neighbors | TBD | TBD | TBD | TBD | TBD | TBD |
| traverse max_hops=2 | TBD | TBD | TBD | TBD | TBD | TBD |
| traverse max_hops=5 | TBD | TBD | TBD | TBD | TBD | TBD |
| shortest_path | TBD | TBD | TBD | TBD | TBD | TBD |
| similar_nodes filtered | TBD | TBD | TBD | TBD | TBD | TBD |
| update_edge_weight | TBD | TBD | TBD | TBD | TBD | TBD |
| decay_edges batch | TBD | TBD | TBD | TBD | TBD | TBD |
| commit | TBD | TBD | TBD | TBD | TBD | TBD |
| open | TBD | TBD | TBD | TBD | TBD | TBD |

### 16.5 Measurement dimensions

Measure across:

- Graph sizes: small, representative, and stress.
- Edge fanout: low, medium, and high.
- Edge type cardinality.
- Weight-threshold selectivity.
- Property-index selectivity.
- Similarity candidate-set sizes.
- Workload mixes: update-heavy, traversal-heavy, similarity-heavy, and mixed.
- Cold start versus warm start.

## 17. Correctness requirements

The crate must guarantee:

- No dangling edges after node removal unless explicitly permitted by
  `RemoveNodeMode`.
- Stable ID lookup within a graph instance, and durable IDs across persistence
  and reload per section 7.4.
- Traversal respects direction, edge type filter, max hops, and weight
  threshold.
- Similarity results respect all composed filters and enforce dimensionality.
- Property queries return an empty vector, never an error, for a registered
  type with zero matching members.
- Weight update operations are atomic relative to the selected concurrency
  model, and bounds are applied as part of the update.
- Persistence round-trip preserves nodes, edges, properties, weights,
  embeddings, indexes, and IDs, with `Bytes` and `f32` embeddings byte-exact.
- Snapshot, import, and export preserve graph identity and format version
  metadata, and import restores the exact ID space.
- Corrupt or incompatible storage formats fail safely with explicit errors.
- Property indexes remain consistent after property mutation and deletion.

## 18. Security and isolation

The crate minimizes its internal security surface by not being a server.
There is no network listener, no authentication subsystem, no internal
multi-tenant authorization model, and no runtime execution of user-defined
query code. File permissions are left to OS owner, group, and mode, because
isolation is externalized to the OS boundary by design. All APIs return
typed errors, not panics, for malformed input or missing IDs. A read-only
open mode is contractual, for inspection consumers reading a graph nothing is
writing. Optional: file locking against accidental concurrent process
writers, and an integrity-check API for debugging.

## 19. Error model

A crate-level error enum with categories such as `Storage`, `Codec`,
`NotFound`, `InvalidNodeType`, `InvalidEdgeType`, `InvalidPropertyValue`,
`DimensionMismatch`, `DanglingEdge`, `IndexCorruption`, `UnsupportedQuery`,
`VersionMismatch`, and `LockConflict`. Errors include enough context for
debugging without leaking policy-layer assumptions.

## 20. Versioning and compatibility

Semantic versioning for the crate API. A graph format version stored in
persisted metadata. No storage-format stability promise before v1.0.
Explicit export and import as the compatibility bridge during pre-1.0
development, and a changelog of format changes.

## 21. Milestones

### M0: Workload capture and budget derivation

Deliverables:

- A representative graph fixture, either captured from a real consumer via a
  deliberately throwaway export script or generated synthetically per the
  section 16.2 fallback, documented and versioned with its source marked.
- Query, mutation, and similarity shapes with rates.
- The derived budgets of section 16.2.
- A measurement harness with JSON output.

Exit criteria: budgets are written down with numbers, the fixture source
(captured or synthetic) is recorded, and the harness runs repeatably against
the fixture. A synthetic fixture satisfies M0 and is superseded when a real
capture arrives.

### M1: In-memory prototype

Deliverables: core data model including embeddings, mutation API, neighbor
listing, bounded traversal, shortest path, property lookup, similarity scan,
all without persistence, plus unit tests for graph invariants.
Exit criteria: every query shape in the M0 fixture, captured or synthetic,
executes correctly in memory.

### M2: Persistence prototype

Deliverables: persistence trait, first durability implementation per the
section 10.3 decision, format metadata, commit semantics per section 9.2,
snapshot, export, and import, persistence round-trip tests including
embeddings, and the section 10.2.1 recovery-matrix tests.
Exit criteria: the representative fixture persists and reloads without
divergence in any index or vector, and every recovery-matrix row is asserted.
M2 must also state, in plain terms, what `commit` guarantees for the chosen
implementation: application-level durability (the mutation is in the durable
structure but may sit in OS buffers), OS-buffer durability, or fsync-backed
crash durability. The recovery matrix is only meaningful against a named
level, and a consumer must not be left to assume a stronger guarantee than
v0.1 provides.

Stated consumer requirement feeding this decision, from the reference agent
workload: at least one operation must offer fsync-backed crash durability,
because an agent's continuity depends on the moments it persists, the turn
boundary and the consolidation pass. Whether every `commit` is fsync-backed or
a separate stronger verb exists is the crate's M2 choice. That a fsync-backed
point exists at all is the consumer's requirement, recorded here as input, not
as a v0.1 API commitment.

### M3: Budget gate

Deliverables: measurement results for every section 16.3 gate at median,
p95, and p99 where latency-shaped, plus resident-memory and link-size
results.
Exit criteria: all budgets met, or a written reason to revise the
persistence or index design.

### M4: Feature export

Deliverables: `GraphFeatureExport` implementation, contiguous ID mapping,
edge index, weight and type export, embedding export through `FeatureSpec`,
deterministic export tests.
Exit criteria: an external structural-embedding experiment, GraphSAGE or RGCN
or other, consumes exported topology without crate-specific hacks.

Ordering note: M4 stays after M3. Feature export is not needed to define the
representative workload, so it does not gate budget derivation. If a future
consumer's workload were defined by its embedding pipeline, M4 would move ahead
of M3, but no current consumer requires that.

### M5: Query seam

Deliverables: the `PropertyGraphRead` adapter trait, a survey of Cypher
parser and executor options, and a decision: adapter, custom limited
parser, or defer.
Exit criteria: the query integration decision is made without blocking the
core crate release.

## 22. Acceptance criteria for v0.1

v0.1 is acceptable when:

1. The crate can create, open, mutate, persist, and reload a local graph,
   memory-primary, with IDs durable across reload.
2. Nodes and edges support caller-defined types and properties over the v0.1
   value set, and nodes support declared-dimension embeddings.
3. Edge weight update with bounds and explicit decay operations work and are
   tested.
4. Neighbor listing, n-hop traversal, and shortest path work with type,
   direction, and weight filters.
5. Similarity search and property queries work composed with structural
   filters, property queries support equality and range over scalars, and
   filter correctness is tested.
6. The measurement harness evaluates every section 16.3 budget gate.
7. The crate has no daemon, no listener, no internal multi-tenant access
   model, and no built-in cognitive or governance semantics.
8. The public API is documented enough for consumer integration.
9. A storage-format version is written into persisted graph state.
10. The GNN export trait is present or explicitly moved to v0.2 with
    rationale.
11. All open questions blocking implementation have been converted into
    decisions or milestone gates.

## 23. Risks

### 23.1 Rebuilding a graph database accidentally

Risk: the crate expands until it recreates a server-class graph database
poorly. Mitigation: the captured workload is the feature boundary. Any
feature not needed for lookup, bounded traversal, weighted edges,
similarity, persistence, or export requires explicit justification.

### 23.2 Query-language gravity

Risk: Cypher support pulls in server semantics, query planning, mutation
clauses, and broad compatibility obligations. Mitigation: Cypher stays an
adapter seam, read-only bounded subset, never v0.1 release-blocking.

### 23.3 ANN-index gravity

Risk: the similarity feature attracts an approximate-nearest-neighbor
index before measurement justifies one, bringing persistence coupling and
deletion complexity. Mitigation: exhaustive scan behind the index seam
until the M3 budget gate shows the scan failing, and integration of an
existing crate rather than implementation if it ever graduates.

### 23.4 Memory-primary ceiling

Risk: a consumer's graph outgrows the RAM budget and the memory-primary
commitment becomes a wall. Mitigation: the M0 capture sizes the
representative and stress fixtures, resident memory is a standing gate,
and the persistence trait leaves room for a paged design as a future major
version if real consumers ever approach the ceiling.

### 23.5 Weight-update write amplification

Risk: frequent weight updates cause excessive persistence churn.
Mitigation: measure update-heavy workloads in M3, and prefer log-append or
write-coalescing designs in section 10.3 evaluation.

### 23.6 Property model overgeneralization

Risk: the property model grows structure the substrate should not carry,
pulling serialization, deep equality, and nested-path query semantics into a
primitive. Mitigation: v0.1 property values are scalars, bytes, and lists of
scalars only. Nesting is cut, and hierarchical needs are met by the
metadata-graph composition pattern in section 7.3, not by richer values.

### 23.7 Hidden policy leakage

Risk: names, helper methods, or defaults encode consumer-specific meaning.
Mitigation: type labels stay caller-defined, no reserved cognitive names,
scheduling and pruning stay external, embedding semantics stay in consumer
properties.

## 24. Open questions

1. Persistence design one or two from section 10.3, decided by M2 and M3
   measurement.
2. Is `f32` sufficient for weights, or `f64` internally with `f32` export
   for ML consumers.
3. Removed in v0.6: core timestamps. Consumers store timestamp semantics as
   properties. Recorded here so the decision is not re-opened.
4. Removed in v0.8: external addressing. There is no `external_id`. The crate
   has no external addressing scheme, internal IDs are durable across reload
   per section 7.4, and consumers hold IDs natively. Recorded so the question
   is not re-opened.
5. Does structural-embedding export require neighbor sampling, or only
   full-graph export.
6. Should query integration target Cypher, a GQL subset, or a smaller
   internal pattern-query adapter.
7. Similarity metric set: are cosine, dot, and euclidean sufficient for the
   consumer workload, decided from the M0 capture.
8. Which binary codec: self-describing (MessagePack, CBOR) or tight
   schema-bound (bincode). Decided at M2 from decode time, size, and expected
   schema churn, per section 10.1.1.
9. Uncommitted-mutation discard semantics. The crate has no rollback verb:
   single-writer mutations accumulate and `commit` publishes them. What
   happens to accumulated-but-uncommitted mutations on an explicit discard, or
   on drop without commit, is an open design point for M1/M2. Stated as a
   question rather than assumed.
10. Declined in v0.5, recorded for revisiting: an `open_or_create` convenience
    constructor. Declined because merging the absent and present cases encodes
    a create-or-open policy, and an embedded-store consumer usually wants those
    two failure modes distinct. `open` fails if absent, `create` fails if
    present, and a consumer composes the third case if it wants it. Revisit only
    if real consumers repeatedly hand-write the same compose.
11. Declined in v0.5, recorded for revisiting: splitting durability into
    `commit` plus `flush` in the v0.1 API. Declined because the split would
    decide the persistence design by its API surface before the section 10.3
    measurement. Revisit at M2 once the durability design is chosen.
