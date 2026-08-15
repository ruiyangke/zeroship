import ts from "typescript";

/** Return the initializer carried by a declaration, when it has one. */
function declarationInitializer(declaration) {
  if (
    ts.isVariableDeclaration(declaration) ||
    ts.isParameter(declaration) ||
    ts.isPropertyDeclaration(declaration) ||
    ts.isPropertyAssignment(declaration) ||
    ts.isBindingElement(declaration) ||
    ts.isEnumMember(declaration)
  ) {
    return declaration.initializer;
  }
  return undefined;
}

/** Follow a symbol to the initializer nodes that define its value. */
function symbolInitializers(original, checker, seenSymbols) {
  let symbol = original;
  if (symbol.flags & ts.SymbolFlags.Alias) {
    symbol = checker.getAliasedSymbol(symbol);
  }
  if (seenSymbols.has(symbol)) return [];
  seenSymbols.add(symbol);

  const initializers = [];
  const declarations = new Set(symbol.declarations ?? []);
  if (symbol.valueDeclaration) declarations.add(symbol.valueDeclaration);
  for (const declaration of declarations) {
    const initializer = declarationInitializer(declaration);
    if (initializer) initializers.push(initializer);
  }
  return initializers;
}

/**
 * Find the string and template literals that contribute to a className.
 *
 * JSX commonly points className at a local assembled with clsx/classnames.
 * Follow those identifiers through their declarations, including chains such
 * as `const b = cx(a, extra)`, while leaving unresolved caller-provided values
 * alone. `seenSymbols` and `seenNodes` make malformed/self-referential chains
 * harmless.
 */
export function classNameLiterals(initializer, checker) {
  const literals = [];
  const seenNodes = new Set();
  const seenSymbols = new Set();

  const visitSymbol = (original) => {
    const initializers = symbolInitializers(original, checker, seenSymbols);
    for (const next of initializers) visit(next);
    return initializers.length > 0;
  };

  const visit = (node) => {
    if (!node || seenNodes.has(node)) return;
    seenNodes.add(node);

    if (ts.isJsxExpression(node)) {
      visit(node.expression);
      return;
    }
    if (
      ts.isStringLiteral(node) ||
      ts.isNoSubstitutionTemplateLiteral(node)
    ) {
      literals.push(node);
      return;
    }
    if (ts.isTemplateExpression(node)) {
      literals.push(node);
      for (const span of node.templateSpans) visit(span.expression);
      return;
    }
    if (ts.isIdentifier(node)) {
      const symbol = checker.getSymbolAtLocation(node);
      if (symbol) visitSymbol(symbol);
      return;
    }
    if (ts.isPropertyAccessExpression(node)) {
      const symbol = checker.getSymbolAtLocation(node.name);
      if (!symbol || !visitSymbol(symbol)) visit(node.expression);
      return;
    }
    if (ts.isCallExpression(node) || ts.isNewExpression(node)) {
      for (const argument of node.arguments ?? []) visit(argument);
      return;
    }

    ts.forEachChild(node, visit);
  };

  visit(initializer);
  return literals;
}

/**
 * Resolve an expression such as `{...dataProps}` to its object properties.
 * This is deliberately limited to value initializers and object spreads; it
 * never expands a function body or a caller-provided object with no local
 * initializer.
 */
export function objectProperties(initializer, checker) {
  const properties = [];
  const seenNodes = new Set();
  const seenSymbols = new Set();

  const visit = (node) => {
    if (!node || seenNodes.has(node)) return;
    seenNodes.add(node);

    if (ts.isIdentifier(node)) {
      const symbol = checker.getSymbolAtLocation(node);
      if (!symbol) return;
      for (const next of symbolInitializers(symbol, checker, seenSymbols)) {
        visit(next);
      }
      return;
    }
    if (ts.isObjectLiteralExpression(node)) {
      for (const property of node.properties) {
        if (ts.isSpreadAssignment(property)) visit(property.expression);
        else if (ts.isPropertyAssignment(property)) properties.push(property);
      }
      return;
    }
    if (
      ts.isParenthesizedExpression(node) ||
      ts.isAsExpression(node) ||
      ts.isTypeAssertionExpression(node) ||
      ts.isNonNullExpression(node) ||
      ts.isSatisfiesExpression(node)
    ) {
      visit(node.expression);
      return;
    }
    if (ts.isConditionalExpression(node)) {
      visit(node.whenTrue);
      visit(node.whenFalse);
    }
  };

  visit(initializer);
  return properties;
}
