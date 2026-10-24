set windows-shell := ["pwsh", "-NoLogo", "-NoProfileLoadTime", "-Command"]

mod release

# Show available commands
default:
    @just --list

# Build the Rust-only client feasibility gate and official sync server
anki-stack-build:
    {{ ninja }} rslib:proto ftl:repo
    cargo build --locked -p anki-headless-spike -p anki-sync-server -p anki-mcp

anki-stack-test:
    {{ ninja }} rslib:proto ftl:repo
    cargo test --locked -p anki-headless-spike -p anki-mcp

anki-stack-check:
    cargo clippy --locked -p anki-headless-spike -p anki-mcp --tests -- -D warnings
    out/node_modules/.bin/dprint check 'deploy/anki/*.md' 'deploy/anki/*.mjs' 'services/**/*.json' 'services/**/Cargo.toml'

anki-stack-fmt:
    cargo +nightly-2025-03-20 fmt -p anki-headless-spike -p anki-mcp
    out/node_modules/.bin/dprint fmt 'deploy/anki/*.md' 'deploy/anki/*.mjs' 'services/**/*.json' 'services/**/Cargo.toml'

anki-inspect-export export target:
    cargo run --locked -p anki-headless-spike -- --inspect-export {{quote(export)}} --collection {{quote(target)}}

anki-inspect-collection target:
    cargo run --locked -p anki-headless-spike -- --inspect-collection --collection {{quote(target)}}

anki-inspect-notetype target id:
    cargo run --locked -p anki-headless-spike -- --inspect-notetype {{quote(id)}} --collection {{quote(target)}}

anki-preview-migration target id:
    cargo run --locked -p anki-mcp -- --preview-migration {{quote(id)}} --collection {{quote(target)}}

anki-context-fixture target output:
    cargo run --locked -p anki-mcp -- --build-context-fixture {{quote(output)}} --collection {{quote(target)}}

anki-context-browser-test rendered:
    out/extracted/node/bin/node deploy/anki/context-browser-test.mjs {{quote(rendered)}}

anki-spike *args:
    cargo run --locked -p anki-headless-spike -- {{args}}

anki-update-sanitizer:
    cargo update -p ammonia --precise 4.1.8

# Update dependencies affected by the October 2026 security advisories
anki-security-update:
    cargo update -p ammonia --precise 4.1.8
    cargo update -p rustls -p rustls-webpki -p webpki-roots -p zip
    cargo update -p tokio --precise 1.51.6

anki-security-audit:
    cargo deny check advisories

anki-security-update-js:
    {{ yarn }} up -R ajv brace-expansion devalue diff esbuild http-cache-semantics ip-address js-yaml minimatch nanoid picomatch postcss-selector-parser source-map-js svgo yaml

anki-security-audit-js:
    {{ yarn }} npm audit --all --recursive --json

anki-security-update-python:
    {{ uv }} lock --upgrade-package anyio --upgrade-package certifi --upgrade-package filelock --upgrade-package gitpython --upgrade-package pip --upgrade-package pygments --upgrade-package pytest --upgrade-package setuptools --upgrade-package urllib3 --upgrade-package virtualenv --upgrade-package werkzeug

anki-security-audit-python:
    {{ uv }} export --frozen --all-groups --no-hashes --no-emit-project --format requirements-txt --output-file /tmp/anki-security-requirements.txt
    {{ uv }} tool run pip-audit --disable-pip --no-deps -r /tmp/anki-security-requirements.txt

# Build the official server with the patched workspace lockfile
anki-sync-image:
    docker build --platform linux/amd64 -f deploy/anki/Dockerfile.sync -t anki-sync:dd324c8 .

anki-mcp-image:
    docker build --platform linux/amd64 -f deploy/anki/Dockerfile.mcp -t anki-mcp:staging .

# Deploy staging through the existing Caddy edge
anki-deploy *args:
    bash deploy/anki/deploy.sh {{ args }}

anki-deploy-syntax:
    bash -n deploy/anki/deploy.sh deploy/anki/backup-snapshot.sh deploy/anki/maintenance.sh
    ansible-playbook -i deploy/anki/ansible/inventory.yml deploy/anki/ansible/site.yml --syntax-check -e anki_image_archive=/tmp/anki-image-syntax.tar

anki-maintenance-test:
    python3 deploy/anki/maintenance-test.py

# Build the project
build:
    {{ ninja }} pylib qt

# Build and run Anki in development mode
run *args:
    {{ run_script }} {{ args }}

# Build and run Anki in optimized (release) mode
run-optimized *args:
    {{ if os() == "windows" { "$env:RELEASE='1'; .\\run.bat" } else { "RELEASE=1 ./run" } }} {{ args }}

# Watch web sources and rebuild/reload Anki's web stack on change (macOS/Linux)
web-watch:
    ./tools/web-watch

# Rebuild and reload Anki's web stack without restarting (macOS/Linux)
rebuild-web:
    ./tools/rebuild-web

# Build wheels (needed for some platforms)
wheels:
    {{ ninja }} wheels

# Build and run all checks (lint + test) - lets ninja handle dependencies
check:
    {{ ninja }} pylib qt check

# Run all tests (Rust, Python, TypeScript). Pass --coverage to enforce coverage, and --html to include HTML reports.
[arg("coverage", long="coverage", value="--coverage")]
[arg("html", long="html", value="--html")]
test coverage='' html='':
    just {{ if coverage == "--coverage" { "coverage " + html } else { "_test" } }}

# Run coverage for all test stacks. Pass --html to also generate HTML reports.
[arg("html", long="html", value="--html")]
coverage html='':
    just _coverage-rust {{ html }}
    just _coverage-py {{ html }}
    just _coverage-ts {{ html }}

# Run Rust tests. Pass --coverage to enforce Rust coverage, and --html to include an HTML report.
[arg("coverage", long="coverage", value="--coverage")]
[arg("html", long="html", value="--html")]
test-rust coverage='' html='':
    just {{ if coverage == "--coverage" { "_coverage-rust " + html } else { "_test-rust" } }}

# Run Python tests (pylib + qt). Pass --coverage to enforce coverage, and --html to include HTML reports.
[arg("coverage", long="coverage", value="--coverage")]
[arg("html", long="html", value="--html")]
test-py coverage='' html='':
    just {{ if coverage == "--coverage" { "_coverage-py " + html } else { "_test-py" } }}

# Run TypeScript/Svelte Vitest tests. Pass --coverage to enforce coverage, and --html to include an HTML report.
[arg("coverage", long="coverage", value="--coverage")]
[arg("html", long="html", value="--html")]
test-ts coverage='' html='':
    just {{ if coverage == "--coverage" { "_coverage-ts " + html } else { "_test-ts" } }}

# Run Playwright end-to-end tests. Pass --ui to open the interactive UI.
[arg("ui", long="ui", value="--ui")]
test-e2e ui='': _install-playwright-browsers
    {{ ninja }} pyenv ts:generated pylib qt
    {{ playwright_env }} {{ yarn }} test:e2e {{ ui }}

[private]
_test:
    {{ ninja }} check:rust_test check:pytest check:vitest

[private]
_test-rust:
    {{ ninja }} check:rust_test

[private]
_test-py:
    {{ ninja }} check:pytest

[private]
_test-ts:
    {{ ninja }} check:vitest

[private]
_coverage-rust html='':
    {{ if os_family() == "windows" { "tools\\coverage\\coverage-rust" } else { "tools/coverage/coverage-rust" } }} {{ html }}

[private]
_coverage-py html='':
    {{ ninja }} pylib qt
    just _coverage-py-pylib {{ html }}
    just _coverage-py-qt {{ html }}

[private]
_coverage-py-pylib html='':
    {{ if os_family() == "windows" { "tools\\coverage\\coverage-py" } else { "tools/coverage/coverage-py" } }} pylib {{ html }}

[private]
_coverage-py-qt html='':
    {{ if os_family() == "windows" { "tools\\coverage\\coverage-py" } else { "tools/coverage/coverage-py" } }} qt {{ html }}

[private]
_coverage-ts html='':
    {{ ninja }} node_modules ts:generated
    {{ if os_family() == "windows" { "tools\\coverage\\coverage-ts" } else { "tools/coverage/coverage-ts" } }} {{ html }}

[private]
_install-playwright-browsers:
    {{ ninja }} node_modules
    {{ playwright_env }} {{ yarn }} playwright install chromium

# Check formatting (fast, no build needed)
fmt:
    {{ ninja }} check:format

# Fix formatting
fix-fmt:
    {{ ninja }} format

# Run linting and type checking (requires build outputs)
lint:
    {{ ninja }} \
        check:clippy \
        check:mypy \
        check:ruff \
        check:eslint \
        check:svelte \
        check:typescript

# Fix auto-fixable lint issues (ruff + eslint)
fix-lint:
    {{ ninja }} fix:ruff fix:eslint

# Run minilints (copyright, contributors, licenses)
minilints:
    {{ ninja }} check:minilints

# Fix minilints (update licenses.json)
fix-minilints:
    {{ ninja }} fix:minilints

# Sync translation files
ftl-sync:
    {{ ninja }} ftl-sync

# Deprecate translation strings
ftl-deprecate:
    {{ ninja }} ftl-deprecate

# Build documentation site
docs:
    {{ uv }} run --group docs sphinx-build -b html docs out/docs/html
    @echo "Docs built at out/docs/html/index.html"

# Build and serve documentation site
docs-serve:
    {{ uv }} run --group docs sphinx-autobuild docs out/docs/html --host 127.0.0.1 --port 8000

# Build Rust API docs
docs-rust:
    cargo doc --open

# Dispatch CI workflow on a given branch or tag
ci branch:
    gh workflow run ci.yml --ref {{ branch }}

# Run Complexipy in regression-only mode
complexipy-diff:
    {{ ninja }} complexipy-diff

# Remove build outputs from out/ (pass keep-env to keep node_modules/pyenv); macOS/Linux
clean *args:
    ./tools/clean {{ args }}

# Helpers to get the right commands for the platform

ninja := if os() == "windows" { "tools\\ninja" } else { "./ninja" }
run_script := if os() == "windows" { ".\\run.bat" } else { "./run" }
playwright_env := if os() == "windows" { "set PLAYWRIGHT_BROWSERS_PATH=out\\playwright-browsers&&" } else { "PLAYWRIGHT_BROWSERS_PATH=out/playwright-browsers" }
yarn := if os() == "windows" { "out\\extracted\\node\\yarn.cmd" } else { "out/extracted/node/bin/yarn" }
uv := env("UV_BINARY", if os() == "windows" { "out\\extracted\\uv\\uv" } else { "out/extracted/uv/uv" })
export UV_PROJECT_ENVIRONMENT := if os() == "windows" { "out\\pyenv" } else { "out/pyenv" }
