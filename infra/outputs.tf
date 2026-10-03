output "github_production_variables" {
  value = {
    GCP_PROJECT_ID              = var.project_id
    GCP_REGION                  = var.region
    GCP_ARTIFACT_REPOSITORY     = google_artifact_registry_repository.images.repository_id
    GCP_RUN_SERVICE             = "yt-uploader"
    GCP_RUNTIME_SERVICE_ACCOUNT = google_service_account.runtime.email
    GCP_DEPLOY_SERVICE_ACCOUNT  = google_service_account.deploy.email
    GCP_WIF_PROVIDER            = google_iam_workload_identity_pool_provider.github.name
  }
}

output "application_resources" {
  value = {
    video_bucket        = google_storage_bucket.videos.name
    state_bucket        = google_storage_bucket.state.name
    oauth_secret        = google_secret_manager_secret.oauth.id
    oauth_client_secret = google_secret_manager_secret.oauth_client.id
    service_url         = try(google_cloud_run_v2_service.app[0].uri, null)
  }
}