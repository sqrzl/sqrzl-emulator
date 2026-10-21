import { Link } from '@askrjs/askr/router';
import { LogOutIcon, MoonIcon, SunIcon } from '@askrjs/lucide';
import {
  Brand,
  BrandLabel,
  BrandMark,
  Container,
  Header,
  NavBrand,
  NavGroup,
  NavLink,
  Navbar,
} from '@askrjs/themes/components';
import { ThemeToggle } from '@askrjs/themes/theme';
import sqrzlLogo from '@/assets/sqrzl-logo.png';
import { isDevAuthBypassed } from '@/features/auth';
import {
  adminBucketsPath,
  adminMailboxesPath,
  adminTextsPath,
  logoutPath,
} from '@/shared/routes';

export default function AppLayout({ children }: { children?: unknown }) {
  const showLogout = !isDevAuthBypassed();

  return (
    <>
      <Header sticky>
        <Container paddingY="sm">
          <Navbar
            breakpoint="md"
            width="full"
            aria-label="Application navigation"
            data-sqrzl-slot="application-navbar"
          >
            <NavBrand>
              <Brand asChild>
                <Link href={adminBucketsPath()}>
                  <BrandMark
                    aria-hidden="true"
                    data-sqrzl-slot="application-brand-mark"
                  >
                    <img
                      data-sqrzl-slot="application-brand-logo"
                      src={sqrzlLogo}
                      alt=""
                    />
                  </BrandMark>
                  <BrandLabel>Sqrzl</BrandLabel>
                </Link>
              </Brand>
            </NavBrand>
            <NavGroup>
              <NavLink href={adminBucketsPath()} match="prefix">
                Buckets
              </NavLink>
              <NavLink href={adminMailboxesPath()} match="prefix">
                Mailboxes
              </NavLink>
              <NavLink href={adminTextsPath()} match="prefix">
                Texts
              </NavLink>
            </NavGroup>
            <NavGroup align="end" data-sqrzl-slot="navbar-utilities">
              <ThemeToggle
                aria-label="Toggle theme"
                variant="ghost"
                size="icon"
                darkIcon={<MoonIcon size={16} aria-hidden="true" />}
                lightIcon={<SunIcon size={16} aria-hidden="true" />}
              />

              {showLogout ? (
                <NavLink
                  href={logoutPath()}
                  match="exact"
                  aria-label="Log out"
                >
                  <LogOutIcon size={16} aria-hidden="true" />
                </NavLink>
              ) : null}
            </NavGroup>
          </Navbar>
        </Container>
      </Header>
      {children}
    </>
  );
}
