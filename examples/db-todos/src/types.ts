import type {
  Priority,
  Todo as ServerTodo,
  TodoSnapshot as ServerTodoSnapshot,
  User as ServerUser,
} from "./index";

type ClientShape<T> =
  T extends string & { readonly __zeroshipTable: string } ? string :
  T extends string ? string extends T ? string : T :
  T extends number | boolean | null | undefined ? T :
  T extends readonly (infer U)[] ? ClientShape<U>[] :
  T extends object ? { [K in keyof T]: ClientShape<T[K]> } :
  T;

export type Todo = ClientShape<ServerTodo> & {
  /** Client-only: stable render key bridging an optimistic row to its real id. */
  _key?: string;
};

export type User = ClientShape<ServerUser>;
export type { Priority };
export type TodoSnapshot = ClientShape<ServerTodoSnapshot>;
