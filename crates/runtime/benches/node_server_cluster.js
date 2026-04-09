import cluster from 'node:cluster';
import os from 'node:os';

const PORT = parseInt(process.argv[2] || '4002');
const NUM_WORKERS = parseInt(process.argv[3] || os.cpus().length.toString());

if (cluster.isPrimary) {
    console.log(`[node-cluster] ${NUM_WORKERS} workers on port ${PORT} (Node.js ${process.version})`);
    for (let i = 0; i < NUM_WORKERS; i++) cluster.fork();
    cluster.on('exit', () => cluster.fork());
} else {
    // Import and start the server (it reads PORT from argv)
    await import('./node_server.js');
}
