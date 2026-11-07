//! The entity graph an assessment walks: parent/child links, blocking
//! relations (both directions), loose relations, and which entities the
//! request referenced but did not include.

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::model::{Entity, EntityId, RelationKind};

/// Ancestor walks stop after this many hops: deeper chains are degenerate
/// (real trees are a handful of levels deep) and count as having no common root.
pub(crate) const MAX_ANCESTRY: usize = 1024;

/// An indexed, immutable entity graph.
#[derive(Debug, Clone, Default)]
pub(crate) struct Graph {
    pub nodes: Vec<Entity>,
    /// Entities deleted by the assessed changes (kept so their children and
    /// dependents can still be found).
    pub deleted: Vec<bool>,
    pub index: HashMap<EntityId, usize>,
    pub parent: Vec<Option<usize>>,
    pub children: Vec<Vec<usize>>,
    /// `blocks[i]`: entities `i` blocks (downstream).
    pub blocks: Vec<Vec<usize>>,
    /// `blocked_by[i]`: entities blocking `i` (upstream).
    pub blocked_by: Vec<Vec<usize>>,
    pub related: Vec<Vec<usize>>,
    /// `(entity, referenced id)` for parents and relation targets not in the graph.
    pub missing: Vec<(usize, EntityId)>,
}

impl Graph {
    /// Builds the graph from `(entity, deleted)` pairs. Ids must be unique.
    pub(crate) fn build(entities: Vec<(Entity, bool)>) -> Self {
        let n = entities.len();
        let mut g = Graph {
            nodes: Vec::with_capacity(n),
            deleted: Vec::with_capacity(n),
            index: HashMap::with_capacity(n),
            parent: vec![None; n],
            children: vec![Vec::new(); n],
            blocks: vec![Vec::new(); n],
            blocked_by: vec![Vec::new(); n],
            related: vec![Vec::new(); n],
            missing: Vec::new(),
        };
        for (i, (entity, deleted)) in entities.into_iter().enumerate() {
            g.index.insert(entity.id.clone(), i);
            g.nodes.push(entity);
            g.deleted.push(deleted);
        }
        for i in 0..n {
            if let Some(pid) = g.nodes[i].parent_id.clone() {
                match g.index.get(&pid) {
                    Some(&p) if p != i => {
                        g.parent[i] = Some(p);
                        g.children[p].push(i);
                    }
                    Some(_) => {}
                    None => g.missing.push((i, pid)),
                }
            }
            let relations = g.nodes[i].relations.clone();
            for r in relations {
                let Some(&j) = g.index.get(&r.target) else {
                    g.missing.push((i, r.target.clone()));
                    continue;
                };
                if j == i {
                    continue;
                }
                match r.kind {
                    RelationKind::Blocks => {
                        g.blocks[i].push(j);
                        g.blocked_by[j].push(i);
                    }
                    RelationKind::BlockedBy => {
                        g.blocks[j].push(i);
                        g.blocked_by[i].push(j);
                    }
                    RelationKind::RelatesTo | RelationKind::Duplicates => {
                        g.related[i].push(j);
                        g.related[j].push(i);
                    }
                }
            }
        }
        for list in g
            .children
            .iter_mut()
            .chain(g.blocks.iter_mut())
            .chain(g.blocked_by.iter_mut())
            .chain(g.related.iter_mut())
        {
            list.sort_unstable();
            list.dedup();
        }
        g
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn get(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// Ancestors from the parent upwards, stopping at a cycle.
    #[cfg(test)]
    pub(crate) fn ancestors(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::from([i]);
        let mut cur = self.parent[i];
        while let Some(p) = cur {
            if !seen.insert(p) {
                break;
            }
            out.push(p);
            cur = self.parent[p];
        }
        out
    }

    /// Other children of `i`'s parent.
    #[cfg(test)]
    pub(crate) fn siblings(&self, i: usize) -> Vec<usize> {
        match self.parent[i] {
            Some(p) => self.children[p]
                .iter()
                .copied()
                .filter(|&c| c != i)
                .collect(),
            None => Vec::new(),
        }
    }

    /// The lowest common ancestor-or-self of all `nodes`, if they share one
    /// within [`MAX_ANCESTRY`] hops.
    ///
    /// The first node's chain is walked once; every other node walks up only
    /// until it meets that chain, so the cost is the sum of the distances to
    /// the chain, not `nodes × depth`.
    pub(crate) fn lowest_common_ancestor(&self, nodes: &[usize]) -> Option<usize> {
        let (&first, rest) = nodes.split_first()?;
        let mut chain = vec![first];
        let mut cur = self.parent[first];
        while let Some(p) = cur {
            if chain.len() > MAX_ANCESTRY || chain.contains(&p) {
                break;
            }
            chain.push(p);
            cur = self.parent[p];
        }
        let position: HashMap<usize, usize> =
            chain.iter().enumerate().map(|(i, &n)| (n, i)).collect();
        // Everything in `chain` below `lowest` is no longer common to all.
        let mut lowest = 0;
        for &n in rest {
            let mut cur = Some(n);
            let mut hops = 0;
            let met = loop {
                let Some(c) = cur else { break None };
                if let Some(&i) = position.get(&c) {
                    break Some(i);
                }
                hops += 1;
                if hops > MAX_ANCESTRY {
                    break None;
                }
                cur = self.parent[c];
            };
            lowest = lowest.max(met?);
        }
        chain.get(lowest).copied()
    }

    /// Every entity that structurally depends on `seeds` within `depth`
    /// hops: descendants, and what they (transitively) block.
    pub(crate) fn dependents(&self, seeds: &[usize], depth: u32) -> BTreeSet<usize> {
        let mut seen: BTreeSet<usize> = seeds.iter().copied().collect();
        let mut queue: VecDeque<(usize, u32)> = seeds.iter().map(|&s| (s, 0)).collect();
        while let Some((i, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for &j in self.children[i].iter().chain(&self.blocks[i]) {
                if seen.insert(j) {
                    queue.push_back((j, d + 1));
                }
            }
        }
        seen
    }

    /// Every entity within `depth` hops of `seeds` over any edge.
    pub(crate) fn neighbourhood(&self, seeds: &[usize], depth: u32) -> BTreeSet<usize> {
        let mut seen: BTreeSet<usize> = seeds.iter().copied().collect();
        let mut queue: VecDeque<(usize, u32)> = seeds.iter().map(|&s| (s, 0)).collect();
        while let Some((i, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            let neighbours = self.parent[i]
                .into_iter()
                .chain(self.children[i].iter().copied())
                .chain(self.blocks[i].iter().copied())
                .chain(self.blocked_by[i].iter().copied())
                .chain(self.related[i].iter().copied());
            for j in neighbours {
                if seen.insert(j) {
                    queue.push_back((j, d + 1));
                }
            }
        }
        seen
    }

    /// The groups of entities that block each other in a cycle (strongly
    /// connected components of the blocking graph with more than one
    /// member), restricted to `within`. Iterative Kosaraju.
    pub(crate) fn blocking_cycles(&self, within: &BTreeSet<usize>) -> Vec<Vec<usize>> {
        let mut order = Vec::with_capacity(within.len());
        let mut visited = vec![false; self.len()];
        for &start in within {
            if visited[start] {
                continue;
            }
            visited[start] = true;
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            while let Some(top) = stack.last_mut() {
                let (node, next) = *top;
                if let Some(&j) = self.blocks[node].get(next) {
                    top.1 += 1;
                    if within.contains(&j) && !visited[j] {
                        visited[j] = true;
                        stack.push((j, 0));
                    }
                } else {
                    order.push(node);
                    stack.pop();
                }
            }
        }
        let mut assigned = vec![false; self.len()];
        let mut components = Vec::new();
        for &start in order.iter().rev() {
            if assigned[start] {
                continue;
            }
            assigned[start] = true;
            let mut component = vec![start];
            let mut stack = vec![start];
            while let Some(node) = stack.pop() {
                for &j in &self.blocked_by[node] {
                    if within.contains(&j) && !assigned[j] {
                        assigned[j] = true;
                        component.push(j);
                        stack.push(j);
                    }
                }
            }
            if component.len() > 1 {
                component.sort_unstable();
                components.push(component);
            }
        }
        components.sort();
        components
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Entity, RelationKind};

    fn e(id: &str) -> Entity {
        Entity::new(id, "issue")
    }

    #[test]
    fn builds_links_and_reports_missing() {
        let g = Graph::build(vec![
            (e("p"), false),
            (
                e("a")
                    .with_parent("p")
                    .with_relation(RelationKind::Blocks, "b"),
                false,
            ),
            (
                e("b")
                    .with_parent("p")
                    .with_relation(RelationKind::BlockedBy, "a"),
                false,
            ),
            (
                e("c")
                    .with_parent("ghost")
                    .with_relation(RelationKind::RelatesTo, "a"),
                false,
            ),
        ]);
        let (p, a, b, c) = (0, 1, 2, 3);
        assert_eq!(g.children[p], vec![a, b]);
        assert_eq!(g.blocks[a], vec![b], "both directions dedupe into one edge");
        assert_eq!(g.blocked_by[b], vec![a]);
        assert_eq!(g.related[a], vec![c]);
        assert_eq!(g.siblings(a), vec![b]);
        assert_eq!(g.missing, vec![(c, "ghost".to_owned())]);
        assert_eq!(g.lowest_common_ancestor(&[a, b]), Some(p));
        assert_eq!(g.lowest_common_ancestor(&[a, c]), None);
        assert_eq!(g.lowest_common_ancestor(&[a]), Some(a));
        assert_eq!(g.lowest_common_ancestor(&[a, p]), Some(p));
        assert_eq!(g.dependents(&[p], 4), BTreeSet::from([p, a, b]));
    }

    #[test]
    fn lowest_common_ancestor_of_a_deep_chain_is_linear() {
        // A 40,000-deep parent chain with its bottom 4,000 entities as input:
        // quadratic before (every node walked the whole chain).
        let n = 40_000;
        let entities: Vec<(Entity, bool)> = (0..n)
            .map(|i| {
                let e = e(&format!("c{i}"));
                (
                    if i == 0 {
                        e
                    } else {
                        e.with_parent(format!("c{}", i - 1))
                    },
                    false,
                )
            })
            .collect();
        let g = Graph::build(entities);
        let started = std::time::Instant::now();
        let bottom: Vec<usize> = (n - 1_000..n).rev().collect();
        assert_eq!(g.lowest_common_ancestor(&bottom), Some(n - 1_000));
        // Beyond the ancestry cap there is no common root.
        let deeper: Vec<usize> = (n - 4_000..n).rev().collect();
        assert_eq!(g.lowest_common_ancestor(&deeper), None);
        assert_eq!(g.lowest_common_ancestor(&[n - 1, 0]), None);
        assert!(
            started.elapsed().as_secs_f64() < 1.0,
            "{:?}",
            started.elapsed()
        );
        // Parent cycles terminate.
        let cyc = Graph::build(vec![
            (e("x").with_parent("y"), false),
            (e("y").with_parent("x"), false),
            (e("z").with_parent("x"), false),
        ]);
        assert_eq!(cyc.lowest_common_ancestor(&[2, 1]), Some(1));
    }

    #[test]
    fn parent_cycles_do_not_hang() {
        let g = Graph::build(vec![
            (e("x").with_parent("y"), false),
            (e("y").with_parent("x"), false),
        ]);
        assert_eq!(g.ancestors(0), vec![1]);
    }

    #[test]
    fn finds_blocking_cycles() {
        let g = Graph::build(vec![
            (e("a").with_relation(RelationKind::Blocks, "b"), false),
            (e("b").with_relation(RelationKind::Blocks, "c"), false),
            (e("c").with_relation(RelationKind::Blocks, "a"), false),
            (e("d").with_relation(RelationKind::Blocks, "a"), false),
        ]);
        let all: BTreeSet<usize> = (0..4).collect();
        assert_eq!(g.blocking_cycles(&all), vec![vec![0, 1, 2]]);
        let n = g.neighbourhood(&[3], 1);
        assert_eq!(n, BTreeSet::from([0, 3]));
    }
}
