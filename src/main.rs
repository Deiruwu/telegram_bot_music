use serde::{Deserialize, Serialize};
use std::time::Duration;
use teloxide::{prelude::*, types::InputFile};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command; // <-- Para ejecutar FFmpeg sin bloquear el hilo

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

async fn ensure_cached(query: &str) -> anyhow::Result<TrackResult> {
    let track = resolve_track(query).await?;
    if track.state.as_deref() == Some("partial") {
        let track2 = resolve_track(&track.id).await?;
        return Ok(track2);
    }
    Ok(track)
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

    match ensure_cached(query).await {
        Ok(track) => {
            let file_path = match track.file_path {
                Some(ref p) => p,
                None => {
                    bot.edit_message_text(chat_id, status_msg.id, "Track could not be downloaded.")
                        .await?;
                    return Ok(());
                }
            };

            let album_str = track
                .album
                .as_ref()
                .map(|a| a.name.as_str())
                .unwrap_or("Unknown");

            let artists_str = artist_names(&track.artists);

            let caption = format!(
                "{}\n{} - {}\n{}",
                track.title,
                artists_str,
                album_str,
                format_duration(track.duration_seconds)
            );

            // 1. Fase de Conversión
            bot.edit_message_text(chat_id, status_msg.id, "Procesando audio (FLAC -> OPUS)...").await?;
            let opus_tmp_path = format!("/tmp/{}.opus", track.id);

            let ffmpeg_output = Command::new("ffmpeg")
                .arg("-y") // Sobrescribir sin preguntar
                .arg("-i")
                .arg(file_path)
                .arg("-c:a")
                .arg("libopus")
                .arg("-b:a")
                .arg("128k")
                .arg(&opus_tmp_path)
                .output() // .output() es asíncrono y espera a que el proceso termine
                .await?;

            if !ffmpeg_output.status.success() {
                let err_msg = String::from_utf8_lossy(&ffmpeg_output.stderr);
                log::error!("FFmpeg error: {}", err_msg);
                bot.edit_message_text(chat_id, status_msg.id, "Error interno: Falló la conversión a Opus.").await?;
                return Ok(());
            }

            // 2. Fase de Subida
            bot.edit_message_text(chat_id, status_msg.id, "Subiendo a Telegram...").await?;
            let path = std::path::PathBuf::from(&opus_tmp_path);

            let send_result = bot
                .send_audio(chat_id, InputFile::file(path))
                .caption(caption)
                // Inyección estricta de metadatos vía API (Ignora las tags internas del archivo)
                .title(track.title.clone())
                .performer(artists_str)
                .duration(track.duration_seconds as u32)
                .await;

            // 3. Limpieza: Eliminamos el Opus temporal de la RAM/Disco sin importar si la subida falló o no
            if let Err(e) = tokio::fs::remove_file(&opus_tmp_path).await {
                log::warn!("No se pudo eliminar el archivo temporal {}: {}", opus_tmp_path, e);
            }

            // 4. Resolución final del estado
            match send_result {
                Ok(_) => {
                    bot.delete_message(chat_id, status_msg.id).await.ok();
                }
                Err(e) => {
                    bot.edit_message_text(chat_id, status_msg.id, format!("Failed to send file: {e}")).await?;
                }
            }
        }
        Err(e) => {
            bot.edit_message_text(chat_id, status_msg.id, format!("Error: {e}")).await?;
        }
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
                        "Commands:\n/play <name or YouTube ID> - send a track\n/info <name or YouTube ID> - show info without downloading\n\nOr just type a song name directly.",
                    )
                        .await?;
                }

                "/play" => {
                    if args.is_empty() {
                        bot.send_message(msg.chat.id, "Usage: /play <song name or YouTube ID>")
                            .await?;
                        return respond(());
                    }
                    send_track(&bot, msg.chat.id, &args).await.ok();
                }

                "/info" => {
                    if args.is_empty() {
                        bot.send_message(msg.chat.id, "Usage: /info <song name or YouTube ID>")
                            .await?;
                        return respond(());
                    }

                    match resolve_track(&args).await {
                        Ok(track) => {
                            let album_str = track
                                .album
                                .as_ref()
                                .map(|a| a.name.as_str())
                                .unwrap_or("Unknown");
                            let info = format!(
                                "Title: {}\nArtist: {}\nAlbum: {}\nDuration: {}\nState: {:?}",
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
                    bot.send_message(msg.chat.id, "Unknown command. Use /help.")
                        .await?;
                }
            }
        } else {
            send_track(&bot, msg.chat.id, &text).await.ok();
        }

        respond(())
    })
        .await;
}