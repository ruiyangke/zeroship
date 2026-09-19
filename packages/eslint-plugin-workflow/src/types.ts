export interface RuleModule {
  meta: {
    type: "problem" | "suggestion" | "layout";
    docs: {
      description: string;
      recommended?: boolean;
      url?: string;
    };
    schema: unknown[];
    messages: Record<string, string>;
  };
  create(context: RuleContext): Visitor;
}

export interface RuleContext {
  report(descriptor: {
    node: AstNode;
    messageId: string;
    data?: Record<string, string>;
  }): void;
}

export interface AstNode {
  type: string;
  parent?: AstNode;
  callee?: AstNode;
  arguments?: AstNode[];
  object?: AstNode;
  property?: AstNode;
  name?: string;
  value?: unknown;
  computed?: boolean;
  optional?: boolean;
  key?: AstNode;
  expression?: AstNode;
  expressions?: AstNode[];
  quasis?: AstNode[];
  elements?: Array<AstNode | null>;
  properties?: AstNode[];
  body?: AstNode | AstNode[];
  declarations?: AstNode[];
  init?: AstNode | null;
  id?: AstNode | null;
  params?: AstNode[];
  left?: AstNode;
  right?: AstNode;
  argument?: AstNode | null;
  test?: AstNode | null;
  consequent?: AstNode;
  alternate?: AstNode | null;
  operator?: string;
  block?: AstNode;
  handler?: AstNode | null;
  finalizer?: AstNode | null;
  param?: AstNode | null;
}

export interface Visitor {
  Program?(node: AstNode): void;
  CallExpression?(node: AstNode): void;
  NewExpression?(node: AstNode): void;
}
