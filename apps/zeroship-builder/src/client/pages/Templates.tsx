// ─── Templates — public gallery (`/templates`) ──────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.4. Filter chips by category + grid of TemplateCards.
// Public route — uses PublicNav (not TopBar/PageFrame which expect
// an authed user). Click any card → /new?template=<slug>; the
// wizard reads the param and pre-fills the brief.
//
// Crystal skin: a DS Container (rendered as <main>) holds an intro
// section, a single-selection Toggle.Group for the category filter, a
// fluid Grid of (already-crystal) TemplateCards, and a footer. Bespoke
// chrome — the page canvas, the display heading, the eyebrow rule, the
// "describe your own" link, and the footer caption type — lives in the
// co-located Templates.css, all --zs-* tokens. Props, exports, the
// useState filter hook, routing, and every data-testid are preserved
// exactly; only the presentation changed.

import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Button,
  Container,
  EmptyState,
  Grid,
  Separator,
  Stack,
  Toggle,
  ToggleGroup,
} from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import { TemplateCard } from "../components/TemplateCard";
import { TEMPLATES, TEMPLATE_CATEGORIES, type TemplateCategory } from "../lib/templates";
import "./Templates.css";

type Filter = "all" | TemplateCategory;

const FOOTER_LINKS: { to: string; label: string }[] = [
  { to: "/", label: "Home" },
  { to: "/pricing", label: "Pricing" },
  { to: "/skills", label: "Skills" },
  { to: "/about", label: "About" },
];

export function Templates() {
  const [filter, setFilter] = useState<Filter>("all");
  const visible = filter === "all" ? TEMPLATES : TEMPLATES.filter((t) => t.category === filter);

  return (
    <div className="zs-templates-page" data-testid="templates-page">
      <PublicNav />

      <Container asChild size="lg" padX={6}>
        <main className="zs-templates__main">
          <Stack asChild gap={3} className="zs-templates__intro">
            <section>
              <div className="zs-templates__eyebrow">
                <span className="zs-templates__eyebrow-rule" aria-hidden="true" />
                Templates
              </div>
              <h1 className="zs-templates__title">
                Pick a <em className="zs-templates__title-accent">starting point</em>.
              </h1>
              <p className="zs-templates__lede">
                A dozen templates organised by what you're trying to do — share,
                collect, sell, show. Or describe your own.
              </p>
            </section>
          </Stack>

          <ToggleGroup
            value={filter}
            onValueChange={(next) => setFilter((next ?? "all") as Filter)}
            equalWidth={false}
            aria-label="Filter templates by category"
            className="zs-templates__filters"
            data-testid="templates-filters"
          >
            {TEMPLATE_CATEGORIES.map(({ key, label }) => (
              <Toggle key={key} value={key} data-testid={`filter:${key}`}>
                {label}
              </Toggle>
            ))}
          </ToggleGroup>

          {visible.length === 0 ? (
            <EmptyState
              data-testid="templates-empty"
              className="zs-templates__empty"
              title={
                <span className="zs-templates__empty-title">
                  Nothing in <em>{filter}</em> yet — that shelf is still being stocked.
                </span>
              }
              action={
                <Button
                  type="button"
                  variant="plain"
                  onClick={() => setFilter("all")}
                >
                  ← Show all templates
                </Button>
              }
            />
          ) : (
            <Grid
              minColWidth="16rem"
              gap={5}
              className="zs-templates__grid"
              data-testid="templates-grid"
            >
              {visible.map((t) => (
                <TemplateCard key={t.slug} template={t} />
              ))}
            </Grid>
          )}

          <div className="zs-templates__blank">
            <Link
              to="/new"
              className="zs-templates__blank-link"
              data-testid="templates-blank"
            >
              Or describe your own →
            </Link>
          </div>

          <Separator className="zs-templates__rule" />

          <footer className="zs-templates__footer">
            {FOOTER_LINKS.map((l) => (
              <Link key={l.to} to={l.to} className="zs-templates__footer-link">
                {l.label}
              </Link>
            ))}
            <span className="zs-templates__footer-mark">
              zeroship<span className="zs-templates__footer-dot">.</span> &copy; 2026
            </span>
          </footer>
        </main>
      </Container>
    </div>
  );
}
