#!/usr/bin/env node
// WebSocket benchmark: measures messages/sec round-trip (echo)
// Usage: node ws_benchmark.js [url] [--clients=N] [--duration=N] [--payload=N]

import { WebSocket } from 'ws';

const args = process.argv.slice(2);
const url = args.find(a => !a.startsWith('--')) || 'ws://localhost:5100';
const clients = parseInt(args.find(a => a.startsWith('--clients='))?.split('=')[1] || '50');
const duration = parseInt(args.find(a => a.startsWith('--duration='))?.split('=')[1] || '10');
const payloadSize = parseInt(args.find(a => a.startsWith('--payload='))?.split('=')[1] || '64');

const payload = 'x'.repeat(payloadSize);

let totalMessages = 0;
let totalLatency = 0;
let latencies = [];
let connected = 0;
let errors = 0;
let done = false;

function startClient() {
    return new Promise((resolve) => {
        const ws = new WebSocket(url);
        let pending = null;

        ws.on('open', () => {
            connected++;
            sendNext();
        });

        ws.on('message', (data) => {
            if (done) { ws.close(); return; }
            const now = process.hrtime.bigint();
            if (pending) {
                const latUs = Number(now - pending) / 1000;
                totalLatency += latUs;
                latencies.push(latUs);
            }
            totalMessages++;
            sendNext();
        });

        ws.on('error', () => { errors++; });
        ws.on('close', () => { resolve(); });

        function sendNext() {
            if (done) { ws.close(); return; }
            pending = process.hrtime.bigint();
            ws.send(payload);
        }
    });
}

console.log(`WebSocket Benchmark`);
console.log(`  URL:      ${url}`);
console.log(`  Clients:  ${clients}`);
console.log(`  Duration: ${duration}s`);
console.log(`  Payload:  ${payloadSize} bytes`);
console.log('');

// Launch all clients
const promises = [];
for (let i = 0; i < clients; i++) {
    promises.push(startClient());
}

// Wait for connections
await new Promise(r => setTimeout(r, 2000));
console.log(`Connected: ${connected}/${clients} (errors: ${errors})`);

// Reset counters after warmup
totalMessages = 0;
totalLatency = 0;
latencies = [];
const startTime = Date.now();

// Run for duration
await new Promise(r => setTimeout(r, duration * 1000));
done = true;

const elapsed = (Date.now() - startTime) / 1000;

// Wait for all clients to close
await Promise.allSettled(promises);

// Stats
latencies.sort((a, b) => a - b);
const msgPerSec = Math.round(totalMessages / elapsed);
const avgLatency = latencies.length ? (totalLatency / latencies.length / 1000).toFixed(2) : 'N/A';
const p50 = latencies.length ? (latencies[Math.floor(latencies.length * 0.50)] / 1000).toFixed(2) : 'N/A';
const p99 = latencies.length ? (latencies[Math.floor(latencies.length * 0.99)] / 1000).toFixed(2) : 'N/A';
const p999 = latencies.length ? (latencies[Math.floor(latencies.length * 0.999)] / 1000).toFixed(2) : 'N/A';

console.log('');
console.log('--- Results ---');
console.log(`  Messages/sec:  ${msgPerSec.toLocaleString()}`);
console.log(`  Total msgs:    ${totalMessages.toLocaleString()}`);
console.log(`  Avg latency:   ${avgLatency} ms`);
console.log(`  p50 latency:   ${p50} ms`);
console.log(`  p99 latency:   ${p99} ms`);
console.log(`  p999 latency:  ${p999} ms`);
console.log(`  Errors:        ${errors}`);
