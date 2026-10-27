//! Transitive dependency queries over the change graph, memoized per change.
//!
//! The name-resolution reduce step asks "does change A depend on change B"
//! for every pair of events on a path. A memo keyed by the pair still
//! re-walked A's whole dependency graph for every distinct B, and each step
//! of that walk is a database lookup keyed by the change's string form: a
//! 427-commit import spent hours in it. Each change's ancestor set is now
//! computed once and shared by every query about it.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::rc::Rc;

/// Memoized transitive dependency closures keyed by a change identity.
pub(crate) struct DependencyClosures<K> {
    ancestors: HashMap<K, Rc<HashSet<K>>>,
}

impl<K: Copy + Eq + Hash> Default for DependencyClosures<K> {
    fn default() -> Self {
        Self {
            ancestors: HashMap::new(),
        }
    }
}

impl<K: Copy + Eq + Hash> DependencyClosures<K> {
    /// Whether `descendant` is `ancestor` or transitively depends on it.
    /// `dependencies` lists a change's direct dependencies.
    pub(crate) fn depends_on<E>(
        &mut self,
        descendant: K,
        ancestor: K,
        dependencies: &mut impl FnMut(K) -> Result<Vec<K>, E>,
    ) -> Result<bool, E> {
        if descendant == ancestor {
            return Ok(true);
        }
        Ok(self
            .ancestors_of(descendant, dependencies)?
            .contains(&ancestor))
    }

    /// The transitive dependency set of `change`, computed once. The walk is
    /// iterative, so a long linear history cannot overflow the stack, and a
    /// change whose closure is already known is merged instead of re-walked.
    fn ancestors_of<E>(
        &mut self,
        change: K,
        dependencies: &mut impl FnMut(K) -> Result<Vec<K>, E>,
    ) -> Result<Rc<HashSet<K>>, E> {
        if let Some(known) = self.ancestors.get(&change) {
            return Ok(Rc::clone(known));
        }
        let mut closure = HashSet::new();
        let mut pending = vec![change];
        while let Some(node) = pending.pop() {
            for dependency in dependencies(node)? {
                if !closure.insert(dependency) {
                    continue;
                }
                match self.ancestors.get(&dependency) {
                    Some(known) => closure.extend(known.iter().copied()),
                    None => pending.push(dependency),
                }
            }
        }
        let closure = Rc::new(closure);
        self.ancestors.insert(change, Rc::clone(&closure));
        Ok(closure)
    }
}

#[cfg(test)]
mod tests {
    use super::DependencyClosures;
    use std::cell::Cell;
    use std::collections::HashMap;

    fn graph(edges: &[(u32, &[u32])]) -> HashMap<u32, Vec<u32>> {
        edges
            .iter()
            .map(|(node, deps)| (*node, deps.to_vec()))
            .collect()
    }

    #[test]
    fn a_chain_depends_transitively_in_one_direction() {
        // 4 -> 3 -> 2 -> 1
        let graph = graph(&[(4, &[3]), (3, &[2]), (2, &[1]), (1, &[])]);
        let mut closures = DependencyClosures::default();
        let mut deps = |node| Ok::<_, ()>(graph.get(&node).cloned().unwrap_or_default());
        assert!(closures.depends_on(4, 1, &mut deps).unwrap());
        assert!(closures.depends_on(4, 4, &mut deps).unwrap());
        assert!(!closures.depends_on(1, 4, &mut deps).unwrap());
        assert!(!closures.depends_on(2, 3, &mut deps).unwrap());
    }

    #[test]
    fn a_diamond_and_a_cycle_terminate() {
        // 5 -> {4, 3}; 4 -> 2; 3 -> 2; 2 -> 5 (cycle)
        let graph = graph(&[(5, &[4, 3]), (4, &[2]), (3, &[2]), (2, &[5])]);
        let mut closures = DependencyClosures::default();
        let mut deps = |node| Ok::<_, ()>(graph.get(&node).cloned().unwrap_or_default());
        assert!(closures.depends_on(5, 2, &mut deps).unwrap());
        // 4 -> 2 -> 5 -> 3 through the cycle.
        assert!(closures.depends_on(4, 3, &mut deps).unwrap());
        assert!(!closures.depends_on(9, 2, &mut deps).unwrap());
    }

    #[test]
    fn a_change_is_walked_once_for_every_ancestor_asked_about() {
        let graph = graph(&[(3, &[2]), (2, &[1]), (1, &[])]);
        let calls = Cell::new(0usize);
        let mut closures = DependencyClosures::default();
        let mut deps = |node| {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(graph.get(&node).cloned().unwrap_or_default())
        };
        assert!(closures.depends_on(3, 1, &mut deps).unwrap());
        let after_first = calls.get();
        assert!(closures.depends_on(3, 2, &mut deps).unwrap());
        assert!(!closures.depends_on(3, 7, &mut deps).unwrap());
        assert_eq!(calls.get(), after_first, "later queries reuse the closure");
    }

    #[test]
    fn a_dependency_error_propagates() {
        let mut closures = DependencyClosures::default();
        let mut deps = |node: u32| {
            if node == 2 {
                Err("unregistered")
            } else {
                Ok(vec![2])
            }
        };
        assert_eq!(closures.depends_on(1, 3, &mut deps), Err("unregistered"));
    }
}
