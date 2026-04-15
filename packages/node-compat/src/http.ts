// node:http stub — just constants and types. No actual HTTP server/client.

export const METHODS = ["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "CONNECT", "TRACE"];
export const STATUS_CODES: Record<number, string> = {
  100: "Continue", 101: "Switching Protocols", 200: "OK", 201: "Created",
  204: "No Content", 301: "Moved Permanently", 302: "Found", 304: "Not Modified",
  400: "Bad Request", 401: "Unauthorized", 403: "Forbidden", 404: "Not Found",
  405: "Method Not Allowed", 408: "Request Timeout", 409: "Conflict",
  413: "Payload Too Large", 415: "Unsupported Media Type", 429: "Too Many Requests",
  500: "Internal Server Error", 501: "Not Implemented", 502: "Bad Gateway",
  503: "Service Unavailable", 504: "Gateway Timeout",
};
export class Agent {}
export class IncomingMessage {}
export class ServerResponse {}
export class Server {}

function notImpl(name: string): (...args: any[]) => never {
  return () => { throw new Error(`http.${name} is not implemented in zeroship runtime`); };
}

export const createServer = notImpl("createServer");
export const request = notImpl("request");
export const get = notImpl("get");
export const globalAgent = new Agent();

export default { METHODS, STATUS_CODES, Agent, IncomingMessage, ServerResponse, Server, createServer, request, get, globalAgent };
