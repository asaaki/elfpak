//! The dependency graph records both *what* is included and *why*.

use crate::{
    elf::Architecture,
    error::{Error, Result},
    rootfs::policy::RuntimeFeature,
    source::SymlinkEntry,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

/// Index of a node in [`DependencyGraph::nodes`].
pub type NodeId = u32;

/// Upper bound on the objects in one runtime closure.
///
/// A real closure is tens of objects; a thousand would already be remarkable.
/// The limit bounds work requested by a synthetic or malformed ELF graph.
pub const NODES_MAX: usize = 4096;

/// Upper bound on edges. Every edge is one `DT_NEEDED`, `PT_INTERP` or policy
/// reason, so this allows an average of 64 dependencies per object.
pub const EDGES_MAX: usize = NODES_MAX * 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(pub String);

/// Length of a SHA-256 digest in lowercase hexadecimal.
pub const DIGEST_LEN_HEX: usize = 64;

impl Digest {
    /// Whether this is a well-formed SHA-256 digest.
    pub fn is_well_formed(&self) -> bool {
        self.0.len() == DIGEST_LEN_HEX
            && self
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
    }
}

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
    RuntimePolicy { feature: RuntimeFeature },
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
    /// `DT_RPATH` and `DT_RUNPATH` of the executable, verbatim and unexpanded.
    /// They travel with the binary, so they matter when it is installed
    /// somewhere other than where it was built.
    pub executable_search_paths: Vec<String>,
    by_logical: HashMap<PathBuf, NodeId>,
}

impl DependencyGraph {
    pub fn new() -> DependencyGraph {
        DependencyGraph::default()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Insert a node, deduplicating on the logical source path.
    ///
    /// Re-inserting a known object merges its symlink chain: the same library
    /// is often reached through several link paths (`/lib64/ld-linux…` and
    /// `/lib/<tuple>/ld-linux…`), and every one of them must be preserved.
    pub fn insert(&mut self, node: Node) -> Result<NodeId> {
        assert!(node.logical.is_absolute());
        assert!(node.destination.is_absolute());
        assert!(node.sha256.is_well_formed());

        if let Some(&id) = self.by_logical.get(&node.logical) {
            let existing = &mut self.nodes[id as usize];
            for link in node.links {
                if !existing.links.contains(&link) {
                    existing.links.push(link);
                }
            }
            return Ok(id);
        }

        if self.nodes.len() >= NODES_MAX {
            return Err(Error::LimitExceeded {
                resource: "runtime closure",
                limit: NODES_MAX,
            });
        }
        let id = NodeId::try_from(self.nodes.len()).expect("node count is bounded by NODES_MAX");
        self.by_logical.insert(node.logical.clone(), id);
        self.nodes.push(node);
        assert_eq!(self.nodes.len(), self.by_logical.len());
        Ok(id)
    }

    pub fn find(&self, logical: &Path) -> Option<NodeId> {
        assert!(logical.is_absolute());
        self.by_logical.get(logical).copied()
    }

    pub fn connect(&mut self, from: NodeId, to: NodeId, reason: DependencyReason) -> Result<()> {
        assert!(self.contains(from));
        assert!(self.contains(to));
        if from == to {
            // The loader considers an object that is already mapped to satisfy
            // a self-reference; it adds no useful edge to the closure.
            return Ok(());
        }

        let known = self
            .edges
            .iter()
            .any(|e| e.from == from && e.to == to && e.reason == reason);
        if known {
            return Ok(());
        }
        if self.edges.len() >= EDGES_MAX {
            return Err(Error::LimitExceeded {
                resource: "runtime dependency graph",
                limit: EDGES_MAX,
            });
        }
        self.edges.push(Edge { from, to, reason });
        Ok(())
    }

    pub fn contains(&self, id: NodeId) -> bool {
        (id as usize) < self.nodes.len()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        assert!(self.contains(id));
        &self.nodes[id as usize]
    }

    pub fn root_node(&self) -> &Node {
        let root = self.node(self.root);
        assert_eq!(root.kind, NodeKind::Executable);
        root
    }

    /// Direct dependencies of a node, in insertion order.
    pub fn dependencies(&self, id: NodeId) -> Vec<(&Edge, &Node)> {
        assert!(self.contains(id));
        self.edges
            .iter()
            .filter(|e| e.from == id)
            .map(|e| (e, self.node(e.to)))
            .collect()
    }

    /// First object that pulled in `id`, used for diagnostics and manifests.
    pub fn first_dependent(&self, id: NodeId) -> Option<(&Edge, &Node)> {
        assert!(self.contains(id));
        self.edges
            .iter()
            .find(|e| e.to == id)
            .map(|e| (e, self.node(e.from)))
    }

    /// Nodes paired with their ids, in insertion order. The only way to obtain
    /// a [`NodeId`] for a node short of looking it up by its logical path.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter().enumerate().map(|(index, node)| {
            let id = NodeId::try_from(index).expect("node count is bounded by NODES_MAX");
            (id, node)
        })
    }

    pub fn shared_objects(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|n| n.kind == NodeKind::SharedObject)
    }

    pub fn total_size(&self) -> u64 {
        self.nodes.iter().map(|n| n.size).sum()
    }

    /// Nodes reachable from the executable through its own ELF dependencies.
    ///
    /// Objects that only runtime policy asked for (NSS modules and their own
    /// dependencies) stay out of this set. They are in the image, but the
    /// application never declared them.
    pub fn application_closure(&self) -> HashSet<NodeId> {
        assert!(self.contains(self.root));

        let mut reached = HashSet::from([self.root]);
        let mut queue = vec![self.root];
        // A node is queued only when it is first reached, so the walk is
        // bounded by the size of the graph.
        while let Some(id) = queue.pop() {
            for edge in self.edges.iter().filter(|e| e.from == id) {
                if matches!(edge.reason, DependencyReason::RuntimePolicy { .. }) {
                    continue;
                }
                if reached.insert(edge.to) {
                    queue.push(edge.to);
                }
            }
        }
        reached
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        elf::{ElfClass, Endianness, Machine},
        hash::sha256_bytes,
    };

    fn node(path: PathBuf) -> Node {
        Node {
            source: path.clone(),
            logical: path.clone(),
            destination: path,
            kind: NodeKind::SharedObject,
            soname: None,
            architecture: Architecture {
                machine: Machine::X86_64,
                class: ElfClass::Elf64,
                endianness: Endianness::Little,
            },
            sha256: sha256_bytes(b"test"),
            size: 0,
            links: Vec::new(),
            dlopen_references: Vec::new(),
        }
    }

    #[test]
    fn an_oversized_closure_is_an_error() {
        let mut graph = DependencyGraph::new();
        for index in 0..NODES_MAX {
            graph
                .insert(node(PathBuf::from(format!("/lib/{index}"))))
                .unwrap();
        }

        let error = graph
            .insert(node(PathBuf::from("/lib/overflow")))
            .unwrap_err();
        assert!(matches!(
            &error,
            Error::LimitExceeded {
                resource: "runtime closure",
                limit: NODES_MAX,
            }
        ));
        assert_eq!(error.code(), "E1005");
    }

    #[test]
    fn a_self_dependency_does_not_add_an_edge() {
        let mut graph = DependencyGraph::new();
        let id = graph.insert(node(PathBuf::from("/lib/self.so"))).unwrap();
        graph
            .connect(
                id,
                id,
                DependencyReason::Needed {
                    soname: "self.so".to_string(),
                },
            )
            .unwrap();
        assert!(graph.edges.is_empty());
    }
}
