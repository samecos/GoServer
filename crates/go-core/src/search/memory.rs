use super::*;
use std::mem::size_of;

impl Node {
    pub(super) fn initial_heap_charge(area: usize) -> usize {
        (area + 1) * size_of::<Edge>() + area * size_of::<f64>()
    }

    pub(super) fn heap_charge(&self, area: usize) -> usize {
        let arrays = self.edges.capacity() * size_of::<Edge>()
            + self.ownership.capacity() * size_of::<f64>();
        // Reserve the full policy and ownership before an asynchronous result
        // arrives. Failure/retry retains this reservation; completion releases
        // unused legal-move slots and absent ownership.
        let arrays = if matches!(self.state, State::Unevaluated | State::Evaluating(_)) {
            arrays.max(Self::initial_heap_charge(area))
        } else {
            arrays
        };
        arrays + self.linked_edges.capacity() * size_of::<u16>()
            // HashSet capacity excludes control bytes and empty buckets. 16
            // bytes per usable usize slot bounds both, including tiny tables.
            + self.parents.capacity() * 16
    }
}

impl Search {
    pub(super) fn node_structure_charge() -> usize {
        // Vec amortized capacity <= 2N, hash index bucket/control/slack and
        // allocator headers. Compaction workspace remains separately reserved.
        2 * size_of::<Node>() + 256 + Self::collection_reserve_per_node()
    }

    pub(super) fn link_growth_charge(
        &self,
        parent: usize,
        edge: usize,
        child: Option<usize>,
    ) -> usize {
        let n = &self.nodes[parent];
        let linked = if n.linked_edges.len() == n.linked_edges.capacity()
            && n.linked_edges.binary_search(&(edge as u16)).is_err()
        {
            (n.linked_edges.capacity().max(8) * 2 - n.linked_edges.capacity()) * size_of::<u16>()
        } else {
            0
        };
        let parents = child.map(|c| &self.nodes[c].parents);
        let reverse = match parents {
            Some(p) if p.len() < p.capacity() || p.contains(&parent) => 0,
            Some(p) => (p.capacity() + 8) * 16,
            None => 8 * 16,
        };
        linked + reverse
    }

    pub(super) fn attach_child(&mut self, parent: usize, edge: usize, child: usize) {
        let before = self.nodes[parent].linked_edges.capacity();
        self.nodes[parent].set_child(edge, child);
        self.graph_payload_bytes +=
            (self.nodes[parent].linked_edges.capacity() - before) * size_of::<u16>();
        let parents = &mut self.nodes[child].parents;
        let before = parents.capacity();
        // HashSet::insert may grow a full table even for an existing key.
        // Avoid that unneeded allocation on a rebound edge or graph cycle.
        if parents.len() < parents.capacity() || !parents.contains(&parent) {
            parents.insert(parent);
        }
        self.graph_payload_bytes += (parents.capacity() - before) * 16;
    }

    #[cfg(test)]
    pub(super) fn recount_payload(&mut self) {
        self.graph_payload_bytes = self
            .nodes
            .iter()
            .map(|n| n.heap_charge(self.position.board().len()))
            .sum();
    }
}
