// Cluster wrapper around node_whatwg_server.js — same shape as
// node_server_cluster.js but loads the WHATWG fixture in workers.

import cluster from 'node:cluster';
import os from 'node:os';

const PORT = parseInt(process.argv[2] || '4005');
const NUM_WORKERS = parseInt(process.argv[3] || os.cpus().length.toString());

if (cluster.isPrimary) {
    console.log(`[node-whatwg-cluster] ${NUM_WORKERS} workers on port ${PORT} (Node.js ${process.version})`);
    for (let i = 0; i < NUM_WORKERS; i++) cluster.fork();
    cluster.on('exit', () => cluster.fork());
} else {
    await import('./node_whatwg_server.js');
}
