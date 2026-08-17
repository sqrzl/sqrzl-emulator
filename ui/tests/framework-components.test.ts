import { readdirSync, readFileSync } from 'node:fs';
import { extname, join } from 'node:path';
import { describe, expect, it } from 'vite-plus/test';

const sourceRoot = join(import.meta.dirname, '..', 'src');
const legacyLayoutComponents = [
  'Box',
  'Inline',
  'Shell',
  'ShellMain',
  'ShellNav',
  'Stack',
] as const;

function sourceFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory()
      ? sourceFiles(path)
      : ['.ts', '.tsx'].includes(extname(entry.name))
        ? [path]
        : [];
  });
}

describe('Askr component boundaries', () => {
  it('uses the theme component entry point and preferred layout primitives', () => {
    for (const file of sourceFiles(sourceRoot)) {
      const source = readFileSync(file, 'utf8');

      expect(source, file).not.toMatch(/from ['"]@askrjs\/ui(?:\/[^'"]*)?['"]/);

      for (const component of legacyLayoutComponents) {
        expect(source, `${file} imports legacy ${component}`).not.toMatch(
          new RegExp(`\\b${component}\\b`)
        );
      }
    }
  });

  it('keeps pages, features, and components flowing in one direction', () => {
    for (const file of sourceFiles(sourceRoot)) {
      const source = readFileSync(file, 'utf8');
      const normalizedFile = file.replace(/\\/g, '/');

      expect(
        source,
        `${file} uses a deep parent-relative cross-layer import`
      ).not.toMatch(/from ['"](?:\.\.\/){2,}/);

      if (normalizedFile.includes('/src/components/')) {
        expect(source, `${file} reaches above the component layer`).not.toMatch(
          /from ['"]@\/(?:adapters|features|pages|shared)(?:\/|['"])/
        );
      }

      if (normalizedFile.includes('/src/pages/')) {
        expect(source, `${file} bypasses the feature layer`).not.toMatch(
          /from ['"]@\/components(?:\/|['"])/
        );
      }

      if (normalizedFile.includes('/src/features/')) {
        expect(source, `${file} depends on a page`).not.toMatch(
          /from ['"]@\/pages(?:\/|['"])/
        );
      }

      if (normalizedFile.includes('/src/shared/')) {
        expect(source, `${file} depends on another source layer`).not.toMatch(
          /from ['"](?:@\/|\.\.\/)/
        );
      }
    }
  });
});
