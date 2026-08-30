// The luvus website: a custom product landing at `/` (src/pages/index.astro)
// plus Starlight documentation under `/docs/…` (all content lives in the
// `docs/` subfolder of the content collection, so its slugs — and URLs — are
// prefixed with /docs/ and the root stays free for the landing page).
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://luvus.dev',
  integrations: [
    starlight({
      title: 'Luvus',
      description:
        'Mission control for your AI coding agents. Run Claude Code, Copilot, Codex, and opencode side by side, with a live view of every agent, session resume, and multi-agent orchestration.',
      // No `logo` option: the SiteTitle override renders the canonical,
      // theme-aware Luvus SVG shared with the landing pages. `favicon` covers
      // the default <link>; the small and Apple sizes are added by hand.
      favicon: '/favicon.png',
      head: [
        { tag: 'link', attrs: { rel: 'icon', type: 'image/png', sizes: '32x32', href: '/favicon-32.png' } },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' } },
        { tag: 'meta', attrs: { property: 'og:image', content: 'https://luvus.dev/og.png' } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        { tag: 'meta', attrs: { property: 'og:image:alt', content: 'luvus: mission control for your AI coding agents' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: 'https://luvus.dev/og.png' } },
      ],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/RizRiyz/luvus' },
      ],
      customCss: [
        // Inter for running prose, JetBrains Mono for code, IBM Plex Mono for
        // the wordmark, headings and labels. The landing page is mono
        // throughout, but documentation is long-form reading: setting body text
        // in a monospace face hurts legibility over paragraphs, so the docs keep
        // a proportional face for prose and stay mono everywhere it signifies
        // something (code, headings, chrome).
        '@fontsource-variable/inter',
        '@fontsource-variable/jetbrains-mono',
        '@fontsource/ibm-plex-mono/500.css',
        '@fontsource/ibm-plex-mono/600.css',
        '@fontsource/ibm-plex-mono/700.css',
        // Orbit for the wordmark only. One weight, one place: the brand.
        '@fontsource/orbit/400.css',
        // luvus's shipped palettes (generated from src/ui/theme.rs) followed by
        // the brand layer that maps their tokens onto Starlight's variables.
        './src/styles/themes.css',
        './src/styles/custom.css',
      ],
      // The docs wear the landing page's chrome: luvus's own palettes instead
      // of a light/dark switch (ThemeProvider paints the saved one before first
      // paint, ThemeSelect is the palette picker in the navbar), and the site
      // nav in place of the social-icon row. See src/styles/custom.css for how
      // the palette tokens are mapped onto Starlight's variables.
      components: {
        ThemeProvider: './src/components/ThemeProvider.astro',
        ThemeSelect: './src/components/ThemeSelect.astro',
        SocialIcons: './src/components/SocialIcons.astro',
        SiteTitle: './src/components/SiteTitle.astro',
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Quickstart', slug: 'docs' },
            { label: 'Installation', slug: 'docs/getting-started/installation' },
            { label: 'Your First Session', slug: 'docs/getting-started/first-session' },
            { label: 'Core Concepts', slug: 'docs/getting-started/concepts' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Panes, Tabs & Workspaces', slug: 'docs/guides/layout' },
            { label: 'Luvus Bar', slug: 'docs/guides/bar' },
            { label: 'Working with Agents', slug: 'docs/guides/agents' },
            { label: 'Agents Talking to Agents', slug: 'docs/guides/agent-messaging' },
            { label: 'Control Luvus from Codex', slug: 'docs/guides/codex-plugin' },
            { label: 'Multi-Agent Orchestration', slug: 'docs/guides/orchestration' },
            { label: 'The Git Tab', slug: 'docs/guides/git' },
            { label: 'Browsing & Opening Files', slug: 'docs/guides/files' },
            { label: 'Global Fuzzy Finder', slug: 'docs/guides/search' },
            { label: 'DIFF Review', slug: 'docs/guides/diff' },
            { label: 'Worktrees', slug: 'docs/guides/worktrees' },
            { label: 'Remote Sessions', slug: 'docs/guides/remote' },
            { label: 'Mobile Sessions', slug: 'docs/guides/mobile' },
            { label: 'Scrollback & Copy', slug: 'docs/guides/scrollback' },
            { label: 'Settings & Theming', slug: 'docs/guides/settings' },
            { label: 'Community Themes', slug: 'docs/guides/themes' },
            { label: 'Scripting luvus', slug: 'docs/guides/scripting' },
            { label: 'Automating with UHP', slug: 'docs/guides/uhp' },
            { label: 'Remote UHP Access', slug: 'docs/guides/uhp-access' },
          ],
        },
        {
          label: 'Extending',
          items: [
            { label: 'Using Modules', slug: 'docs/extend/using-modules' },
            { label: 'Writing a Module', slug: 'docs/extend/writing-modules' },
            { label: 'Adding Agent Support', slug: 'docs/extend/adding-agent-support' },
            // The community index is a standalone page, not a docs entry.
            { label: 'Module Index', link: '/modules/', attrs: { target: '_self' } },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI Commands', slug: 'docs/reference/cli' },
            { label: 'UHP Methods', slug: 'docs/reference/api' },
            { label: 'Universal Harness Protocol', slug: 'docs/reference/uhp' },
            { label: 'Keybindings', slug: 'docs/reference/keybindings' },
            { label: 'Configuration', slug: 'docs/reference/configuration' },
            { label: 'Supported Agents', slug: 'docs/reference/agents' },
          ],
        },
        {
          label: 'Explanation',
          items: [
            { label: 'Architecture', slug: 'docs/explanation/architecture' },
            { label: 'Security Model', slug: 'docs/explanation/security' },
          ],
        },
        { label: 'FAQ & Troubleshooting', slug: 'docs/faq' },
      ],
    }),
  ],
});
