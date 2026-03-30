export default function RootLayout({ children }) {
  return (
    <div style={styles.shell}>
      <nav style={styles.nav}>
        <span style={styles.logo}>&#9671; appbase</span>
        <div style={styles.links}>
          <a href="/" style={styles.link}>Home</a>
          <a href="/about" style={styles.link}>About</a>
          <a href="/todos" style={styles.link}>Todos</a>
        </div>
      </nav>
      <main style={styles.main}>
        {children}
      </main>
    </div>
  )
}

const styles = {
  shell: {
    minHeight: '100vh',
    background: 'linear-gradient(135deg, #0f0f0f 0%, #1a1a2e 50%, #16213e 100%)',
    fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
    color: '#e0e0e0',
  },
  nav: {
    display: 'flex',
    alignItems: 'center',
    justifyContent: 'space-between',
    padding: '16px 32px',
    borderBottom: '1px solid #1e1e2e',
  },
  logo: {
    fontSize: '16px',
    color: '#646cff',
    fontWeight: 600,
    letterSpacing: '0.08em',
  },
  links: {
    display: 'flex',
    gap: '24px',
  },
  link: {
    color: '#888',
    textDecoration: 'none',
    fontSize: '14px',
  },
  main: {
    maxWidth: '640px',
    margin: '0 auto',
    padding: '40px 20px',
  },
}
