mock_provider "google" {}

variables {
  project_id           = "yt-uploader-test"
  github_repository_id = "123456"
  github_owner_id      = "654321"
}

run "safe_defaults" {
  command = plan
  assert {
    condition     = length(google_cloud_run_v2_service.app) == 0
    error_message = "Cloud Run must not be created before the web image exists."
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
    condition     = strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "assertion.repository_id == '123456'") && strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "assertion.repository_owner_id == '654321'") && strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "environment:production") && strcontains(google_iam_workload_identity_pool_provider.github.attribute_condition, "refs/tags/v")
    error_message = "OIDC must require numeric repository/owner IDs, a release tag, and production."
  }
  assert {
    condition     = google_artifact_registry_repository.images.docker_config[0].immutable_tags
    error_message = "Release image tags must be immutable."
  }
}

run "reject_unpinned_image" {
  command = plan
  variables {
    enable_cloud_run = true
    cloud_run_image  = "example.invalid/image:latest"
  }
  expect_failures = [var.cloud_run_image]
}

run "opt_in_cloud_run_limits" {
  command = plan
  variables {
    enable_cloud_run = true
    cloud_run_image  = "example.invalid/web@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
}