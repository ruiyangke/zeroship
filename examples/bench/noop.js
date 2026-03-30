// Pure JS functions — no db, no I/O
async function echo(text) { return { text } }
async function add(a, b) { return { result: a + b } }
async function fib(n) {
  if (n <= 1) return { result: n }
  let a = 0, b = 1
  for (let i = 2; i <= n; i++) { [a, b] = [b, a + b] }
  return { result: b }
}

globalThis.__rpc = { echo, add, fib }
