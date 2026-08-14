export type DirectedEdge = {
  from: string;
  to: string;
};

function addNeighbor(
  adjacency: Map<string, Set<string>>,
  from: string,
  to: string,
): void {
  const neighbors = adjacency.get(from);
  if (neighbors) {
    neighbors.add(to);
  } else {
    adjacency.set(from, new Set([to]));
  }
}

function directedAdjacency(edges: Iterable<DirectedEdge>): Map<string, Set<string>> {
  const adjacency = new Map<string, Set<string>>();
  for (const { from, to } of edges) addNeighbor(adjacency, from, to);
  return adjacency;
}

function canReach(
  adjacency: ReadonlyMap<string, ReadonlySet<string>>,
  start: string,
  target: string,
): boolean {
  const pending = [start];
  const visited = new Set<string>();

  while (pending.length > 0) {
    const current = pending.pop()!;
    if (current === target) return true;
    if (visited.has(current)) continue;
    visited.add(current);

    for (const neighbor of adjacency.get(current) ?? []) {
      if (!visited.has(neighbor)) pending.push(neighbor);
    }
  }

  return false;
}

/** Whether adding `from -> to` would introduce (or close) a directed cycle. */
export function wouldCreateDirectedCycle(
  edges: Iterable<DirectedEdge>,
  from: string,
  to: string,
): boolean {
  if (from === to) return true;
  return canReach(directedAdjacency(edges), to, from);
}

/**
 * Return the complete duplicate cluster around `start`.
 *
 * Duplicate links are directed toward the canonical issue, but a cluster is
 * intentionally traversed in both directions so it includes the canonical
 * issue, direct duplicates, and duplicates-of-duplicates.
 */
export function weaklyConnectedComponent(
  edges: Iterable<DirectedEdge>,
  start: string,
): string[] {
  const adjacency = new Map<string, Set<string>>();
  for (const { from, to } of edges) {
    addNeighbor(adjacency, from, to);
    addNeighbor(adjacency, to, from);
  }

  const pending = [start];
  const visited = new Set<string>();
  while (pending.length > 0) {
    const current = pending.pop()!;
    if (visited.has(current)) continue;
    visited.add(current);
    for (const neighbor of adjacency.get(current) ?? []) {
      if (!visited.has(neighbor)) pending.push(neighbor);
    }
  }

  return [...visited].sort();
}
