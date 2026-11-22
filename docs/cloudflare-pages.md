# Publish the ChampionVibe landing page on Cloudflare Pages

The English product site in `landing/` is a standalone static site. It is separate from the Tauri app and the license issuer. It does not require Workers, Pages Functions, environment secrets, or an Android/Windows build.

## Preview locally

```sh
npm ci
npm run dev:site
```

The preview is at `http://localhost:4173/`. Build the same files Cloudflare Pages will serve with `npm run build:site`. This writes `dist-site/`; the Tauri app still uses `dist/` and the existing `npm run build` command.

## Cloudflare Pages settings

Connect this repository to a **Pages** project via Git integration, then use these build settings:

| Setting | Value |
| --- | --- |
| Production branch | `main` |
| Framework preset | None |
| Root directory | Repository root (leave blank) |
| Build command | `npm run build:site` |
| Build output directory | `dist-site` |
| Build environment variable | `NODE_VERSION=22` |

Allow the relevant branch under preview controls to review it before merging. Pages serves the files over HTTPS and picks up `dist-site/_headers` for security headers and caching of versioned assets. For a Pages project set up with Direct Upload instead, build locally and upload **the contents of `dist-site/`**, not `dist/` or the repository itself.

## Release link and assets

- Before the first public release, the site sends readers to the official GitHub releases page and does not advertise installers as available. Once the installers are published and verified, update the release section and FAQ; use official download URLs only.
- The images are AI-generated illustrations inspired by Jamaican sound system culture, not documentary photos of a named venue or event. Fonts are served locally, with their SIL Open Font License texts under `landing/public/fonts/`.
