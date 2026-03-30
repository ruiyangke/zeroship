import { describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { matchRoute } from '../packages/appbase/src/client/match.js'

describe('router: matchRoute', () => {
  it('matches exact paths', () => {
    assert.deepEqual(matchRoute('/', '/'), {})
    assert.deepEqual(matchRoute('/about', '/about'), {})
    assert.equal(matchRoute('/about', '/other'), null)
  })

  it('matches dynamic segments', () => {
    assert.deepEqual(matchRoute('/todos/:id', '/todos/abc'), { id: 'abc' })
    assert.deepEqual(
      matchRoute('/users/:userId/posts/:postId', '/users/1/posts/2'),
      { userId: '1', postId: '2' }
    )
    assert.equal(matchRoute('/todos/:id', '/todos'), null)
  })

  it('matches catch-all', () => {
    const result = matchRoute('/blog/*', '/blog/2024/hello-world')
    assert.ok(result)
    assert.deepEqual(result.slug, ['2024', 'hello-world'])
  })

  it('rejects mismatched paths', () => {
    assert.equal(matchRoute('/about', '/about/team'), null)
    assert.equal(matchRoute('/about/team', '/about'), null)
  })

  it('matches root path correctly', () => {
    assert.deepEqual(matchRoute('/', '/'), {})
    assert.equal(matchRoute('/', '/anything'), null)
  })
})
