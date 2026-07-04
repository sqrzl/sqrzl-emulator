import { Link } from '@askrjs/askr/router';
import { LogOutIcon, MoonIcon, SunIcon } from '@askrjs/lucide';
import {
  Container,
  Header,
  NavBrand,
  NavGroup,
  NavLink,
  Navbar,
  Section,
  Stack,
} from '@askrjs/themes/components';
import { ThemeToggle } from '@askrjs/themes/theme';
import { isDevAuthBypassed } from '../../features/auth/admin-session';
import { adminBucketsPath, logoutPath } from '../../shared/routes';

export default function AppLayout({ children }: { children?: unknown }) {
  const showLogout = !isDevAuthBypassed();

  return (
    <>
      <Header>
        <Container>
          <Navbar breakpoint="md" aria-label="Application navigation">
            <NavBrand>
              <Link href={adminBucketsPath()}>Sqrzl</Link>
            </NavBrand>
            <NavGroup align="end">
              <ThemeToggle
                aria-label="Toggle theme"
                darkIcon={<MoonIcon aria-hidden="true" />}
                lightIcon={<SunIcon aria-hidden="true" />}
              />

              {showLogout ? (
                <NavLink href={logoutPath()} match="exact" aria-label="Log out">
                  <LogOutIcon aria-hidden="true" />
                </NavLink>
              ) : null}
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
