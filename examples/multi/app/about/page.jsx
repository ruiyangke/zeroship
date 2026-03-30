export default function About() {
  return (
    <div>
      <h1 style={{ fontSize: '24px', fontWeight: 300, marginBottom: '16px' }}>About</h1>
      <p style={{ color: '#888', lineHeight: 1.6 }}>
        Appbase compiles single-file apps into server + client bundles.
        Server functions run in a V8 isolate with Rust-native SQLite.
        The protocol is JSON-RPC 2.0.
      </p>
    </div>
  )
}
