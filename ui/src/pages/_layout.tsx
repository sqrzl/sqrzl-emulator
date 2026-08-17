import { ThemeScope } from '@askrjs/themes/theme';
import { Block } from '@askrjs/themes/components';

export default function RootLayout({ children }: { children?: unknown }) {
  return (
    <ThemeScope>
      <Block minHeight="screen">{children}</Block>
    </ThemeScope>
  );
}
