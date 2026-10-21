# Sqrzl Admin UI

## Development

Use the normal development command when the Rust emulator is available on
port 9001. Vite proxies admin API requests to that process:

```bash
npm run dev
```

For UI-only development, run the opt-in stateful mock API instead:

```bash
npm run dev:mock
```

The mock starts signed out. Log in with username `admin` and password
`sqrzl-secret`. Its compact walkthrough fixtures cover storage, mail, and text
messaging, with list pages intentionally capped at three rows. Creates,
uploads, edits, delivery transitions, retries, and deletes remain visible after
browser reloads because state lives in the Vite process. Restarting Vite resets
all state to the same deterministic fixtures. Mock callbacks are recorded but
never sent, and no provider APIs are called.

Mock mode intercepts every `/admin/v1` request. Unknown API routes return a
structured mock `404`; they are never forwarded to the Rust-server proxy.

The Sqrzl admin UI is an Askr SPA for the storage administration API.

Keep it small: sign in, search buckets, create/delete buckets, browse
folder-like blob keys, upload/delete blobs, view metadata, and download blob
content.

## Quick Start

```bash
npm ci
npm run gen      # Generate src/adapters/api.g.ts from ../public/openapi.yml
npm run type-check
npm run lint
npm run lint:fix
npm run fmt
npm run dev      # Start dev server at http://localhost:5173
npm run build    # Build for production
npm run preview  # Preview production build
npm test         # Run tests with Vitest
```

Node 24 or newer is required.

## Askr 0.5 package baseline

The UI is pinned to the coordinated stable Askr 0.5 package set. `vite-plus`
1.3.0 satisfies the optional peer range declared by `@askrjs/vite` 0.5.0.

| Package | Version |
| --- | ---: |
| `@askrjs/askr` | 0.5.0 |
| `@askrjs/auth` | 0.5.0 |
| `@askrjs/charts` | 0.5.0 |
| `@askrjs/lucide` | 0.5.0 |
| `@askrjs/themes` | 0.5.0 |
| `@askrjs/ui` | 0.5.0 |
| `@askrjs/vite` | 0.5.0 |
| `vite-plus` | 1.3.0 |

Migration was checked against the [Core 0.5.0 guide](https://github.com/askrjs/askr/blob/v0.5.0/docs/migration/0.5.0.md), the [coordinated package guide](https://github.com/askrjs/askr/blob/v0.5.0/docs/migration/0.5.0-packages.md), and the Themes, UI, and Vite 0.5.0 guides. The app already uses the registry-based router and named `askr()` Vite plugin; `@askrjs/auth` is declared directly because the route registry imports `requireUser`. The shared root layout now sets its vertical direction explicitly, and the responsive navbar utility group stays on the top row above `md` with a scoped override for the 0.5.0 collapsed-content direction. Logout reads its error state during render under Askr's development checks. The migration does not change the OpenAPI contract, generated client, or `@fgrzl/fetch` transport boundary.

## Routes

- `/login` for sign-in
- `/logout` for sign-out
- `/admin/buckets` for the bucket table
- `/admin/buckets/{bucketName}` for a bucket root
- `/admin/buckets/{bucketName}/{path}` for folder-like bucket paths
- `/admin/buckets/{bucketName}/blob/{blobId}` for blob details

## UI Scope

Everything uses Askr theme and UI primitives as the base. Local CSS is limited
to Sqrzl-owned `data-sqrzl-slot` polish for layout and storage-specific sizing.
See [`../docs/sqrzl-storage-ui-guidelines.md`](../docs/sqrzl-storage-ui-guidelines.md)
for the visual and interaction rules.

## Data Flow

- `src/features/auth/admin-session.ts` owns session resolution and auth helpers.
- `src/features/buckets/buckets.query.ts` loads and creates buckets.
- `src/features/objects/objects.query.ts` loads blob metadata and uploads blob
  content.
- `src/adapters/api.g.ts` remains generated from `../public/openapi.yml`.

## API Boundary

`../public/openapi.yml` is the source of truth. Run `npm run gen` after a
contract change. Pages and features use the configured generated adapter and do
not construct endpoint URLs or call global `fetch`.
