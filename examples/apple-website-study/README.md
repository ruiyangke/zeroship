# Apple Website Study

A standalone product-page study for `@zeroship/ui`, modeled after Apple's
public iPhone page.

The demo uses local Apple-sourced page assets saved in
`src/assets/apple/`. The `sources.json` manifest records source URLs, alt text,
file sizes, and checksums.

## Structure

- `src/assets/appleAssets.ts` maps downloaded image files to typed imports.
- `src/content/applePage.ts` owns static page content and navigation models.
- `src/components/` contains reusable UI pieces like the global nav, mega menu,
  carousel paddles, section headers, and product cards.
- `src/sections/` contains page regions such as lineup, guided tour, buying,
  privacy, essentials, and the companion accordion.
- `src/styles/` splits base, navigation, section, and responsive/motion rules.

```bash
pnpm --filter apple-website-study dev
pnpm --filter apple-website-study build
```
