// Creator envelopes still pass the normal reserved-name validator. This helper
// binds only the code-owned journal artifact after compilation. It also names
// compiler-generated constraints and indexes, and preserves string literals.
export function bindOwnedNames(sql, { identifiers, columns }) {
  const owned = new Set(identifiers);
  const tokens = [...sql.matchAll(/'(?:[^']|'')*'|"(?:[^"]|"")*"|[a-zA-Z_][a-zA-Z_0-9]*/g)];
  const nameOf = token => token.startsWith('"') ? token.slice(1, -1).replaceAll('""', '"') : token;
  for (let i = 0; i < tokens.length; i++) {
    let next;
    if (tokens[i][0] === 'CONSTRAINT') {
      next = i + 1;
    } else if (tokens[i][0] === 'CREATE') {
      let j = i + 1;
      if (tokens[j]?.[0] === 'UNIQUE') j++;
      if (tokens[j]?.[0] !== 'INDEX') continue;
      j++;
      if (tokens[j]?.[0] === 'IF' && tokens[j + 1]?.[0] === 'NOT' && tokens[j + 2]?.[0] === 'EXISTS') j += 3;
      next = j;
    } else {
      continue;
    }
    if (!tokens[next]) throw new Error('missing workflow compiler object name');
    owned.add(nameOf(tokens[next][0]));
  }
  for (const name of owned) {
    if (!/^[a-z][a-z_]*$/.test(name)) throw new Error(`invalid workflow owned name: ${name}`);
    if (columns.has(name)) throw new Error(`workflow owned name collides with a column: ${name}`);
    if (`__zeroship_workflow_${name}`.length > 63) throw new Error(`workflow identifier exceeds PostgreSQL limit: ${name}`);
  }
  const seen = new Set();
  let offset = 0;
  const output = [];
  for (const token of tokens) {
    const raw = token[0];
    if (raw.startsWith("'")) continue;
    const name = nameOf(raw);
    if (!owned.has(name)) continue;
    seen.add(name);
    output.push(sql.slice(offset, token.index), `"__zeroship_workflow_${name}"`);
    offset = token.index + raw.length;
  }
  output.push(sql.slice(offset));
  for (const name of owned) {
    if (!seen.has(name)) throw new Error(`workflow compiler omitted owned identifier: ${name}`);
  }
  if (!seen.size) throw new Error('workflow schema has no owned identifiers');
  return output.join('');
}
