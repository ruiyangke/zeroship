import {
  createRpcClient,
  defineRpcProcedures,
  type InferRpcContract,
  type Mutation,
  type Query,
  type Stream,
  type Subscription,
} from "../src/index.js";
import {
  mutation as serverMutation,
  procedure as serverProcedure,
  query as serverQuery,
} from "../src/server.js";
import type { RpcClientProcedure } from "../src/types.js";

type AssertNever<T extends never> = T;

type Todo = {
  id: string;
  text: string;
};

type ChatChunk = {
  text: string;
};

type AppRpc = {
  "todos.list": Query<{ limit?: number }, Todo[]>;
  "todos.create": Mutation<{ text: string }, Todo, { idempotent: true }>;
  "chat.ask": Stream<{ prompt: string }, ChatChunk>;
  "feed.events": Subscription<void, { id: string }>;
};

type SubscriptionIsNotGenericClientProcedure = AssertNever<
  RpcClientProcedure<Subscription<void, { id: string }>>
>;

const rpc = createRpcClient<AppRpc>();
const manualRpc = createRpcClient();

const manualList = manualRpc.query<{ limit?: number }, Todo[]>("todos.list");
const manualListed: Promise<Todo[]> = manualList({ limit: 10 });

const manualAsk = manualRpc.stream<{ prompt: string }, ChatChunk>("chat.ask");
const manualChunks: AsyncIterableIterator<ChatChunk> = manualAsk({ prompt: "status?" });

// @ts-expect-error manual unary procedures do not return async iterators
const invalidManualList: AsyncIterableIterator<Todo> = manualList({ limit: 10 });

const listTodos = rpc.query("todos.list");
const listed: Promise<Todo[]> = listTodos({ limit: 10 });

// @ts-expect-error unknown procedure ids are rejected by typed clients
rpc.query("todos.missing");

// @ts-expect-error read procedures cannot request write idempotency
rpc.query("todos.list", { idempotent: true });

const createTodo = rpc.mutation("todos.create", { idempotent: true });
const created: Promise<Todo> = createTodo({ text: "ship it" });

// @ts-expect-error input remains required for non-void procedures
createTodo();

const ask = rpc.stream("chat.ask");
const chunks: AsyncIterableIterator<ChatChunk> = ask({ prompt: "status?" });

const procedures = defineRpcProcedures<AppRpc>()({
  "todos.list": { kind: "query" },
  "todos.create": { kind: "mutation", idempotent: true },
  "chat.ask": { kind: "stream" },
  "feed.events": { kind: "subscription" },
});

const registered = createRpcClient<AppRpc>({ procedures });
const registeredList: Promise<Todo[]> = registered.call("todos.list", { limit: 5 });
const registeredChunks: AsyncIterableIterator<ChatChunk> = registered.call("chat.ask", {
  prompt: "status?",
});

// @ts-expect-error subscriptions are not callable through the generic client yet
registered.call("feed.events");

const backendProcedures = {
  listTodos: serverQuery(async (_input: { limit?: number }): Promise<Todo[]> => [], {
    id: "todos.list",
  }),
  implicitList: serverQuery(async (): Promise<Todo[]> => []),
  createTodo: serverMutation(
    async (_input: { text: string }): Promise<Todo> => ({ id: "1", text: "ship it" }),
    { id: "todos.create", idempotent: true },
  ),
  explicitGeneric: serverProcedure(
    async (_input: { limit?: number }): Promise<Todo[]> => [],
    { id: "todos.explicit", kind: "query" },
  ),
  privateHelper: async () => "not rpc",
};

type InferredRpc = InferRpcContract<typeof backendProcedures>;
const inferredRpc = createRpcClient<InferredRpc>();
const inferredListed: Promise<Todo[]> = inferredRpc.query("todos.list")({ limit: 10 });
const inferredCreated: Promise<Todo> = inferredRpc
  .mutation("todos.create", { idempotent: true })({ text: "ship it" });
const inferredGeneric: Promise<Todo[]> = inferredRpc
  .query("todos.explicit")({ limit: 10 });

// @ts-expect-error explicit config.id is the client id, not the export name
inferredRpc.query("listTodos");

// @ts-expect-error inferred contracts require explicit literal config.id values
inferredRpc.query("implicitList");

// @ts-expect-error non-wrapper exports are not inferred as RPC procedures
inferredRpc.query("privateHelper");

void listed;
void manualListed;
void manualChunks;
void invalidManualList;
void created;
void chunks;
void registeredList;
void registeredChunks;
void inferredListed;
void inferredCreated;
void inferredGeneric;
