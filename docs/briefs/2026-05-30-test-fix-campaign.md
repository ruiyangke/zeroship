# Test-fix campaign — drive the full @zeroship/ui suite to ZERO failures

User directive (offline pilot): "fix all tests, make sure no failed tests."
These are PRE-EXISTING failures in the original Base-UI components (NOT from the
icon/section work, which is committed + verified 0-regression). Base UI =
`@base-ui/react@1.5.0`. Run cluster fix-agents SEQUENTIALLY (each owns build+test,
must verify its suites GREEN before reporting) to avoid the shared storybook-static
build race. Commit-only, never push.

## Systemic harness fix (applied, uncommitted — commit with round 1)
`.storybook/test-runner.ts` preVisit now `page.emulateMedia({ reducedMotion:
"reduce" })` — overlay entrance transitions are instant, killing the synchronous
`toBeVisible()` vs entrance-transition race. Dropped full-suite fails ~85 → 73.

## Baseline: 73 failed / 611 (24 suites) WITH reduced-motion. Target: 0.

## Clusters (root cause → fix approach)
1. **Hover overlays don't open before sync query** — PreviewCard (9), Tooltip (7).
   getBy* before the hover open-delay / portal-scoped query. Fix: findBy/waitFor +
   portal scope. → AGENT 1 (a73a4135e111149a5) RUNNING.
2. **`toBeDisabled()` on aria-disabled elements** — Collapsible Disabled, Fieldset
   DisabledCascade, Tabs DisabledTab, Toolbar Disabled, NavigationMenu Disabled.
   Base UI uses aria-disabled/data-disabled, not the `disabled` attr → assert
   aria-disabled/data-disabled (faithful), or fix component if it SHOULD be disabled.
3. **Residual overlay `toBeVisible`** — Drawer (Basic/CloseAsChild/Controlled/
   LeftSide/SizesVertical/Top = 6), Menu (ModalBackdrop/NestedSubmenu/
   WithKeyboardShortcuts = 3), AlertDialog (EscNoOps/ForcedColors/FragmentFooter = 3),
   ContextMenu (AsChild/PositionedPopup = 2), NavigationMenu (PopupMinWidthClamp/
   VerticalCustomChrome = 2), Toast (Stacked = 1). reduced-motion didn't fully fix —
   likely transform-slide enter not reset, or open-state query timing. Diagnose.
4. **Role/aria expectations vs Base UI 1.5.0** — Menubar (7, role=button name not
   found — triggers may be role=menuitem), Radio (3, role=radio name), NumberField
   (4, role=spinbutton name), Combobox (Basic toHaveValue / Multiple+Required text /
   FieldAriaAutowiring), Select (Required text / RTL not.toBeInTheDocument),
   Autocomplete (FieldAriaAutowiring aria-labelledby), Slider (RTL aria-valuenow),
   OtpField (RequiredInvalid aria-invalid / StandaloneAriaPaths axe), ScrollArea
   (KeyboardScroll scrollTop), Fieldset (WithFormIntegration aria-invalid).
5. **axe a11y violations** — Accordion Disabled (1), Drawer Nested (2),
   OtpField StandaloneAriaPaths (1).
6. **Focus** — ContextMenu WithDisabledItem not.toHaveFocus, Menu DisabledItem
   not.toHaveFocus, Dialog CloseAsChildLink (role=link name), Popover
   CloseAsChildComposition (role=link name), NavigationMenu KeyboardNav (role=link).

## Rule for every fix
Fix the RIGHT layer: if the test asserts the wrong thing for Base UI's real
(correct) behavior → fix the test faithfully (assert the real contract, never
weaken). If the component genuinely violates the intended contract → fix the
component + regression. Each cluster agent must verify its suites GREEN + axe-clean
before reporting. Orchestrator central-verifies + commits per cluster.

## Progress
- [ ] Cluster 1 hover overlays (PreviewCard+Tooltip) — AGENT RUNNING
- [ ] Cluster 2 toBeDisabled pattern
- [ ] Cluster 3 residual toBeVisible overlays
- [ ] Cluster 4 role/aria
- [ ] Cluster 5 axe violations
- [ ] Cluster 6 focus / role=link
- [ ] FINAL: full suite 0 failed; commit reduced-motion harness + all fixes
