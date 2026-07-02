# drey

An embedded property graph for Rust. For when your data is local and you need it in-process.

drey links into your process as a library and holds the working graph in memory. Nodes and edges are typed and carry properties, edges carry mutable weights, and a single query can compose traversal, property predicates, and vector similarity over stored embeddings. The graph persists to a local path your process owns and reloads with the same durable IDs it saved. There is no server, no daemon, no network listener, no service account, and nothing to operate. One process, one graph, single writer.
