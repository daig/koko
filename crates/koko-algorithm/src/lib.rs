//! Dense, storage-agnostic graph algorithm kernels.
//!
//! Kernels operate on statement-local dense vertex identifiers through narrow,
//! statically dispatched input contracts. Catalog lookup, MVCC visibility,
//! property evaluation, and result materialization remain processor concerns.

mod k_core;
mod louvain;
mod page_rank;
mod strongly_connected_components;
mod topological_levels;
mod weakly_connected_components;

pub use k_core::{KCoreDecomposition, KCoreGraph, UNASSIGNED_CORE, k_core_decomposition};
pub use louvain::{LouvainCommunities, LouvainGraph, louvain};
pub use page_rank::{PageRankConfig, PageRankGraph, PageRankScores, page_rank};
pub use topological_levels::{
    TopologicalGraph, TopologicalLevels, UNRANKED_LEVEL, topological_levels,
};

pub use strongly_connected_components::{
    StronglyConnectedComponents, StronglyConnectedGraph, UNASSIGNED_COMPONENT,
    strongly_connected_components,
};
pub use weakly_connected_components::{
    UNASSIGNED_COMPONENT_ID, WeaklyConnectedComponents, WeaklyConnectedGraph,
    weakly_connected_components,
};
