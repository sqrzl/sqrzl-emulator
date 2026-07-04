# Sqrzl Admin UI

This directory contains the Sqrzl admin SPA and its generated admin API client.

## Required Stack

- **Runtime:** `@askrjs/askr` supplies state, resources, router primitives, and
  SPA boot.
- **Behavior UI:** `@askrjs/ui` supplies headless accessible controls and
  interactions.
- **Visual UI:** `@askrjs/themes` supplies themed layouts, shells, navigation,
  controls, surfaces, and feedback.
- **Charts/icons:** Use `@askrjs/charts` and `@askrjs/lucide`.
- **Build integration:** `@askrjs/vite` owns the Askr JSX/Vite transform.
- **HTTP transport:** `@fgrzl/fetch` is the single HTTP client foundation.
- **API generation:** `@fgrzl/fetch-gen` generates `src/adapters/api.g.ts` from
  `../public/openapi.yml`.

These dependencies are architectural choices, not suggestions. Do not replace
them with locally invented versions because an integration is inconvenient.

## Commands

```bash
npm run dev        # Vite dev server with HMR (port 5173)
npm run build      # Production build to dist/
npm run preview    # Serve production build locally
npm test           # Vitest (jsdom)
npm run type-check # tsc --noEmit
npm run lint       # ESLint
npm run lint:fix   # ESLint with --fix
npm run fmt        # Prettier
npm run gen        # Regenerate src/adapters/api.g.ts from public/openapi.yml
```

## Architecture

- **Routing:** `src/main.tsx` imports `src/pages/_routes.tsx`, then boots
  `createSPA()` with the route manifest. Route branches live under
  `src/pages/auth` and `src/pages/app`.
- **Layouts:** `_layout.tsx` files own shells. The root layout owns
  `ThemeProvider`; branch layouts own auth chrome or authenticated sidebar
  chrome.
- **UI:** Prefer `@askrjs/themes/layouts`, `surfaces`, `controls`, `shells`,
  `navs`, and `feedback` before writing local components. Use app-local
  components only for product concepts such as `MetricCard` and `StatusBadge`.
- **State:** `const [value, setValue] = state(initial)`. Read with `value()`,
  update with `setValue(...)`. Use `derive()` for computed values and
  `resource()` for async data.
- **Data:** Route/container components own resources; `src/features` owns
  product workflows and DTO-to-view-model composition; `src/adapters` owns the
  configured `FetchClient`, generated API client, abort forwarding, and narrow
  transport concerns.
- **Consistency:** Event-sourced screens should expose pending writes,
  projection lag, stale data, retries, and manual refresh instead of hiding
  everything behind one loading state.
- **Styling:** Import the theme once in `src/styles.css`. App CSS should use
  `--ak-*` tokens and Sqrzl-owned `data-sqrzl-slot` hooks for local polish.
- **Charts:** Import chart components from `@askrjs/charts/components`; chart
  CSS is loaded from `@askrjs/charts/default`.
- **Vite plugin:** `askr()` from `@askrjs/vite` handles JSX transform. Do not
  add manual esbuild JSX config.

## API Contract Boundary

- `../public/openapi.yml` is the source of truth for the admin API surface.
- `src/adapters/api.g.ts` is generated output. Never manually patch generated
  endpoint signatures, paths, request construction, or response schemas.
- When generated API methods are missing path parameters, query parameters,
  request bodies, headers, or response types that the backend supports, fix
  `../public/openapi.yml` and run `npm run gen`.
- Shape OpenAPI operations so `@fgrzl/fetch-gen` emits usable calls. A
  technically valid contract that generates literal `{bucketName}` URLs or omits
  supported query inputs is incomplete for this application.
- Configure and reuse `@fgrzl/fetch` in the adapter boundary. Do not add
  parallel raw-`fetch` clients, ad hoc URL builders, or duplicate
  request/response typing for endpoints already present in the contract.
- A hand-written adapter may wrap generated calls only for real application
  behavior: DTO mapping, aggregation across operations, download presentation,
  upload UX, cancellation forwarding, or normalized feature errors. It must not
  reimplement generated endpoint transport.
- Existing handwritten transport duplication is migration debt, not a pattern
  to copy. Replace it with generated calls when touching that area.

## No Workarounds

- Do not invent a second router, state container, query system, component
  library, design-token set, API client, or transport layer.
- Do not build local substitutes for capabilities already supplied by Askr,
  `@askrjs/ui`, `@askrjs/themes`, `@askrjs/charts`, `@fgrzl/fetch`, or the
  generated client.
- Do not route around an incorrect generated API client with a second
  half-implemented client. Correct the OpenAPI contract or generator integration
  first.
- Do not introduce React-style hooks, effect-driven loading, JSX transform
  workarounds, or framework-agnostic patterns that bypass established Askr
  ownership.
- Do not use temporary mock or demo data in product paths when a supported admin
  API operation exists.

## Naming And Docs Policy

- Use `Askr` in prose and UI copy when referencing the framework. Keep package
  names like `@askrjs/*` and file names like `askr-bug.md` unchanged.
- Prefer single-surface truth in docs: describe behavior from the product and
  framework perspective, then list implementation notes separately.
- Keep `AGENTS.md` as the policy source for approach-level decisions. If a
  policy conflicts with existing code, call it out before changing direction.

## Validation

- Run `npm run gen` after any admin API contract change and verify the generated
  signatures before writing adapter code.
- Run `npm run type-check` and relevant tests after UI changes; do not mask type
  failures with duplicate interfaces or bypass layers.
- Use browser QA for at least one create dialog, one destructive dialog, and one
  nested bucket path when you change user-facing storage flows.

## Related Docs

- [UI quick start and architecture](README.md)
- [Storage UI guidelines](../docs/sqrzl-storage-ui-guidelines.md)
