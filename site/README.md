# Remux documentation site

User docs for Remux, built with [Starlight](https://starlight.astro.build) and published to
<https://rakanalh.github.io/remux/> by `.github/workflows/docs.yml` on every push to `master`
that touches `site/`.

```sh
npm ci
npm run dev       # http://localhost:4321/remux/
npm run build     # static output in dist/
npm run preview   # serve dist/ (search only works here, not in dev)
```

Pages live in `src/content/docs/`; the sidebar is declared in `astro.config.mjs`.
Screenshots in `src/assets/screenshots/` are copies of `docs/screenshots/`; re-copy them when
those are regenerated.
