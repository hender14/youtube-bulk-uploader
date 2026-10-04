# GCP infrastructure

Terraform manages an existing dedicated, billing-enabled project. It does not
create a project, billing account, OAuth client, GitHub Environment, release tag,
or running uploader by default. CI validates Terraform and uses a mock Google
provider; it never performs a live plan or apply. Real changes are administrator
initiated and reviewed.

## Trust and resources

- Separate deployment and runtime service accounts, without private keys.
- GitHub OIDC requires the numeric repository and owner IDs, repository name,
  a `refs/tags/v*` ref, and the `production` Environment subject. Ordinary main
  pushes and fork PRs do not match these conditions.
- Deployment can write only the application registry, impersonate only the
  runtime account, and use Run Developer and Service Usage Consumer roles on
  the dedicated project. It is not granted access to the OAuth secret.
- The registry uses immutable image tags.
- Video and state buckets are private, with public-access prevention and soft
   delete disabled. Bucket-specific custom roles let the web app create video
   objects without reading or deleting them; only the worker can read/delete
   staged videos. State objects can be read and generation-conditionally updated.
   The video bucket accepts resumable PUT requests only from the HTTPS web origin,
   allowing direct browser upload without buffering media in Cloud Run.
- Terraform creates OAuth secret metadata, not secret versions or secret values.
   Runtime access and add-version permissions apply only to that secret. A
   separate OAuth-client secret is readable by the runtime but cannot be changed
   by the application.
- Cloud Run creation is opt-in and requires an image digest, HTTPS callback URL,
  and exactly one allowed Google account. Its minimum is zero instances and
  maximum is one; it runs `serve` on port 8080 and starts a separate `worker` Job.
  The public invoker permits OAuth redirects; app-level account checks protect data.
   The trusted web runtime can execute this fixed Job with per-execution overrides;
   protect its image and deployment path accordingly.

Administrator security and reviewed code are the trust boundary. Protect main,
version tags, and the `production` Environment, including required reviewers and
tag deployment restrictions. Workflow YAML alone does not enable approvals.
Do not add other providers with weaker conditions to the dedicated identity pool.
Run Developer is project-wide for initial service creation; narrow its scope
when bootstrap requirements are settled. This is not an Owner/Editor grant.

## Offline checks

```sh
terraform init -backend=false
terraform fmt -check -recursive
terraform validate
terraform test
```

The provider lock file is committed. CI performs only these checks and mock
plans, never `apply`. Real state, local variable values, and saved plans are
ignored by Git and must remain access-controlled.

## Administrator setup

1. Confirm the project, region, upload volume, and cost estimate before creating
   paid resources. Cloud Run-to-YouTube transfer pricing still needs confirmation;
   do not assume the Compute Engine exemption applies.
2. Authenticate using administrator Application Default Credentials outside the
   repository, such as `gcloud auth application-default login`. Do not create or
   paste a service account private key into Terraform.
3. Obtain GitHub numeric repository/owner IDs from the GitHub API. Set them and
   the real project ID in local `terraform.tfvars` using the example as a template.
   Its IDs are placeholders. Keep `enable_cloud_run=false` during bootstrap.
4. Run `terraform plan -out=review.tfplan`. Review the permissions and changes.
   Run `terraform apply review.tfplan` only after explicit approval. No automated
   apply or live plan is included in this PR.
5. Copy the `github_production_variables` output to the GitHub `production`
   Environment variables. These identifiers are not private keys. Require
   administrator approval and restrict deployment to protected version tags.
6. Follow [First Cloud Run deployment](#first-cloud-run-deployment) for the
   staged OAuth setup. The token secret starts empty; the first successful OAuth
   callback writes its version. Keep `youtube_audit_confirmed=false` until
   YouTube confirms the project audit. Never place OAuth values in Terraform
   variables, state, or repository files.

## First Cloud Run deployment

The production tag workflow always builds and publishes an immutable image. If
the `production` Environment variable `GCP_CLOUD_RUN_READY` is not exactly
`true`, it reports the image digest and skips Cloud Run updates. Use this for the
first release, then copy the digest from the workflow summary into
`cloud_run_image` in the ignored local `terraform.tfvars`.

The web process reads its OAuth client JSON while starting, but the Cloud Run
URL needed by the final OAuth redirect URI is only known after service creation.
For the initial creation only, add a temporary JSON version such as
`{"client_id":"bootstrap-placeholder","client_secret":"bootstrap-placeholder"}`
to the `yt-uploader-oauth-client` secret. Set `enable_cloud_run=true`, the
digest-pinned `cloud_run_image`, a temporary HTTPS `oauth_redirect_uri`, and the
single allowed account in local `terraform.tfvars`. Plan and review, then apply
to create the service and Job. OAuth will not work with the placeholder.

After Terraform outputs the service URL, create the Google Web OAuth client with
the exact redirect URI `https://SERVICE_URL/oauth/callback`, replace the
temporary Secret Manager version with JSON containing the real `client_id` and
`client_secret`, and set `oauth_redirect_uri` to that exact URL. Apply the
reviewed Terraform change and test login. Only after the service and Job are
working, set `GCP_CLOUD_RUN_READY=true` in the `production` Environment; later
version tags will then update both resources. Keep this variable unset or false
until that point.

Use a protected, dedicated backend for shared Terraform state. The application
state bucket expires objects after 30 days and must never be the Terraform
backend. Backend bootstrap and GitHub protections are separate setup steps.

## Lifecycle and deployment

The application must delete temporary videos immediately after confirmed
YouTube completion. The seven-day video lifecycle is only fallback cleanup for
abandoned objects and is asynchronous. Failed uploads must resume within that
configurable window. Application records expire after 30 days. The original PC
file and YouTube video are unaffected.

Terraform ignores image changes after initial Cloud Run creation so reviewed tag
deployments can own image updates; other service settings remain Terraform-owned.
The tag-triggered deployment workflow reruns CI, verifies that the tag points to
main and matches Cargo's version, then builds and publishes the image from that
source. After `GCP_CLOUD_RUN_READY=true`, it also updates the web service and
worker Job by image digest after production Environment approval. The workflow
does not create infrastructure or run Terraform apply. Review IAM before
adopting any existing service because Terraform only manages the invoker
permission it declares.

There is no load balancer, NAT, or reserved IP. Registry, storage, secret access,
and Cloud Run may still incur charges. Budget alerts are not hard spending caps.
Bucket/secret destruction is guarded. Disabling soft delete means deleted objects
cannot be recovered from it. Teardown needs a separately reviewed change.