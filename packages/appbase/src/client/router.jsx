import React, { useState, useEffect, Suspense, Component } from 'react'
export { matchRoute } from './match.js'
import { matchRoute } from './match.js'

// --- Error Boundary ---
export class ErrorBoundary extends Component {
  constructor(props) {
    super(props)
    this.state = { error: null }
  }
  static getDerivedStateFromError(error) {
    return { error }
  }
  render() {
    if (this.state.error) {
      const Fallback = this.props.fallback
      if (Fallback) {
        return <Fallback
          error={this.state.error}
          reset={() => this.setState({ error: null })}
        />
      }
      return <div>Error: {this.state.error.message}</div>
    }
    return this.props.children
  }
}

// --- Router Component ---
export function Router({ routes }) {
  const [pathname, setPathname] = useState(
    typeof window !== 'undefined' ? window.location.pathname : '/'
  )

  useEffect(() => {
    const onPopState = () => setPathname(window.location.pathname)
    window.addEventListener('popstate', onPopState)
    return () => window.removeEventListener('popstate', onPopState)
  }, [])

  useEffect(() => {
    window.__navigate = (to) => {
      window.history.pushState({}, '', to)
      setPathname(to)
    }
  }, [])

  // Find matching route (most specific first — longer patterns match first)
  const sorted = [...routes].sort((a, b) => b.path.length - a.path.length)
  let matched = null
  let params = {}
  for (const route of sorted) {
    const result = matchRoute(route.path, pathname)
    if (result !== null) {
      matched = route
      params = result
      break
    }
  }

  if (!matched) {
    const root = routes.find(r => r.path === '/')
    if (root && root.notFound) {
      const NotFound = root.notFound
      return <NotFound />
    }
    return <div>404 Not Found</div>
  }

  // Build component tree: page -> error boundary -> suspense -> template -> layouts
  const Page = matched.component
  let element = <Page params={params} />

  if (matched.error) {
    element = <ErrorBoundary fallback={matched.error}>{element}</ErrorBoundary>
  }

  if (matched.loading) {
    element = <Suspense fallback={<matched.loading />}>{element}</Suspense>
  }

  if (matched.template) {
    const Template = matched.template
    element = <Template key={pathname}>{element}</Template>
  }

  // Wrap with layout chain (innermost first, so reverse)
  const layouts = matched.layouts || []
  for (let i = layouts.length - 1; i >= 0; i--) {
    const Layout = layouts[i]
    element = <Layout>{element}</Layout>
  }

  return element
}

// --- Link Component ---
export function Link({ to, children, ...props }) {
  const handleClick = (e) => {
    e.preventDefault()
    if (window.__navigate) window.__navigate(to)
  }
  return <a href={to} onClick={handleClick} {...props}>{children}</a>
}
