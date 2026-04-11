// Node.js WebSocket echo server for baseline comparison
import { WebSocketServer } from 'ws';

const PORT = parseInt(process.argv[2] || '4010');

const wss = new WebSocketServer({ port: PORT });

wss.on('connection', (ws) => {
    ws.on('message', (data) => {
        ws.send(data);
    });
});

wss.on('listening', () => {
    console.log(`[node-ws] ws://0.0.0.0:${PORT} (Node.js ${process.version})`);
});
