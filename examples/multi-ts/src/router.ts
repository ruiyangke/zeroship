// Simple URL router

import { errorResponse } from "./utils";

export interface Route {
    path: string;
    handler: (request: Request) => Response;
}

export function createRouter(routes: Route[]): (request: Request) => Response {
    const routeMap = new Map<string, Route["handler"]>();
    for (const route of routes) {
        routeMap.set(route.path, route.handler);
    }

    return (request: Request): Response => {
        const url = new URL(request.url);
        const handler = routeMap.get(url.pathname);
        if (handler) {
            return handler(request);
        }
        return errorResponse("Not Found", 404);
    };
}
