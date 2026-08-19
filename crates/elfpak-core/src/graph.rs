//! The dependency graph records both *what* is included and *why*.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::elf::Architecture;
use crate::source::SymlinkEntry;

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(pub String);

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Executable,
    Interpreter,
    SharedObject,
}

#[derive(Debug, Clone)]
pub struct Node {
    /// Host path the bytes are read from.
    pub source: PathBuf,
    /// Logical path inside the source root.
    pub logical: PathBuf,
    /// Logical path inside the generated rootfs.
    pub destination: PathBuf,
    pub kind: NodeKind,
    pub soname: Option<String>,
    pub architecture: Architecture,
    pub sha256: Digest,
    pub size: u64,
    /// Symlinks traversed to reach this object, preserved in the output.
    pub links: Vec<SymlinkEntry>,
    /// `dlopen`-family references found in this object.
    pub dlopen_references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyReason {
    Interpreter,
    Needed { soname: String },
    RuntimePolicy { feature: &'static str },
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub reason: DependencyReason,
}

#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    pub root: NodeId,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// `PT_INTERP` exactly as declared by the executable, before symlinks are
    /// followed. This is the path the kernel will use at runtime.
    pub declared_interpreter: Option<PathBuf>,
    by_logical: HashMap<PathBuf, NodeId>,
}

impl DependencyGraph {
    pub fn new() -> DependencyGraph {
        DependencyGraph::default()
    }

    /// Insert a node, deduplicating on the logical source path.
    ///
    /// Re-inserting a known object merges its symlink chain: the same library
    /// is often reached through several link paths (`/lib64/ld-linux…` and
    /// `/lib/<tuple>/ld-linux…`), and every one of them must be preserved.
    pub fn insert(&mut self, node: Node) -> NodeId {
        if let Some(&id) = self.by_logical.get(&node.logical) {
            let existing = &mut self.nodes[id];
            for link in node.links {
                if !existing.links.contains(&link) {
                    existing.links.push(link);
                }
            }
            return id;
        }
        let id = self.nodes.len();
        self.by_logical.insert(node.logical.clone(), id);
        self.nodes.push(node);
        id
    }

    pub fn find(&self, logical: &Path) -> Option<NodeId> {
        self.by_logical.get(logical).copied()
    }

    pub fn connect(&mut self, from: NodeId, to: NodeId, reason: DependencyReason) {
        if self
            .edges
            .iter()
            .any(|e| e.from == from && e.to == to && e.reason == reason)
        {
            return;
        }
        self.edges.push(Edge { from, to, reason });
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    pub fn root_node(&self) -> &Node {
        &self.nodes[self.root]
    }

    /// Direct dependencies of a node, in insertion order.
    pub fn dependencies(&self, id: NodeId) -> Vec<(&Edge, &Node)> {
        self.edges
            .iter()
            .filter(|e| e.from == id)
            .map(|e| (e, &self.nodes[e.to]))
            .collect()
    }

    /// First object that pulled in `id`, used for diagnostics and manifests.
    pub fn first_dependent(&self, id: NodeId) -> Option<(&Edge, &Node)> {
        self.edges
            .iter()
            .find(|e| e.to == id)
            .map(|e| (e, &self.nodes[e.from]))
    }

    pub fn shared_objects(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::SharedObject)
    }

    pub fn total_size(&self) -> u64 {
        self.nodes.iter().map(|n| n.size).sum()
    }
}
