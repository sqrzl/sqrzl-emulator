import { Link } from '@askrjs/askr/router';
import { LogOutIcon, MoonIcon, SunIcon } from '@askrjs/lucide';
import { Container, Section, Stack } from '@askrjs/themes/layouts';
import {
  Header,
  NavBrand,
  NavGroup,
  NavLink,
  Navbar,
} from '@askrjs/themes/shells';
import { ThemeToggle } from '@askrjs/themes/theme';
import { adminBucketsPath, logoutPath } from '../../shared/routes';

export default function AppLayout({ children }: { children?: unknown }) {
  return (
    <>
      <Header>
        <Container>
          <Navbar breakpoint="md" aria-label="Application navigation">
            <NavBrand>
              <Link href={adminBucketsPath()}>Peas</Link>
            </NavBrand>
            <NavGroup align="end">
              <ThemeToggle
                aria-label="Toggle theme"
                darkIcon={<MoonIcon aria-hidden="true" />}
                lightIcon={<SunIcon aria-hidden="true" />}
              />

              <NavLink href={logoutPath()} match="exact">
                <LogOutIcon />
              </NavLink>
            </NavGroup>
          </Navbar>
        </Container>
      </Header>
      <Section size="4">
        <Container>
          <Stack gap="4">{children}</Stack>
        </Container>
      </Section>
    </>
  );
}
