# M5 Query-Seam Decision

Status: v0.1, 2026-07-02. The PRD §21 M5 deliverable is "the
`PropertyGraphRead` adapter trait, a survey of Cypher parser and executor
options, and a decision: adapter, custom limited parser, or defer." This
records that decision.

## Delivered

`PropertyGraphRead` ships in `drey::read` (implemented for `Graph`): typed node
and edge lookup, filtered node/edge scans returning iterators, and a directional
`expand`. This is the seam a query layer compiles against (PRD §12.1). It is a
read adapter only, carries no query language, and is not a v0.1 core dependency
of anything.

## Survey (Rust Cypher / GQL options, 2026)

- **Full openCypher/GQL engines** (server-class): reintroduce query planning,
  mutation clauses, and broad compatibility obligations — exactly the
  "query-language gravity" risk of PRD §23.2. Rejected for a primitive.
- **Embeddable parser-only crates** (parse Cypher to an AST, no executor):
  viable as the front half of an adapter, leaving execution to compile onto
  `PropertyGraphRead`. Promising but unproven against this trait.
- **A small internal pattern-query adapter** (bounded `MATCH`/`WHERE`/`RETURN`
  over the seam, per PRD §12.2): smallest surface, no external dependency, but
  is new code to own.

## Decision: **defer** (adapter seam only in v0.1)

Per PRD §12.3, query support is not a release blocker, and v0.1 ships the
Rust-native API plus the `PropertyGraphRead` seam. A query layer becomes a v0.2
item once a survey proves an existing parser can be adapted onto the seam
without dragging server semantics back in. The seam exists so that decision
costs no core rework later; the language binding is deliberately not built now.

Open question 6 (Cypher vs GQL subset vs internal pattern-query adapter) stays
open, now narrowed: the front half will be an existing embeddable parser if one
adapts cleanly onto `PropertyGraphRead`, else the small internal adapter of
§12.2. Decided in v0.2 against a real consumer query workload, not before.
