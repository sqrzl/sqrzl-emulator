import { ThemeScope } from '@askrjs/themes/theme';

export default function RootLayout({ children }: { children?: unknown }) {
  return (
    <ThemeScope>
      <main>{children}</main>
    </ThemeScope>
  );
}
