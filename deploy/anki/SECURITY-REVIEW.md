# Security review — 2026-10-24

## Fixed

- CVE-2026-111497 (HTTP2Shell / Node2Shell): upgrade all six Node.js build
  archives from 22.17.0 to 22.23.4, using the official distribution checksums.
- The running MCP image contained vulnerable Debian `libbrotli1` 1.0.9-2+b6.
  Require Debian's fixed 1.0.9-2+deb12u1 or newer when building the image.
  Curl is used only for the local health check, without `--compressed`; the
  vulnerable decoder was installed, but remote exploitability was not established.
- Update ammonia to 4.1.8, rustls to 0.23.46, rustls-webpki to 0.103.16,
  Tokio to 1.51.6, webpki-roots to 1.0.11, and zip to 9.1.0. The RustSec audit
  reports no advisories after these updates. Adapt archive handling to zip's
  fallible filename decoding and additional file-option lifetime.
- Build both Rust services using this workspace and its patched lockfile. The
  previous sync image installed an upstream revision with its own older lockfile.
- Update the 11 Python packages flagged by pip-audit and compatible JavaScript
  dependencies flagged by Yarn. Python auditing reports no known vulnerabilities.
- Add repeatable audit recipes and make the Rust audit a deployment prerequisite.

## Exposure and limits

The deployed Anki services run Rust executables, with no Node.js or Python
runtime. The sync container has no libbrotli1 package. Neither workspace HTTP
client enables the C Brotli decoder. Both containers run as an unprivileged UID,
with read-only root filesystems, dropped capabilities, memory/PID limits, and no
published host ports. MCP verifies JWT signature, issuer, resource audience,
expiry and owner before dispatch, and enforces its public hostname/body limit.

The existing owner-only `anki:sync` compatibility permission still authorizes
writes and maintenance, as documented in README.md. Removing it requires the
existing ChatGPT connection to request and receive the replacement scopes; this
review does not silently break the configured connection.

## Remaining desktop / development JavaScript advisories

The following dependency families still have reported advisories. They are not
included in the Rust service images. They require separate compatibility work;
this review does not claim that the desktop application or development tooling
is free of known vulnerabilities. Deprecation notices are excluded here.

- `@sveltejs/kit`: [advisory 1](https://github.com/advisories/GHSA-27mw-82g8-phjj), [advisory 2](https://github.com/advisories/GHSA-pfhg-q3qv-9q6r).
- `@vitest/mocker`: [advisory 1](https://github.com/advisories/GHSA-82fw-gwwq-j7x9), [advisory 2](https://github.com/advisories/GHSA-qxg6-89mg-rqv8).
- `fabric`: [advisory 1](https://github.com/advisories/GHSA-hfvx-25r5-qc3w), [advisory 2](https://github.com/advisories/GHSA-w22m-hvvm-xmwx).
- `ip-address`: [advisory 1](https://github.com/advisories/GHSA-h3mg-xc3c-68pw), [advisory 2](https://github.com/advisories/GHSA-j6r3-76f7-8jcv), [advisory 3](https://github.com/advisories/GHSA-mwp4-54f8-5fhr), [advisory 4](https://github.com/advisories/GHSA-rpw4-54j3-4h4q), [advisory 5](https://github.com/advisories/GHSA-rxp7-6hxx-x8q5), [advisory 6](https://github.com/advisories/GHSA-v2v4-37r5-5v8g).
- `postcss-selector-parser`: [advisory 1](https://github.com/advisories/GHSA-rj75-hqrm-r3gf).
- `tinypool`: [advisory 1](https://github.com/advisories/GHSA-5gmw-xhrv-c9v3), [advisory 2](https://github.com/advisories/GHSA-85c8-ppgw-ccpr).
- `vite`: [advisory 1](https://github.com/advisories/GHSA-587h-7wmj-h5pm), [advisory 2](https://github.com/advisories/GHSA-698v-4r3x-3vcv), [advisory 3](https://github.com/advisories/GHSA-9jrq-w75r-8gcw), [advisory 4](https://github.com/advisories/GHSA-r825-w8mm-5pxw), [advisory 5](https://github.com/advisories/GHSA-rq7h-c2jc-7f22).
- `vitest`: [advisory 1](https://github.com/advisories/GHSA-82fw-gwwq-j7x9).

## Sources and verification

- [Node.js October security release](https://nodejs.org/en/blog/vulnerability/october-2026-security-releases)
- [Debian Brotli security status](https://security-tracker.debian.org/tracker/CVE-2026-111497)
- `just anki-security-audit`: RustSec advisories for the complete workspace.
- `just anki-security-audit-python`: all groups in the frozen uv lockfile,
  evaluated using the auditing host's environment markers.
- `just anki-security-audit-js`: recursive Yarn dependency advisory report.

A dependency audit and targeted code/configuration review are not an exhaustive
security assessment. Desktop Qt WebEngine's bundled Chromium was not separately
scanned. No exploit payloads were sent to the production services.
