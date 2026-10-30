# Ebook CLI

Rust CLI, SQLite worker and authenticated HTTP API for MAM → qBittorrent → repaired
EPUB → Kindle email. The default output is one JSON envelope on stdout; worker
events go to stderr/journald. It uses no AI service.

The CLI owns category `ebook-cli` and separate download, processing and SQLite
state directories. It can run alongside Chaptarr and Calibre-Web Automated
without changing their categories or completion hooks. Source EPUBs are read
and copied, never rewritten or removed.

## Build and setup

Requires Linux, Rust (tested with 1.99.1), a C compiler, qBittorrent's
Web API, a MAM account/session and TLS SMTP credentials. `Cargo.lock` pins the
dependencies. Use a standard Rust toolchain and Python 3 for the optional smoke
checks. Build and configure:

```sh
cargo test --locked -j 2
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked -j 2
python3 scripts/smoke.py --binary target/release/ebook
mkdir -p bin
install -m 0755 target/release/ebook bin/ebook
bin/ebook --config config.json setup
```

Use the release executable for the API and worker services. Once installed and
verified, `cargo clean` removes generated build output without removing `bin/ebook`.

The optional `scripts/cargo` helper uses Cargo from your PATH. It also supports
existing isolated installations under `.toolchain/` when present.

The template uses `/srv/ebook-cli/state` and `/srv/ebook-cli/processing` for
persistent state and working copies. Change them to absolute paths writable by
the account running the CLI, or create those directories with that ownership.
The template's qBittorrent URL is `http://127.0.0.1:8080`; change it if necessary.

Edit the generated mode-0600 JSON file. Configure MAM session, qBittorrent URL
and credentials, SMTP settings and Kindle mailbox. `remote_dir` and `local_dir`
must map the same download directory, end in `ebook-cli`, and not overlap state
or processing directories. The category is deliberately fixed to `ebook-cli`.
Keep the processing directory outside CWA ingest/library paths. The API token
is generated during setup; treat the entire config as a secret.

For compatible Chaptarr/CWA installations, `scripts/import-existing.py` is an
optional migration helper. It reads the standard `/opt/.../config` SQLite paths
and decrypts SMTP settings inside the `calibre-web-automated` container. It
requires exactly one configured Kindle destination and refuses to overwrite a
config. Set `EBOOK_QBITTORRENT_URL` to the reachable qBittorrent origin. Review
the helper's paths before use; other deployments should use `setup`.

Create the host download directory with ownership matching qBittorrent's UID.
The template maps `/downloads/books/ebook-cli` inside qBittorrent to
`/data/downloads/books/ebook-cli` on the CLI host. Both must refer to the same
files; mount or map them accordingly. Configure Amazon's approved sender list
using the SMTP From address, then check the connections:

```sh
bin/ebook --config config.json check --create-category
```

`check` tests MAM search, qBittorrent login/category and authenticated SMTP
connection. It sends no email. An existing category with a different path is
an error; the CLI never rewrites it. Existing torrents in another category
produce `existing_workflow`; the CLI never takes them over.

## Commands

```sh
bin/ebook search "Alice's Adventures in Wonderland"
bin/ebook select mam:12345 --language en --idempotency-key alice-1
bin/ebook request "Alice's Adventures in Wonderland" --author 'Lewis Carroll' --language en --idempotency-key alice-1
bin/ebook jobs
bin/ebook status JOB_ID
bin/ebook worker --once
bin/ebook worker
bin/ebook retry JOB_ID
bin/ebook retry JOB_ID --acknowledge-possible-duplicate
bin/ebook confirm-delivery JOB_ID
bin/ebook prepare source.epub prepared.epub --language fr
```

`mam:12345` is a placeholder; use an actual release ID returned by `search`.
Run either `select` or `request` for a book. Both reuse an existing release's job;
reusing an idempotency key with different release/language data is a conflict.
Use stable caller-supplied keys when retrying operations. A torrent's raw v1 info
hash is persisted before addition, so lost add responses reconcile by hash.
Prepared bytes also have a durable delivery deduplication record.

Search returns stable `mam:<id>` IDs, title, author, language, format, bytes,
seeders, restrictions and eligibility. Searches are bounded to 300 results and
report truncation. MAM still enforces account restrictions (including slots and
ratio) when downloading metainfo. CLI requests do not manage freeleech wedges.
Keep enough unsatisfied slots for the parallel Chaptarr workflow.

Automatic selection ranks title token overlap, author, language, seeders and
size. It automatically downloads exact title/author matches with a resolved
language; otherwise it returns `clarification_required` with candidates. An
explicit language wins. Without one, a small set of title stopwords can resolve
language among matching metadata; otherwise all clear matches must agree on
language. Short titles and unsupported languages may require clarification.
This conservative selector avoids a language-detection service. An agent can
resolve natural-language intent and supply an explicit language or release ID.

States: `queued` → `downloading` → `preparing` → `ready` → `sending` →
`smtp_accepted`. Only a user's device confirmation changes a job to `delivered`.
SMTP acceptance does **not** prove Amazon accepted or delivered the book.
Any SMTP failure or restart during `sending` becomes `send_uncertain` and is
never automatically resent. Inspect Kindle/email before acknowledging a retry.
Message IDs remain stable, but SMTP servers need not deduplicate them.

Worker errors are stored on the job and emitted as structured events. Dependency
failures have at most five consecutive retries at the worker's 15-second poll
interval; persistent failures become `failed`. Explicit retry reconciles the
same metainfo and torrent hash. Successful SMTP jobs cannot be retried. One
worker holds an OS file lock. Retry/confirmation use conditional SQLite updates
and operate while the daemon runs. Twenty active jobs bound normal queue growth.

Exit codes: `0` success, `2` invalid input/config, `3` not found or clarification,
`4` dependency failure, `5` conflict/restriction, `6` rejected EPUB, `7` local
storage/internal failure. `--human` pretty-prints the same JSON. `--help` and
`--version` use normal text. Failures have `code`, `message`, `retryable` fields.

The optional `scripts/ebook` launcher runs `bin/ebook` with the checkout's
`config.json` from any working directory. `EBOOK_CONFIG` or `--config` overrides
that location. It can be symlinked into a directory on your PATH.

## EPUB compatibility and limits

The MIT Kindling repair module is pinned unchanged in `vendor/`, with its license
and source revision. It supplies encoding declarations, body-ID link repair,
stray-image removal and language checks. Before invoking it, the wrapper validates
ZIP/XML structure, converts explicitly declared encodings to UTF-8, repairs the
container rootfile media type and supplies the selected language. It never lets
Kindling silently inject English. The container fix corresponds to CWA's
`fix_container_xml` behavior; see the upstream attribution below.

Tests use pinned CWA fixtures plus generated regressions for selected language,
Windows-1252, corruption, traversal, entities, DRM and decompression limits.
Encrypted books, active content, entity declarations, unknown/invalid encodings,
archives over 4096 entries, entries over 32 MiB, expanded data over 128 MiB,
compression ratios over 200:1 and attachments over 30 MiB fail safely. One EPUB
per torrent is supported. Multiple EPUBs require a future explicit file selector;
the worker fails the job instead of guessing. Only v1/hybrid torrent metainfo is
supported. No EPUB extraction or shell execution occurs during preparation.

## Worker and API services

The provided units are examples for a dedicated `ebook` account, a binary at
`/opt/ebook-cli/bin/ebook`, private configuration at `/etc/ebook-cli/config.json`,
and writable state under `/srv/ebook-cli`. Create that account and those paths,
copy the binary/config there, and grant the service account access to the shared
download directory. Adapt the unit files if your paths differ. The config must
remain mode 0600 and readable by the service account.

```sh
sudo install -m 0644 deploy/ebook-worker.service deploy/ebook-api.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ebook-worker ebook-api
journalctl -u ebook-worker -u ebook-api
```

Worker transition events, service restart counts and job errors are the local
operational signals. Review `failed` and `send_uncertain` jobs. Remote alert
forwarding is not included; connect journald/events to your observability
platform as appropriate. Back up the private config and SQLite using SQLite's backup API
(or stop the services and back up the whole state directory, including WAL).
Do not copy only a live SQLite main file. Retain `.torrent` files and processing
copies for restart recovery. There is no automatic retention/deletion policy.

## HTTPS API and Instinct integration

The API shares the CLI's application core and needs no AI service. Instinct or
another agent must support HTTPS JSON requests, bearer authentication and secure
credential storage. `openapi.json` documents the full contract. The server stays
on `127.0.0.1:8791`; expose it through the Apache example in
`deploy/apache-ebook.conf`, or access it using an authenticated SSH tunnel.

Keep `api_token` as the owner's credential. Add this optional configuration for
a restricted client (generate a fresh random token, at least 32 characters):

```json
"api": {
  "client_token": "REPLACE_WITH_A_RANDOM_SECRET_OF_AT_LEAST_32_CHARACTERS",
  "requests_per_minute": 30,
  "jobs_per_day": 10
},
"max_torrent_bytes": 104857600
```

Start with authenticated `GET /v1/help` (`ebookHelp` in OpenAPI). It explains the
workflow and examples, reports whether the destination is configured without
exposing its address, and returns this credential's permissions and actual limits.
It performs no dependency calls or job mutations. `delivery.configured` describes
local configuration only; it is not a readiness check or proof of Kindle delivery.

`search` only finds books. To fulfill a request to send one, call `request` or
`select` and require a returned `data.job.id` before claiming a job exists. The
worker handles downloading and emailing automatically. If a tool blocks the
submission, report its actual error rather than an unspecified server failure.

The restricted token permits `search`, `request`, `select`, `jobs` and `status`.
It cannot retry a send or confirm Kindle delivery. Use the local CLI for those
actions. Both credentials use `Authorization: Bearer TOKEN`; store the client's
token in its secret store, outside prompts, chat messages, URLs and source files.
To revoke access, set `api.client_token` to null and restart `ebook-api`. To
rotate it, replace that value and update the client's secret store. In-flight
jobs remain queued after revocation; inspect them separately if compromise is
suspected. Never share the full server configuration with the client.

Authentication runs before JSON parsing. Requests are limited to 8 KiB, two
concurrent operations, and a sliding minute budget per credential role. The
minute budget resets on API restart. The new-job quota uses a SQLite transaction
and a rolling 24-hour window; it survives restarts and token rotation. Existing
job replays cost no additional job quota. CLI submissions are outside the API
quota. Torrent sizes are checked against both tracker metadata and actual
metainfo before adding to qBittorrent. The download cap applies to CLI jobs too.
Existing downloads and seeded sources are preserved. Defaults work with old
configuration files; client access stays disabled until its token is configured.

For Apache deployment:

1. Point the chosen hostname at the server, and confirm ports 80/443 reach
   Apache. Keep port 8791 local. Create `/var/lib/ebook-cli/acme` for ACME challenges
   and `/var/lib/ebook-cli/public` for the public schema, outside the source tree.
2. Adapt `deploy/apache-ebook.conf` to the hostname. Initially enable only its
   HTTP vhost, then obtain a certificate with Certbot's webroot authenticator:
   `certbot certonly --webroot -w /var/lib/ebook-cli/acme -d ebook.example.com`.
3. Export the restricted client's contract with
   `python3 scripts/export-openapi.py --server https://ebook.example.com --output /var/lib/ebook-cli/public/openapi.json`.
   Enable the HTTPS vhost only after API tests pass. Run `apache2ctl configtest`
   before reloading Apache. Set a Certbot deploy hook to test and reload Apache
   after renewing this certificate, and verify renewal with `certbot renew
   --cert-name ebook.example.com --dry-run`.
4. Register `https://ebook.example.com/openapi.json` in the agent's tool settings
   and supply the restricted token through its secret store. Call `/v1/help` first. Use a 130-second
   client deadline. The Apache example exposes `POST /v1/operations`,
   authenticated `GET /v1/help`, and the public schema; plain HTTP serves only certificate challenges.

Proxy access logs contain method, path, status and timing, excluding headers,
query strings and bodies. API events contain operation names, job IDs and error
codes, excluding book queries and credentials. The Apache example excludes
headers/bodies from ModSecurity audit logs while preserving inherited rules;
review any locally customized logging before deployment. Liveness remains local
at `/health/live` and does not depend on MAM, qBittorrent or SMTP.

Example request body:

```json
{"action":"request","title":"Alice's Adventures in Wonderland","author":"Lewis Carroll","language":"en","idempotency_key":"client-alice-1"}
```

Use one unique idempotency key per intended submission and reuse it after a lost
response or timeout. Resolve `clarification_required` by asking the user to
choose and calling `select`; poll `status` every 15–30 seconds for the returned
job ID. Follow `Retry-After` on minute-limit responses. A `daily_quota_exceeded`
error requires waiting for earlier new jobs to leave the rolling 24-hour window.
Never claim Kindle delivery from `smtp_accepted`; the user confirms receipt.
No callback URL, arbitrary destination email, shell or filesystem operation is
exposed. Instinct registration and credentials are configured on its side.

To roll back, stop/disable only `ebook-worker` and `ebook-api`. Leave source
torrents seeding. Chaptarr and CWA continue using their existing configuration.
Retiring them is a separate cutover after device acceptance and validation.

Upstream references: [Kindling](https://github.com/ciscoriordan/kindling),
[original fixes](https://github.com/innocenat/kindle-epub-fix),
[CWA](https://github.com/crocodilestick/Calibre-Web-Automated).

## License and verification

The application is MIT licensed and free to use, modify and redistribute.
Vendored Kindling source retains its MIT license. CWA test fixtures retain their
upstream GPL-3.0-or-later notices and license in `tests/fixtures/cwa/`.

The test suite covers EPUB compatibility, unsafe inputs, selected language,
SQLite recovery, idempotency and qBittorrent response compatibility. The smoke
script uses a local qBittorrent stub and the real CLI/API; it sends no email.
Live Kindle delivery is separately confirmed by the device's user, since SMTP
acceptance alone is not proof of delivery.
