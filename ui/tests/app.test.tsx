import { describe, expect, it } from 'vite-plus/test';

describe('SPA structure', () => {
  it('groups app pages by buckets, mailboxes, and texts', () => {
    const structure = [
      'src/main.tsx',
      'src/pages/_routes.tsx',
      'src/pages/_layout.tsx',
      'src/pages/app/_routes.tsx',
      'src/pages/app/_layout.tsx',
      'src/pages/app/buckets/index.tsx',
      'src/pages/app/buckets/bucket.tsx',
      'src/pages/app/buckets/blob.tsx',
      'src/pages/app/mailboxes/index.tsx',
      'src/pages/app/mailboxes/mailbox.tsx',
      'src/pages/app/mailboxes/message.tsx',
      'src/pages/app/texts/index.tsx',
      'src/pages/app/texts/conversation.tsx',
      'src/pages/app/texts/message.tsx',
      'src/pages/auth/login.tsx',
      'src/pages/auth/logout.tsx',
      'src/features/buckets/buckets.query.ts',
      'src/features/objects/objects.query.ts',
      'src/shared/routes.ts',
      'src/adapters/api.g.ts',
    ];

    expect(structure).toContain('src/pages/_routes.tsx');
    expect(structure).toContain('src/pages/app/buckets/index.tsx');
    expect(structure).toContain('src/pages/app/buckets/blob.tsx');
    expect(structure).toContain('src/pages/app/mailboxes/index.tsx');
    expect(structure).toContain('src/pages/app/mailboxes/message.tsx');
    expect(structure).toContain('src/pages/app/texts/index.tsx');
    expect(structure).toContain('src/pages/app/texts/message.tsx');
    expect(structure).toContain('src/shared/routes.ts');
  });
});
