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
npm install
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
