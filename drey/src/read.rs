//! The read-adapter seam for an external query layer (PRD §12).
//!
//! The crate exposes a read trait rather than owning a query language. A Cypher
//! (or GQL-subset) adapter can compile a bounded read-only subset to this
//! trait, but that adapter is never a v0.1 core dependency (PRD §12.3). This is
//! the seam only; the query-layer decision is M5.

use crate::error::Result;
use crate::graph::Graph;
use crate::mutation::EdgeFilter;
use crate::types::{Edge, EdgeId, Node, NodeId, NodeType};

/// A node scan filter for the read adapter.
#[derive(Clone, Default, Debug)]
pub struct NodeFilter {
    pub node_type: Option<NodeType>,
}

/// A directional expansion pattern for the read adapter (a thin projection of
/// the traversal surface).
#[derive(Clone, Debug)]
pub struct ExpandPattern {
    pub direction: crate::traverse::DirectionOpt,
    pub edge_types: Vec<crate::types::EdgeType>,
    pub min_weight: Option<f32>,
}

/// One expansion result: the edge taken and the node reached.
#[derive(Clone, Debug)]
pub struct EdgeTraversal {
    pub edge: EdgeId,
    pub neighbor: NodeId,
}

/// The read surface a query adapter compiles against (PRD §12.1).
pub trait PropertyGraphRead {
    fn get_node(&self, id: NodeId) -> Result<Option<Node>>;
    fn get_edge(&self, id: EdgeId) -> Result<Option<Edge>>;
    fn scan_nodes<'a>(
        &'a self,
        filter: NodeFilter,
    ) -> Result<Box<dyn Iterator<Item = NodeId> + 'a>>;
    fn scan_edges<'a>(
        &'a self,
        filter: EdgeFilter,
    ) -> Result<Box<dyn Iterator<Item = EdgeId> + 'a>>;
    fn expand(&self, from: NodeId, pattern: ExpandPattern) -> Result<Vec<EdgeTraversal>>;
}

impl PropertyGraphRead for Graph {
    fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        self.node(id)
    }

    fn get_edge(&self, id: EdgeId) -> Result<Option<Edge>> {
        self.edge(id)
    }

    fn scan_nodes<'a>(
        &'a self,
        filter: NodeFilter,
    ) -> Result<Box<dyn Iterator<Item = NodeId> + 'a>> {
        match filter.node_type {
            Some(t) => {
                let ids = self.nodes_by_type(&t)?;
                Ok(Box::new(ids.into_iter()))
            }
            None => {
                let mut ids: Vec<NodeId> = self.node_ids();
                ids.sort_unstable();
                Ok(Box::new(ids.into_iter()))
            }
        }
    }

    fn scan_edges<'a>(
        &'a self,
        filter: EdgeFilter,
    ) -> Result<Box<dyn Iterator<Item = EdgeId> + 'a>> {
        crate::graph::validate_min_weight(filter.min_weight)?;
        let ids: Vec<EdgeId> = self
            .edges_matching(&filter)
            .into_iter()
            .map(EdgeId)
            .collect();
        Ok(Box::new(ids.into_iter()))
    }

    fn expand(&self, from: NodeId, pattern: ExpandPattern) -> Result<Vec<EdgeTraversal>> {
        let neighbors = self.neighbors(
            from,
            crate::traverse::NeighborOptions {
                direction: pattern.direction,
                edge_types: pattern.edge_types,
                min_weight: pattern.min_weight,
            },
        )?;
        Ok(neighbors
            .into_iter()
            .map(|n| EdgeTraversal {
                edge: n.via,
                neighbor: n.node,
            })
            .collect())
    }
}
