# Creator Video Transfer

This binary provides a local CLI plus a Cloud Run web
uploader. The browser sends resumable chunks directly to Cloud Storage; a
separate Cloud Run Job streams them to YouTube and removes the staged object
after YouTube confirms completion.

## Build and check

Run in this directory; `rust-toolchain.toml` selects Rust 1.90.0:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
./target/release/yt-uploader-rs --help
```

## Authentication and commands

The CLI uses an authorized-user OAuth JSON containing `client_id`,
`client_secret`, and `refresh_token`. By default the binary uses
`~/.config/yt-uploader/token.json`. Override it with `YOUTUBE_TOKEN_FILE` or
`--token-file`. It refreshes the access token
directly with Google. Keep the token file outside the repository with
owner-only permissions.

```sh
./target/release/yt-uploader-rs inventory
./target/release/yt-uploader-rs playlists
./target/release/yt-uploader-rs video VIDEO_ID
./target/release/yt-uploader-rs playlist-create 'Test playlist'
./target/release/yt-uploader-rs playlist-add PLAYLIST_ID VIDEO_ID
./target/release/yt-uploader-rs set-privacy VIDEO_ID unlisted
./target/release/yt-uploader-rs set-playlist-privacy PLAYLIST_ID unlisted
./target/release/yt-uploader-rs upload clip.mp4 --title 'Test clip' --audience not-kids
./target/release/yt-uploader-rs serve
```

`serve` requires `OAUTH_REDIRECT_URI`, `OAUTH_ALLOWED_EMAILS` (exactly one
verified account), and either `OAUTH_TOKEN_FILE` for local testing or
`OAUTH_SECRET_RESOURCE` for Cloud Run. Configure `OAUTH_CLIENT_CONFIG_RESOURCE`
to a Secret Manager version containing JSON with `client_id` and `client_secret`
(or use `OAUTH_CLIENT_ID` and `OAUTH_CLIENT_SECRET` for local development). The
browser is redirected to Google automatically. The callback uses state, PKCE,
and an OIDC nonce, checks Google's tokeninfo audience and verified email, then
creates a signed, HttpOnly, SameSite session. The authorization response uses a
form POST so its short-lived code is not placed in request URLs. After GCS
resumable upload completes, the service starts a durable worker Job. The worker
reads 8 MiB byte ranges, stores YouTube offsets and session URIs in the private
state bucket, verifies video privacy, adds the selected playlist by ID, then
deletes only the staged GCS object. The original PC file and YouTube video are
never deleted. Set
`COOKIE_SECURE=false` only for localhost session cookies; the OAuth state cookie
always requires Secure transport. The dashboard shows read-only channel
inventory plus the upload workflow. Non-private uploads stay disabled until
YouTube's project audit is confirmed by the administrator.

The live web service exposes public `/privacy-policy` and `/terms-of-service`
pages for OAuth review. The signed-in dashboard links to both pages. Deletion
requests can be opened through the repository's GitHub Issues; Issues are public,
so never include account identifiers, video URLs, OAuth tokens, or secrets.

Playlist creation is private and explicit. Reuse existing playlists by ID;
the CLI does not silently select a same-title playlist. Playlist membership is
checked before insertion. These list-then-write guards do not guarantee atomic
deduplication across independent processes and eventually consistent API lists.

Visibility changes read back the actual status. If YouTube has not reflected
a change yet, the CLI reports that verification failed; refresh inventory
before repeating a write. Playlist changes preserve the writable snippet and
use `snippet,status`, rather than updating `status` alone.

Uploads default to private and require an explicit audience. Requesting unlisted
or public uploads additionally requires confirmed YouTube project compliance
audit status via `YOUTUBE_AUDIT_CONFIRMED=true` or `--audit-confirmed`. This is an
administrator assertion, not a way to bypass Google restrictions. An existing
video's successful privacy update does not prove that new uploads are unrestricted.

## Resume and local state

The uploader reads bounded 8 MiB chunks. It saves the resumable session URI,
acknowledged offset, immutable request metadata, file size, and completed video
ID, keyed by SHA-256, under `~/.local/share/yt-uploader/rust-uploads`. Override
the directory with `YOUTUBE_UPLOAD_STATE_DIR` or `upload --state-dir`.

On Unix the record directory is mode 700 and JSON files are mode 600. Records
are atomically replaced and file locks prevent simultaneous uploads of the same
fingerprint within a shared state directory. Do not publish these records: the
session URI is sensitive.

If an upload fails, rerun the same command with the same source and metadata.
The next run probes the server offset before continuing. A completed record
prevents sending the file again. Expired/missing sessions are not automatically
replaced: inspect YouTube before deciding to create a fresh upload. A completed
upload whose visibility verification fails is still recorded, preventing
accidental reuploads; use inventory and privacy commands to resolve it.

The local source video is never deleted. Web upload state expires after 30 days;
abandoned staged videos have a seven-day fallback lifecycle rule. Completed
videos are deleted from staging immediately after independent YouTube privacy
readback. No cloud deployment is performed by the local CLI.

## Tagged releases

After merging reviewed changes into `main`, create a `vX.Y.Z` tag whose version
matches this crate. GitHub Actions runs Rust CI and verifies that the tagged
commit is reachable from `main`, then builds a Linux x86_64 executable and
creates a draft GitHub Release with automatically generated notes, ELF, and
`SHA256SUMS`. Ordinary pushes and PR merges do not create releases.

Protect version tags with repository rulesets, and configure required reviewers
and tag restrictions for the `release` Environment. Naming the Environment in
YAML alone does not enable approval. Review the notes and artifacts before
publishing; GitHub generates the headings and change list, not a translation.
No OAuth credentials are provided to the release build. Do not embed secrets
in source or build arguments: ELF and release notes become public on publication.

The separate tag-triggered deployment workflow rebuilds the same reviewed tag
from source and updates both the web service and worker Job by Artifact Registry
digest. It requires the
`production` Environment's approval, GCP variables, protected version tags,
numeric repository/owner IDs in Terraform, and a pre-created Cloud Run service.
It never deploys the public ELF release artifact or applies Terraform.

CI and release builds use Ubuntu 24.04, matching `Dockerfile.release`. Both
workflows run the executable inside that runtime image before publishing.
The isolated Docker context contains only the executable, never OAuth files.
Rustup reads the compiler and components from `rust-toolchain.toml`; workflow
files do not duplicate that version. The image runs the CLI/server as a non-root
user. The deployment workflow rebuilds this image from the protected tag.

Local development can use Ubuntu 26.04. Do not publish its native ELF as the
Ubuntu 24.04 release artifact: it may require newer glibc symbols. Publish only
the CI-built artifact verified inside the matching runtime container.