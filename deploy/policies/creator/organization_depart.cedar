// Band: any seated member (rank >= 10). The one authority a member holds over
// their OWN seat and over nothing else.
//
// WHY IT IS ITS OWN BAND RATHER THAN A LINE IN THE VIEWER BAND. Every other
// action in organization_read.cedar is a read; this one deletes a row. Putting
// a write in a file called "read" would make the file's name the least reliable
// thing about it, and the two are read together by anyone auditing what a
// viewer can do.
//
// WHY IT IS NOT `organization:members:write`. That action is banded at admin and
// above, because seating a member is authority OVER the organization. Leaving is
// the opposite direction, and a viewer - who never holds `members:write` at any
// rank - is exactly the member most likely to want out. Gating departure on
// `members:write` is why nobody could leave at all.
//
// WHY NOT `organization:read` EITHER, which would also reach a viewer: a client
// granted read-only consent would then be able to delete its holder's seat. The
// consent vocabulary is 1:1 with this vocabulary, so an action that deletes a
// row needs a scope a human can decline on its own.
//
// WHAT STOPS IT REACHING SOMEONE ELSE'S SEAT. Not this band. The route it gates,
// `DELETE /api/organizations/{organization_id}/membership`, names no user: the
// statement matches `user_id = <the authenticated principal>` and there is no
// path segment, body field or query parameter that could name another. The
// general rank inequality - `actor.rank > target.rank`, which is what stops an
// admin demoting a peer - is untouched and still governs every OTHER membership
// write.
//
// The last-owner rule still applies. A sole owner is refused and told to
// transfer ownership first, because an organization that keeps no owner is one
// no route can repair.

permit (
  principal is User,
  action in [Action::"organization:members:leave"],
  resource is Organization
) when { context.effective_rank >= 10 };
