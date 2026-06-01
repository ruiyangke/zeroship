// ─── NotFound — 404 route ────────────────────────────────────────
//
// Crystal: built over @zeroship/ui. A full-viewport Center holds a
// DS EmptyState whose compound parts read eyebrow → title → muted
// description → a "back to home" action. The action routes a
// react-router Link through a plain Button (asChild) so navigation and
// the data-testid hooks are preserved. Bespoke eyebrow + accented
// title word live in the co-located NotFound.css reading --zs-*.

import { Link } from "react-router-dom";
import { Button, Center, EmptyState } from "@zeroship/ui";
import "./NotFound.css";

export function NotFound() {
  return (
    <Center
      asChild
      minHeight="100dvh"
      className="nf-center"
    >
      <main data-testid="not-found-page">
        <EmptyState className="nf-empty">
          <p className="nf-eyebrow">Not found</p>
          <EmptyState.Title className="nf-title">
            This page does not <em className="nf-title__accent">exist</em>.
          </EmptyState.Title>
          <EmptyState.Description>
            The route may be stale, or the project may have moved.
          </EmptyState.Description>
          <EmptyState.Actions>
            <Button variant="plain" asChild className="nf-home">
              <Link to="/home" data-testid="not-found-home">
                Back to home
              </Link>
            </Button>
          </EmptyState.Actions>
        </EmptyState>
      </main>
    </Center>
  );
}
