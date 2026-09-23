export const testOrigin = `http://127.0.0.1:${process.env.GATHER_TEST_PORT ?? 5198}`;
export const backofficeOrigin = `http://localhost:${process.env.GATHER_TEST_BACKOFFICE_PORT ?? 5200}`;
// The fixture binds this only once both apps answer a procedure, so it is what
// Playwright waits for: either app's own port opens with Vite, well before the
// runtime behind it can serve a spec.
export const readyOrigin = `http://127.0.0.1:${process.env.GATHER_TEST_READY_PORT ?? 5202}`;
