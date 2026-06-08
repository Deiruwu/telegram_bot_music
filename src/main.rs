use serde::{Deserialize, Serialize};
use std::time::Duration;
use teloxide::{prelude::*, types::InputFile};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

const MUSIC_HOST: &str = "10.0.0.5";
const MUSIC_PORT: u16 = 7878;

#[derive(Serialize)]
struct MusicRequest {
    action: String,
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct Album {
    #[allow(dead_code)]
    id: String,
    name: String,
}

#[derive(Deserialize, Debug)]
struct Artist {
    #[allow(dead_code)]
    id: String,
    name: String,
}

#[derive(Deserialize, Debug)]
struct TrackResult {
    state: Option<String>,
    id: String,
    title: String,
    duration_seconds: u64,
    #[allow(dead_code)]
    thumbnail_url: Option<String>,
    file_path: Option<String>,
    album: Option<Album>,
    artists: Vec<Artist>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum MusicData {
    Single(TrackResult),
    Multiple(Vec<TrackResult>),
    Ids(Vec<String>),
    Ok { message: String },
}

#[derive(Deserialize, Debug)]
struct MusicResponse {
    status: String,
    data: Option<MusicData>,
    #[allow(dead_code)]
    message: Option<String>,
}

async fn music_request(req: &MusicRequest) -> anyhow::Result<MusicResponse> {
    let mut stream = TcpStream::connect((MUSIC_HOST, MUSIC_PORT)).await?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');

    stream.write_all(line.as_bytes()).await?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;

    let raw_json = response_line.trim();
    log::info!("Raw response: {}", raw_json);

    let response: MusicResponse = serde_json::from_str(raw_json)
        .map_err(|e| anyhow::anyhow!("Serde error: {} | Raw string: {}", e, raw_json))?;

    Ok(response)
}

async fn resolve_track(query: &str) -> anyhow::Result<TrackResult> {
    let req = MusicRequest {
        action: "resolve".to_string(),
        query: query.to_string(),
        limit: None,
    };

    let resp = music_request(&req).await?;
    if resp.status != "ok" {
        return Err(anyhow::anyhow!(
            resp.message.unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    match resp.data {
        Some(MusicData::Single(track)) => Ok(track),
        _ => Err(anyhow::anyhow!("unexpected response shape")),
    }
}

async fn download_track(id: &str) -> anyhow::Result<TrackResult> {
    let req = MusicRequest {
        action: "download".to_string(),
        query: id.to_string(),
        limit: None,
    };

    let resp = music_request(&req).await?;
    if resp.status != "ok" {
        return Err(anyhow::anyhow!(
            resp.message.unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    match resp.data {
        Some(MusicData::Single(track)) => Ok(track),
        _ => Err(anyhow::anyhow!("unexpected response shape")),
    }
}

fn format_duration(seconds: u64) -> String {
    let m = seconds / 60;
    let s = seconds % 60;
    format!("{m}:{s:02}")
}

fn artist_names(artists: &[Artist]) -> String {
    artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

async fn send_track(bot: &AutoSend<Bot>, chat_id: ChatId, query: &str) -> anyhow::Result<()> {
    let status_msg = bot.send_message(chat_id, "Resolviendo track...").await?;

    // 1. Resolve para metadata
    let track = match resolve_track(query).await {
        Ok(t) => t,
        Err(e) => {
            bot.edit_message_text(chat_id, status_msg.id, format!("Error: {e}")).await?;
            return Ok(());
        }
    };

    // 2. Download si no tiene file_path
    let track = if track.file_path.is_none() {
        bot.edit_message_text(
            chat_id,
            status_msg.id,
            format!("Descargando: {}...", track.title),
        ).await?;

        match download_track(&track.id).await {
            Ok(t) => t,
            Err(e) => {
                bot.edit_message_text(chat_id, status_msg.id, format!("Error al descargar: {e}")).await?;
                return Ok(());
            }
        }
    } else {
        track
    };

    let file_path = match track.file_path.as_ref() {
        Some(p) => p,
        None => {
            bot.edit_message_text(chat_id, status_msg.id, "No se pudo obtener el archivo.").await?;
            return Ok(());
        }
    };

    // 3. Preparar audio: limpiar tag ENCODER que causa el bug de voice note
    bot.edit_message_text(chat_id, status_msg.id, "Preparando audio...").await?;
    let tmp_path = format!("/tmp/{}.opus", track.id);

    let (audio_path, needs_cleanup) = if file_path.ends_with(".flac") {
        let out = Command::new("ffmpeg")
            .args(["-y", "-i", file_path, "-c:a", "libopus", "-b:a", "128k", &tmp_path])
            .output().await?;

        if !out.status.success() {
            log::error!("FFmpeg error: {}", String::from_utf8_lossy(&out.stderr));
            bot.edit_message_text(chat_id, status_msg.id, "Error: fallo la conversion.").await?;
            return Ok(());
        }
        (tmp_path, true)
    } else {
        let out = Command::new("ffmpeg")
            .args([
                "-y", "-i", file_path,
                "-c", "copy",
                "-map_metadata", "-1",
                "-metadata", &format!("title={}", track.title),
                "-metadata", &format!("artist={}", artist_names(&track.artists)),
                "-metadata", &format!("album={}", track.album.as_ref().map(|a| a.name.as_str()).unwrap_or("")),
                &tmp_path,
            ])
            .output().await?;

        if !out.status.success() {
            log::error!("FFmpeg tag error: {}", String::from_utf8_lossy(&out.stderr));
            (file_path.clone(), false)
        } else {
            (tmp_path, true)
        }
    };

    // 4. Caratula directo desde la URL, sin descargar
    let thumb_input = track.thumbnail_url.as_ref()
        .map(|url| InputFile::url(url.parse().unwrap()));

    // 5. Subida
    bot.edit_message_text(chat_id, status_msg.id, "Subiendo a Telegram...").await?;

    let album_str = track.album.as_ref().map(|a| a.name.as_str()).unwrap_or("Unknown");
    let artists_str = artist_names(&track.artists);
    let caption = format!(
        "{}\n{} — {}\n{}",
        track.title, artists_str, album_str,
        format_duration(track.duration_seconds)
    );

    let mut req = bot
        .send_audio(chat_id, InputFile::file(std::path::PathBuf::from(&audio_path)))
        .caption(caption)
        .title(track.title.clone())
        .performer(artists_str)
        .duration(track.duration_seconds as u32);

    if let Some(thumb) = thumb_input {
        req = req.thumb(thumb);
    }

    let result = req.await;

    if needs_cleanup {
        tokio::fs::remove_file(&audio_path).await.ok();
    }

    match result {
        Ok(_) => { bot.delete_message(chat_id, status_msg.id).await.ok(); }
        Err(e) => { bot.edit_message_text(chat_id, status_msg.id, format!("Error al enviar: {e}")).await?; }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("starting music bot");

    let custom_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("Failed to build reqwest client");

    let token = std::env::var("TELOXIDE_TOKEN").expect("Falta TELOXIDE_TOKEN en .env");
    let bot = Bot::with_client(token, custom_client).auto_send();

    teloxide::repl(bot, |bot: AutoSend<Bot>, msg: Message| async move {
        let text = match msg.text() {
            Some(t) => t.trim().to_string(),
            None => return respond(()),
        };

        if text.starts_with('/') {
            let mut parts = text.splitn(2, ' ');
            let cmd = parts.next().unwrap_or("").to_lowercase();
            let args = parts.next().unwrap_or("").trim().to_string();

            match cmd.as_str() {
                "/start" | "/help" => {
                    bot.send_message(
                        msg.chat.id,
                        "Comandos:\n/play <nombre o ID de YouTube> - enviar un track\n/info <nombre o ID de YouTube> - ver info sin descargar\n\nO escribe el nombre de una cancion directamente.",
                    ).await?;
                }

                "/play" => {
                    if args.is_empty() {
                        bot.send_message(msg.chat.id, "Uso: /play <nombre o ID de YouTube>").await?;
                        return respond(());
                    }
                    send_track(&bot, msg.chat.id, &args).await.ok();
                }

                "/info" => {
                    if args.is_empty() {
                        bot.send_message(msg.chat.id, "Uso: /info <nombre o ID de YouTube>").await?;
                        return respond(());
                    }

                    match resolve_track(&args).await {
                        Ok(track) => {
                            let album_str = track.album.as_ref().map(|a| a.name.as_str()).unwrap_or("Unknown");
                            let info = format!(
                                "Titulo: {}\nArtista: {}\nAlbum: {}\nDuracion: {}\nEstado: {:?}",
                                track.title,
                                artist_names(&track.artists),
                                album_str,
                                format_duration(track.duration_seconds),
                                track.state,
                            );
                            bot.send_message(msg.chat.id, info).await?;
                        }
                        Err(e) => {
                            bot.send_message(msg.chat.id, format!("Error: {e}")).await?;
                        }
                    }
                }

                _ => {
                    bot.send_message(msg.chat.id, "Comando desconocido. Usa /help.").await?;
                }
            }
        } else {
            send_track(&bot, msg.chat.id, &text).await.ok();
        }

        respond(())
    }).await;
}