variable "project_id" {
  type        = string
  description = "Existing dedicated GCP project; billing must already be configured."
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{4,28}[a-z0-9]$", var.project_id))
    error_message = "Specify a valid Google Cloud project ID."
  }
}

variable "region" {
  type    = string
  default = "asia-northeast1"
}

variable "github_repository" {
  type    = string
  default = "hender14/youtube-bulk-uploader"
  validation {
    condition     = can(regex("^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$", var.github_repository))
    error_message = "Use owner/repository syntax."
  }
}

variable "github_repository_id" {
  type        = string
  description = "Numeric GitHub repository ID, not the repository name."
  validation {
    condition     = can(regex("^[0-9]+$", var.github_repository_id))
    error_message = "Repository ID must be numeric."
  }
}

variable "github_owner_id" {
  type        = string
  description = "Numeric GitHub owner ID, not the owner name."
  validation {
    condition     = can(regex("^[0-9]+$", var.github_owner_id))
    error_message = "Owner ID must be numeric."
  }
}

variable "enable_cloud_run" {
  type        = bool
  default     = false
  description = "Enable only once a reviewed image implements the serve command."
}

variable "cloud_run_image" {
  type        = string
  default     = null
  description = "Reviewed container image pinned to a SHA-256 digest."
  validation {
    condition     = !var.enable_cloud_run || can(regex("@sha256:[0-9a-f]{64}$", var.cloud_run_image))
    error_message = "Enabling Cloud Run requires a digest-pinned web image."
  }
}

variable "temporary_video_days" {
  type        = number
  default     = 7
  description = "Fallback cleanup age for abandoned temporary videos; completed uploads must be deleted by the application."
  validation {
    condition     = var.temporary_video_days >= 1 && floor(var.temporary_video_days) == var.temporary_video_days
    error_message = "Cleanup age must be a positive whole number of days."
  }
}