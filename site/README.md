# RetroFeel website

Hugo renders the public website. Technical documentation stays in `../docs/` as Markdown.
The approved Variant Hunter story, original logo, and local Barlow fonts are retained.

## Build and check

Use Node.js 22+ and Hugo 0.164.0. From this directory:

```sh
npm ci --ignore-scripts
npm run check
npx playwright install chromium
npm run preview
# In another terminal:
npm run check:browser
```

Stop the preview, then run `npm run preview:contact` and, in another terminal,
`npm run check:contact`. This configuration simulates email locally using fictional
addresses. Never deploy `wrangler.test.jsonc`. Contact tests reject remote hosts.
Checks cover responsive layout, font loading, local links, CSP, native form delivery,
escaping, rate limits, missing configuration, and exact license-table fidelity.

## Configuration

The default Worker has no account, route, mailbox, or email delivery binding.
Submissions return HTTP 503 until all delivery settings are configured. The form
uses fixed operator-configured recipients; visitor addresses are Reply-To only.

For a future deployment, keep an operator-owned Wrangler config outside source
control (or ignored `wrangler.local.jsonc`) with the desired account, routes,
`CONTACT_EMAIL` Email Sending binding with explicit sender/destination allowlists,
and the `CONTACT_LIMIT` and `CONTACT_TOTAL_LIMIT` rate-limit bindings. Supply
`CONTACT_RECIPIENT` and `CONTACT_SENDER` through private environment settings.
Use verified mailboxes and namespace IDs allocated for that deployment. No
credentials or private config are copied into generated static assets.

The runtime preserves same-origin POST validation, bounded bodies, escaped input,
rate limits, and fail-closed delivery. Success means provider acceptance; it does
not prove inbox arrival. No automatic retry occurs after an ambiguous error.
Review operator-specific privacy/contact statements before a deployment.

`params.repositoryURL` in `hugo.toml` is optional and empty by default. Enable it
only when the repository is ready to be linked publicly. The current configuration
has no automatic publishing or deployment command.

## Licenses and assets

Hugo mounts the authoritative root `LICENSE.md` and `LICENSE` into the Licenses
page and byte-identical downloads. It mounts the app fonts and SIL OFL directly;
there is no duplicate tracker or external font request. The license page retains
Libretro acknowledgements. Review bundled core redistribution terms before release.

See [Workers configuration](https://developers.cloudflare.com/workers/wrangler/configuration/),
[environment settings](https://developers.cloudflare.com/workers/configuration/environment-variables/),
and [Email Sending bindings](https://developers.cloudflare.com/email-service/configuration/send-bindings/).

## Screenshots and accessibility checks

`npm run check:a11y` runs axe WCAG A/AA checks, color contrast, a project minimum
text size of 14px, and 200% text resizing. Body and form text use at least 16px.
These automated checks complement the keyboard and responsive browser checks;
they are not a claim of complete accessibility conformance.

The two PNGs show the dark Library and light Transcription views and are unretouched captures of the Linux GUI at 1280×800 with a clean,
network-isolated runtime. Library entries are fictional fixture files with the
app’s generated cover placeholders; the transcription view has no model installed.
Captions and alt text explain each view, and links open the original size. No
recordings, real game artwork, or personal configuration were used. To refresh,
run a clean build in an isolated runtime, populate fictional library fixtures,
open Library and Settings → Transcription, and capture the application window
with PNG metadata disabled. Review every screenshot before adding it to the site.
