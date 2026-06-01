// ─── Skills — public skill catalogue (`/skills`) ────────────────
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §5.3. Filter pills at the top, grid of skill cards
// underneath. "Add to project" is greyed out / "Coming soon" — the
// install action and skill registry are not wired yet.
//
// Crystal skin: PublicNav band, then a Container holding a PageHeader
// lede (eyebrow → display title → description), a ToggleGroup filter
// row (replacing the editorial FilterPill), and a fluid Grid of DS
// Cards (replacing the bespoke <article> cards). The empty-state uses
// the EmptyState block; the registry note is a tinted Card. Bespoke
// chrome (the eyebrow rule, the display-scale title, the card
// number/icon/footer treatment, the footer link row) lives in the
// co-located Skills.css reading only --zs-* tokens. Props, state,
// routing, and every data-testid are preserved exactly — only the
// presentation changed.

import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Button,
  Card,
  Cluster,
  Container,
  EmptyState,
  Grid,
  PageHeader,
  Separator,
  Stack,
  ToggleGroup,
  Toggle,
} from "@zeroship/ui";
import { PublicNav } from "../components/PublicNav";
import { SKILLS, SKILL_CATEGORIES, type SkillCategory, type Skill } from "../lib/skills";
import "./Skills.css";

export function Skills() {
  const [filter, setFilter] = useState<"all" | SkillCategory>("all");
  const visible = filter === "all" ? SKILLS : SKILLS.filter((s) => s.category === filter);

  return (
    <div className="zs-skills" data-testid="skills-page">
      <PublicNav />

      <Container asChild size="md" padX={6}>
        <main className="zs-skills__main">
          {/* ─── Lede ─────────────────────────────────────────────── */}
          <PageHeader className="zs-skills__lede">
            <PageHeader.Text>
              <span className="zs-skills__eyebrow">
                <span className="zs-skills__eyebrow-rule" aria-hidden="true" />
                Skills catalogue
              </span>
              <PageHeader.Title className="zs-skills__title">
                What your app can <em className="zs-skills__title-em">do</em>.
              </PageHeader.Title>
              <PageHeader.Description className="zs-skills__lede-copy">
                Skills are the building blocks: auth, payments, search, AI,
                realtime. Add them to a project and the agents wire them in for
                you. No SDKs to read, no API keys to copy.
              </PageHeader.Description>
            </PageHeader.Text>
          </PageHeader>

          {/* ─── Filter pills ─────────────────────────────────────── */}
          <ToggleGroup
            className="zs-skills__filters"
            value={filter}
            onValueChange={(next) => setFilter((next ?? "all") as "all" | SkillCategory)}
            variant="plain"
            equalWidth={false}
            aria-label="Filter skills by category"
            data-testid="skills-filters"
          >
            {SKILL_CATEGORIES.map(({ key, label }) => (
              <Toggle
                key={key}
                value={key}
                className="zs-skills__filter"
                data-testid={`skills-filter:${key}`}
              >
                {label}
              </Toggle>
            ))}
          </ToggleGroup>

          {/* ─── Skill cards ──────────────────────────────────────── */}
          {visible.length === 0 ? (
            <EmptyState
              className="zs-skills__empty"
              data-testid="skills-empty"
              title={
                <>
                  No skills in <em>{filter}</em> yet — the catalogue&rsquo;s still wiring up.
                </>
              }
              action={
                <Button variant="plain" onClick={() => setFilter("all")}>
                  ← Show all skills
                </Button>
              }
            />
          ) : (
            <Grid
              className="zs-skills__grid"
              minColWidth="16.25rem"
              gap={5}
              data-testid="skills-grid"
            >
              {visible.map((s) => <SkillCard key={s.slug} skill={s} />)}
            </Grid>
          )}

          {/* ─── Note about the registry ──────────────────────────── */}
          <Card variant="outline" className="zs-skills__note">
            <Card.Header>
              <Card.Title asChild className="zs-skills__note-title">
                <h2>A note</h2>
              </Card.Title>
            </Card.Header>
            <Card.Content className="zs-skills__note-copy">
              The skill registry is wiring up. For now this page lists what&rsquo;s
              on the roadmap; &ldquo;Add to project&rdquo; lights up once the install
              action ships.
            </Card.Content>
          </Card>

          <Separator className="zs-skills__rule" />

          <footer className="zs-skills__footer">
            <Cluster gap={7} className="zs-skills__footer-links">
              <Link to="/" className="zs-skills__footer-link">Home</Link>
              <Link to="/pricing" className="zs-skills__footer-link">Pricing</Link>
              <Link to="/templates" className="zs-skills__footer-link">Templates</Link>
              <Link to="/about" className="zs-skills__footer-link">About</Link>
              <span className="zs-skills__footer-mark">
                zeroship<span className="zs-skills__footer-dot">.</span> &copy; 2026
              </span>
            </Cluster>
          </footer>
        </main>
      </Container>
    </div>
  );
}

function SkillCard({ skill }: { skill: Skill }) {
  return (
    <Card
      variant="outline"
      className="zs-skill-card"
      data-testid={`skill-card:${skill.slug}`}
    >
      <Stack gap={2}>
        <span className="zs-skill-card__num">
          № {skill.num} · <span className="zs-skill-card__cat">{skill.category}</span>
        </span>
        <Cluster gap={3} align="center">
          <span aria-hidden="true" className="zs-skill-card__icon">
            {skill.icon}
          </span>
          <Card.Title asChild className="zs-skill-card__name">
            <h3>{skill.name}</h3>
          </Card.Title>
        </Cluster>
        <p className="zs-skill-card__tagline">{skill.tagline}</p>
      </Stack>
      <Card.Footer divider="top" align="between" className="zs-skill-card__footer">
        <span className="zs-skill-card__soon">Coming soon</span>
        <button
          type="button"
          disabled
          className="zs-skill-card__add"
          data-testid={`skill-add:${skill.slug}`}
        >
          Add to project →
        </button>
      </Card.Footer>
    </Card>
  );
}
