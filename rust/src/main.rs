use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::path::PathBuf;
use yt_uploader_rs::youtube::{UploadOptions, YouTube};

#[derive(Parser)]
#[command(version, about = "YouTube API management without Python")]
struct Args {
    #[arg(long, env = "YOUTUBE_TOKEN_FILE", global = true)]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Inventory,
    Playlists,
    Video {
        video_id: String,
    },
    PlaylistCreate {
        title: String,
    },
    PlaylistAdd {
        playlist_id: String,
        video_id: String,
    },
    SetPrivacy {
        video_id: String,
        #[arg(value_enum)]
        privacy: Privacy,
    },
    SetPlaylistPrivacy {
        playlist_id: String,
        #[arg(value_enum)]
        privacy: Privacy,
    },
    Upload {
        file: PathBuf,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        #[arg(long, value_enum, default_value = "private")]
        privacy: Privacy,
        #[arg(long, value_enum)]
        audience: Audience,
        #[arg(long, env = "YOUTUBE_UPLOAD_STATE_DIR")]
        state_dir: Option<PathBuf>,
        #[arg(long, env = "YOUTUBE_AUDIT_CONFIRMED", default_value_t = false)]
        audit_confirmed: bool,
    },
}

#[derive(Clone, ValueEnum)]
enum Audience {
    Kids,
    NotKids,
}

#[derive(Clone, ValueEnum)]
enum Privacy {
    Private,
    Unlisted,
    Public,
}

impl Privacy {
    fn as_str(&self) -> &str {
        match self {
            Self::Private => "private",
            Self::Unlisted => "unlisted",
            Self::Public => "public",
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let token_path = match args.token_file {
        Some(path) => path,
        None => PathBuf::from(
            std::env::var_os("HOME").context("Specify --token-file when HOME is unavailable")?,
        )
        .join(".config/yt-uploader/token.json"),
    };
    let mut youtube = YouTube::from_token_file(&token_path)?;
    let result = match args.command {
        Command::Inventory => youtube.inventory()?,
        Command::Playlists => json!(youtube.playlists()?),
        Command::Video { video_id } => youtube.video(&video_id)?,
        Command::PlaylistCreate { title } => youtube.create_playlist(&title)?,
        Command::PlaylistAdd {
            playlist_id,
            video_id,
        } => json!({"added":youtube.add_to_playlist(&playlist_id, &video_id)?}),
        Command::SetPrivacy { video_id, privacy } => {
            youtube.set_privacy(&video_id, privacy.as_str())?
        }
        Command::SetPlaylistPrivacy {
            playlist_id,
            privacy,
        } => youtube.set_playlist_privacy(&playlist_id, privacy.as_str())?,
        Command::Upload {
            file,
            title,
            description,
            privacy,
            audience,
            state_dir,
            audit_confirmed,
        } => {
            let directory = match state_dir {
                Some(path) => path,
                None => PathBuf::from(
                    std::env::var_os("HOME")
                        .context("Specify --state-dir when HOME is unavailable")?,
                )
                .join(".local/share/yt-uploader/rust-uploads"),
            };
            let options = UploadOptions {
                title,
                description,
                privacy: privacy.as_str().into(),
                made_for_kids: matches!(audience, Audience::Kids),
            };
            youtube.upload(&file, options, &directory, audit_confirmed, 8 * 1024 * 1024)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
