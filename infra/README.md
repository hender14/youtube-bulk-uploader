# GCP infrastructure

Terraform manages an existing dedicated, billing-enabled project. It does not
create a project, billing account, OAuth client, GitHub Environment, release tag,
or running uploader by default. No real GCP plan or apply has been performed.
Tests use a mock Google provider with no cloud credentials or cloud resources.

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
  delete disabled. Runtime object permissions apply only to those buckets.
- Terraform creates OAuth secret metadata, not secret versions or secret values.
   Runtime access and add-version permissions apply only to that secret. A
   separate OAuth-client secret is readable by the runtime but cannot be changed
   by the application.
- Cloud Run creation is opt-in and requires an image digest, HTTPS callback URL,
   and exactly one allowed Google account. Its minimum is zero instances and
   maximum is one; it runs `serve` on port 8080. Its public invoker permits access
   to the OAuth redirect; application-level Google account checks protect data.

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
6. Create the Web OAuth client and register the exact HTTPS callback URL. Set
   `oauth_redirect_uri` and exactly one `oauth_allowed_emails` value. Add a JSON
   OAuth client configuration (`client_id`, `client_secret`) to the
   `oauth_client_secret` resource using a secure out-of-band method. The token
   secret starts empty; the first successful OAuth callback writes its version.
   Never place OAuth values in Terraform variables, state, or repository files.

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
main and matches Cargo's version, builds and publishes the image from that source,
then updates the existing Cloud Run service by image digest after production
Environment approval. Terraform must first create the service
(`enable_cloud_run=true`) and its runtime configuration. The workflow does not
create infrastructure or run Terraform apply. Review IAM before adopting any
existing service because Terraform only manages the invoker permission it
declares.

There is no load balancer, NAT, or reserved IP. Registry, storage, secret access,
and Cloud Run may still incur charges. Budget alerts are not hard spending caps.
Bucket/secret destruction is guarded. Disabling soft delete means deleted objects
cannot be recovered from it. Teardown needs a separately reviewed change.