locals {
  services = toset([
    "run.googleapis.com",
    "artifactregistry.googleapis.com",
    "iam.googleapis.com",
    "iamcredentials.googleapis.com",
    "sts.googleapis.com",
    "storage.googleapis.com",
    "secretmanager.googleapis.com",
    "youtube.googleapis.com",
  ])
}

resource "google_project_service" "required" {
  for_each           = local.services
  service            = each.value
  disable_on_destroy = false
}

resource "google_service_account" "deploy" {
  account_id   = "yt-uploader-deploy"
  display_name = "YouTube uploader deployment"
  depends_on   = [google_project_service.required]
}

resource "google_service_account" "runtime" {
  account_id   = "yt-uploader-runtime"
  display_name = "YouTube uploader runtime"
  depends_on   = [google_project_service.required]
}

resource "google_service_account" "worker" {
  account_id   = "yt-uploader-worker"
  display_name = "YouTube uploader background worker"
  depends_on   = [google_project_service.required]
}

resource "google_project_iam_custom_role" "web_video_storage" {
  role_id     = "ytUploaderWebVideoStorage"
  title       = "YouTube uploader web video storage"
  description = "Allows the web service to initiate uploads without reading or deleting staged videos."
  permissions = ["storage.objects.create"]
  stage       = "GA"
}

resource "google_project_iam_custom_role" "web_state_storage" {
  role_id     = "ytUploaderWebStateStorage"
  title       = "YouTube uploader web state storage"
  description = "Allows the web service to read and generation-conditionally replace upload state."
  permissions = ["storage.objects.create", "storage.objects.delete", "storage.objects.get"]
  stage       = "GA"
}

resource "google_project_iam_custom_role" "worker_video_storage" {
  role_id     = "ytUploaderWorkerVideoStorage"
  title       = "YouTube uploader worker video storage"
  description = "Allows the worker to range-read and delete only its staged video objects."
  permissions = ["storage.objects.delete", "storage.objects.get"]
  stage       = "GA"
}

resource "google_project_iam_custom_role" "worker_state_storage" {
  role_id     = "ytUploaderWorkerStateStorage"
  title       = "YouTube uploader worker state storage"
  description = "Allows the worker to read and generation-conditionally replace upload state."
  permissions = ["storage.objects.create", "storage.objects.delete", "storage.objects.get"]
  stage       = "GA"
}

resource "google_iam_workload_identity_pool" "github" {
  workload_identity_pool_id = "yt-uploader-github"
  display_name              = "YouTube uploader GitHub"
  depends_on                = [google_project_service.required]
}

resource "google_iam_workload_identity_pool_provider" "github" {
  workload_identity_pool_id          = google_iam_workload_identity_pool.github.workload_identity_pool_id
  workload_identity_pool_provider_id = "github"
  attribute_mapping = {
    "google.subject"                = "assertion.sub"
    "attribute.repository_id"       = "assertion.repository_id"
    "attribute.repository_owner_id" = "assertion.repository_owner_id"
  }
  attribute_condition = join(" && ", [
    "assertion.repository_id == '${var.github_repository_id}'",
    "assertion.repository_owner_id == '${var.github_owner_id}'",
    "assertion.repository == '${var.github_repository}'",
    "assertion.ref.startsWith('refs/tags/v')",
    "assertion.sub == 'repo:${var.github_repository}:environment:production'",
  ])
  oidc {
    issuer_uri = "https://token.actions.githubusercontent.com"
  }
}

resource "google_service_account_iam_member" "github_deploy" {
  service_account_id = google_service_account.deploy.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "principalSet://iam.googleapis.com/${google_iam_workload_identity_pool.github.name}/attribute.repository_id/${var.github_repository_id}"
}

resource "google_service_account_iam_member" "act_as_runtime" {
  service_account_id = google_service_account.runtime.name
  role               = "roles/iam.serviceAccountUser"
  member             = "serviceAccount:${google_service_account.deploy.email}"
}

resource "google_project_iam_member" "deploy_roles" {
  for_each = toset(["roles/run.developer", "roles/serviceusage.serviceUsageConsumer"])
  project  = var.project_id
  role     = each.value
  member   = "serviceAccount:${google_service_account.deploy.email}"
}

resource "google_artifact_registry_repository" "images" {
  location      = var.region
  repository_id = "yt-uploader"
  format        = "DOCKER"
  docker_config {
    immutable_tags = true
  }
  depends_on = [google_project_service.required]
}

resource "google_artifact_registry_repository_iam_member" "image_writer" {
  location   = var.region
  repository = google_artifact_registry_repository.images.name
  role       = "roles/artifactregistry.writer"
  member     = "serviceAccount:${google_service_account.deploy.email}"
}

resource "google_storage_bucket" "videos" {
  name                        = "${var.project_id}-yt-uploader-videos"
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  force_destroy               = false
  soft_delete_policy {
    retention_duration_seconds = 0
  }
  dynamic "cors" {
    for_each = var.enable_cloud_run ? [1] : []
    content {
      origin          = [regex("^https://[^/]+", var.oauth_redirect_uri)]
      method          = ["PUT"]
      response_header = ["Content-Type", "Content-Range", "Range"]
      max_age_seconds = 3600
    }
  }
  lifecycle_rule {
    condition {
      age = var.temporary_video_days
    }
    action {
      type = "Delete"
    }
  }
  lifecycle {
    prevent_destroy = true
  }
  depends_on = [google_project_service.required]
}

resource "google_storage_bucket" "state" {
  name                        = "${var.project_id}-yt-uploader-state"
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  force_destroy               = false
  soft_delete_policy {
    retention_duration_seconds = 0
  }
  lifecycle_rule {
    condition {
      age = 30
    }
    action {
      type = "Delete"
    }
  }
  lifecycle {
    prevent_destroy = true
  }
  depends_on = [google_project_service.required]
}

resource "google_storage_bucket_iam_member" "runtime_objects" {
  for_each = {
    videos = {
      bucket = google_storage_bucket.videos.name
      role   = google_project_iam_custom_role.web_video_storage.name
    }
    state = {
      bucket = google_storage_bucket.state.name
      role   = google_project_iam_custom_role.web_state_storage.name
    }
  }
  bucket = each.value.bucket
  role   = each.value.role
  member = "serviceAccount:${google_service_account.runtime.email}"
}

resource "google_storage_bucket_iam_member" "worker_objects" {
  for_each = {
    videos = {
      bucket = google_storage_bucket.videos.name
      role   = google_project_iam_custom_role.worker_video_storage.name
    }
    state = {
      bucket = google_storage_bucket.state.name
      role   = google_project_iam_custom_role.worker_state_storage.name
    }
  }
  bucket = each.value.bucket
  role   = each.value.role
  member = "serviceAccount:${google_service_account.worker.email}"
}

resource "google_secret_manager_secret" "oauth" {
  secret_id = "yt-uploader-oauth"
  replication {
    auto {}
  }
  lifecycle {
    prevent_destroy = true
  }
  depends_on = [google_project_service.required]
}

resource "google_secret_manager_secret" "oauth_client" {
  secret_id = "yt-uploader-oauth-client"
  replication {
    auto {}
  }
  lifecycle {
    prevent_destroy = true
  }
  depends_on = [google_project_service.required]
}

resource "google_secret_manager_secret_iam_member" "runtime_oauth" {
  for_each  = toset(["roles/secretmanager.secretAccessor", "roles/secretmanager.secretVersionAdder"])
  secret_id = google_secret_manager_secret.oauth.id
  role      = each.value
  member    = "serviceAccount:${google_service_account.runtime.email}"
}

resource "google_secret_manager_secret_iam_member" "runtime_oauth_client" {
  secret_id = google_secret_manager_secret.oauth_client.id
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.runtime.email}"
}

resource "google_secret_manager_secret_iam_member" "worker_oauth" {
  for_each = {
    oauth        = google_secret_manager_secret.oauth.id
    oauth_client = google_secret_manager_secret.oauth_client.id
  }
  secret_id = each.value
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.worker.email}"
}

resource "google_cloud_run_v2_service" "app" {
  count               = var.enable_cloud_run ? 1 : 0
  name                = "yt-uploader"
  location            = var.region
  deletion_protection = true
  template {
    service_account = google_service_account.runtime.email
    scaling {
      min_instance_count = 0
      max_instance_count = 1
    }
    containers {
      image = var.cloud_run_image
      args  = ["serve"]
      ports {
        container_port = 8080
      }
      resources {
        limits = {
          cpu    = "1"
          memory = "512Mi"
        }
        cpu_idle = true
      }
      env {
        name  = "VIDEO_BUCKET"
        value = google_storage_bucket.videos.name
      }
      env {
        name  = "STATE_BUCKET"
        value = google_storage_bucket.state.name
      }
      env {
        name  = "OAUTH_SECRET_RESOURCE"
        value = google_secret_manager_secret.oauth.id
      }
      env {
        name  = "OAUTH_CLIENT_CONFIG_RESOURCE"
        value = google_secret_manager_secret.oauth_client.id
      }
      env {
        name  = "OAUTH_REDIRECT_URI"
        value = var.oauth_redirect_uri
      }
      env {
        name  = "OAUTH_ALLOWED_EMAILS"
        value = join(",", var.oauth_allowed_emails)
      }
      env {
        name  = "YOUTUBE_AUDIT_CONFIRMED"
        value = tostring(var.youtube_audit_confirmed)
      }
      env {
        name  = "CLOUD_RUN_JOB_RESOURCE"
        value = "projects/${var.project_id}/locations/${var.region}/jobs/yt-uploader-worker"
      }
    }
  }
  lifecycle {
    ignore_changes = [template[0].containers[0].image]
  }
  depends_on = [google_project_service.required]
}

resource "google_project_iam_custom_role" "upload_job_runner" {
  role_id     = "ytUploaderJobRunner"
  title       = "YouTube uploader Job runner"
  description = "Allows the trusted web runtime to run the fixed uploader Job with per-execution overrides."
  permissions = ["run.jobs.run", "run.jobs.runWithOverrides"]
  stage       = "GA"
}

resource "google_cloud_run_v2_job" "upload" {
  count               = var.enable_cloud_run ? 1 : 0
  name                = "yt-uploader-worker"
  location            = var.region
  deletion_protection = true
  template {
    parallelism = 1
    task_count  = 1
    template {
      service_account = google_service_account.worker.email
      max_retries     = 2
      timeout         = "86400s"
      containers {
        name  = "worker"
        image = var.cloud_run_image
        args  = ["worker"]
        resources {
          limits = {
            cpu    = "1"
            memory = "1Gi"
          }
        }
        env {
          name  = "VIDEO_BUCKET"
          value = google_storage_bucket.videos.name
        }
        env {
          name  = "STATE_BUCKET"
          value = google_storage_bucket.state.name
        }
        env {
          name  = "OAUTH_SECRET_RESOURCE"
          value = google_secret_manager_secret.oauth.id
        }
        env {
          name  = "OAUTH_CLIENT_CONFIG_RESOURCE"
          value = google_secret_manager_secret.oauth_client.id
        }
        env {
          name  = "OAUTH_REDIRECT_URI"
          value = var.oauth_redirect_uri
        }
        env {
          name  = "OAUTH_ALLOWED_EMAILS"
          value = join(",", var.oauth_allowed_emails)
        }
        env {
          name  = "YOUTUBE_AUDIT_CONFIRMED"
          value = tostring(var.youtube_audit_confirmed)
        }
      }
    }
  }
  lifecycle {
    ignore_changes = [template[0].template[0].containers[0].image]
  }
  depends_on = [google_project_service.required]
}

resource "google_cloud_run_v2_job_iam_member" "web_runner" {
  count    = var.enable_cloud_run ? 1 : 0
  location = google_cloud_run_v2_job.upload[0].location
  name     = google_cloud_run_v2_job.upload[0].name
  role     = google_project_iam_custom_role.upload_job_runner.name
  member   = "serviceAccount:${google_service_account.runtime.email}"
}

resource "google_cloud_run_v2_service_iam_member" "public_oauth_entry" {
  count    = var.enable_cloud_run ? 1 : 0
  location = google_cloud_run_v2_service.app[0].location
  name     = google_cloud_run_v2_service.app[0].name
  role     = "roles/run.invoker"
  member   = "allUsers"
}