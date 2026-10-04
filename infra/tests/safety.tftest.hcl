mock_provider "google" {}

variables {
  project_id           = "yt-uploader-test"
  github_repository_id = "123456"
  github_owner_id      = "654321"
}

run "safe_defaults" {
  command = plan
  assert {
    condition     = length(google_cloud_run_v2_service.app) == 0 && length(google_cloud_run_v2_job.upload) == 0
    error_message = "Cloud Run service and Job must remain disabled by default."
  }
  assert {
    condition     = google_storage_bucket.videos.public_access_prevention == "enforced" && google_storage_bucket.state.public_access_prevention == "enforced"
    error_message = "Application buckets must reject public access."
  }
  assert {
    condition     = google_storage_bucket.videos.soft_delete_policy[0].retention_duration_seconds == 0
    error_message = "Temporary videos must not remain billed through soft delete."
  }
  assert {
    condition     = strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "assertion.repository_id == '123456'") && strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "assertion.repository_owner_id == '654321'") && strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "assertion.sub == 'repo:hender14@654321/youtube-bulk-uploader@123456:environment:production'") && strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "refs/tags/v")
    error_message = "OIDC must require numeric repository/owner IDs, a release tag, and production."
  }
  assert {
    condition     = google_artifact_registry_repository.images.docker_config[0].immutable_tags
    error_message = "Release image tags must be immutable."
  }
  assert {
    condition     = google_secret_manager_secret_iam_member.runtime_oauth_client.role == "roles/secretmanager.secretAccessor"
    error_message = "Only the runtime account should read the OAuth client secret."
  }
}

run "reject_unpinned_image" {
  command = plan
  variables {
    enable_cloud_run     = true
    cloud_run_image      = "example.invalid/image:latest"
    oauth_redirect_uri   = "https://uploader.example.test/oauth/callback"
    oauth_allowed_emails = ["owner@example.test"]
  }
  expect_failures = [var.cloud_run_image]
}

run "opt_in_cloud_run_limits" {
  command = plan
  variables {
    enable_cloud_run     = true
    cloud_run_image      = "example.invalid/web@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    oauth_redirect_uri   = "https://uploader.example.test/oauth/callback"
    oauth_allowed_emails = ["owner@example.test"]
  }
  assert {
    condition     = length(google_cloud_run_v2_service.app) == 1
    error_message = "Explicit opt-in must create exactly one service."
  }
  assert {
    condition     = google_cloud_run_v2_service.app[0].template[0].scaling[0].min_instance_count == 0 && google_cloud_run_v2_service.app[0].template[0].scaling[0].max_instance_count == 1
    error_message = "The service must scale to zero and be limited to one instance."
  }
  assert {
    condition     = google_cloud_run_v2_service.app[0].template[0].containers[0].args[0] == "serve" && google_cloud_run_v2_service.app[0].template[0].containers[0].ports[0].container_port == 8080
    error_message = "The service must use the web command and expected port."
  }
  assert {
    condition     = google_cloud_run_v2_service_iam_member.public_oauth_entry[0].member == "allUsers" && google_cloud_run_v2_service_iam_member.public_oauth_entry[0].role == "roles/run.invoker"
    error_message = "The OAuth callback must be publicly reachable; app-level Google account allowlisting is the access control."
  }
  assert {
    condition     = google_storage_bucket.videos.cors[0].origin[0] == "https://uploader.example.test" && contains(google_storage_bucket.videos.cors[0].method, "PUT") && contains(google_storage_bucket.videos.cors[0].response_header, "Range")
    error_message = "The browser must be able to send resumable chunks directly to its Cloud Storage bucket."
  }
  assert {
    condition     = length(google_cloud_run_v2_job.upload) == 1 && google_cloud_run_v2_job.upload[0].template[0].template[0].containers[0].args[0] == "worker" && google_cloud_run_v2_job.upload[0].template[0].template[0].max_retries == 2
    error_message = "The uploader Job must run the worker and retry resumably."
  }
  assert {
    condition     = contains(google_project_iam_custom_role.upload_job_runner.permissions, "run.jobs.runWithOverrides")
    error_message = "The custom Job runner role must allow only the required override permission."
  }
  assert {
    condition     = contains(google_project_iam_custom_role.web_video_storage.permissions, "storage.objects.create") && !contains(google_project_iam_custom_role.web_video_storage.permissions, "storage.objects.delete") && contains(google_project_iam_custom_role.worker_video_storage.permissions, "storage.objects.delete")
    error_message = "Only the background worker may delete staged video objects."
  }
  assert {
    condition     = one([for item in google_cloud_run_v2_service.app[0].template[0].containers[0].env : item.value if item.name == "CLOUD_RUN_JOB_RESOURCE"]) == "projects/yt-uploader-test/locations/asia-northeast1/jobs/yt-uploader-worker"
    error_message = "The web service must receive the fully-qualified worker Job resource name."
  }
}