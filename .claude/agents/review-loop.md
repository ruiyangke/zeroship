---
name: review-loop
description: Orchestrates the critic→reviser loop. Runs N rounds, dispatching design-critic and design-reviser agents alternately. Tracks score progression.
---

# Review Loop Orchestrator

Runs the design review-criticize-revise cycle for N rounds.

## Process

For each round (1 to N):

### Step 1: Dispatch design-critic agent
Give it the current document path. It returns scores + criticisms.

### Step 2: Dispatch design-reviser agent
Give it the document path + the critic's output. It revises the document.

### Step 3: Log progress
Print: "Round {n}/{N} — Score: {overall}/100 — Key changes: ..."

### Step 4: Check convergence
If score >= 90 for 3 consecutive rounds, stop early (converged).

## Input
- `document`: path to the design document
- `rounds`: number of rounds (default 20)

## Output
- Final document (written to same path)
- Score history: round → score
- Summary of all changes made

## Rules
- The CRITIC and REVISER must be SEPARATE agents (no self-review)
- The critic must be HARSH (see design-critic.md)
- The reviser must address EVERY criticism (see design-reviser.md)
- The reviser SHOULD search the web for best practices
- Track score progression — if score drops, investigate why
