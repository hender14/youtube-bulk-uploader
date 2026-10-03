# Rust YouTube uploader

This binary runs without Python. It currently provides a local CLI, not the
planned Cloud Run web application. Python remains available during migration.

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

Use an existing authorized-user OAuth JSON containing `client_id`,
`client_secret`, and `refresh_token`. By default the binary uses
`~/.config/yt-uploader/token.json`, shared with the local Python app. Override
it with `YOUTUBE_TOKEN_FILE` or `--token-file`. It refreshes the access token
directly with Google; first-time browser OAuth is not yet implemented in Rust.
Keep the token file outside the repository with owner-only permissions.

```sh
./target/release/yt-uploader-rs inventory
./target/release/yt-uploader-rs playlists
./target/release/yt-uploader-rs video VIDEO_ID
./target/release/yt-uploader-rs playlist-create 'Test playlist'
./target/release/yt-uploader-rs playlist-add PLAYLIST_ID VIDEO_ID
./target/release/yt-uploader-rs set-privacy VIDEO_ID unlisted
./target/release/yt-uploader-rs set-playlist-privacy PLAYLIST_ID unlisted
./target/release/yt-uploader-rs upload clip.mp4 --title 'Test clip' --audience not-kids
```

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

The local source video is never deleted. Cloud temporary-object deletion,
remote state retention, direct browser-to-Storage uploads, Web OAuth, and
Cloud Run Jobs are still to be implemented. No cloud deployment is performed
by this CLI.

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

The release workflow does not deploy to GCP. The later deployment stage must
build from trusted, reviewed source rather than execute an externally supplied
ELF, and require separate production approval and narrowly scoped OIDC access.