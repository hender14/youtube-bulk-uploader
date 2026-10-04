# YouTube Uploader

The Rust implementation provides a local CLI and a browser-based uploader.
Videos transfer directly from the browser to private Cloud Storage, then a
Cloud Run Job uploads them to YouTube and removes the staged object after
YouTube confirms completion.

See the [Rust guide](rust/README.md) for build, OAuth, upload, and release
details. See the [GCP guide](infra/README.md) for infrastructure setup, required
administrator configuration, security boundaries, and cost caveats.

Cloud resources are not created automatically. Configure and review Terraform,
OAuth secrets, GitHub production approvals, and the YouTube API audit status
before enabling a live deployment. Non-private uploads remain disabled until
the YouTube compliance audit is confirmed.
