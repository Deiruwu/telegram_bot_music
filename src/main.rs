use std::time::Duration;
use serde::{Deserialize, Serialize};
use teloxide::{prelude::*, types::InputFile};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

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

// 1. Refactor a asíncrono usando tokio::net::TcpStream
async fn music_request(req: &MusicRequest) -> anyhow::Result<MusicResponse> {
    let mut stream = TcpStream::connect((MUSIC_HOST, MUSIC_PORT)).await?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');

    stream.write_all(line.as_bytes()).await?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line).await?;

    let raw_json = response_line.trim();

    // 1. Printeamos el JSON crudo en el log para ver qué está fallando
    log::info!("Raw response: {}", raw_json);

    // 2. Mapeamos el error de Serde para incluir el JSON en el crash,
    // así es más fácil debugear si vuelve a pasar en el futuro.
    let response: MusicResponse = serde_json::from_str(raw_json)
        .map_err(|e| anyhow::anyhow!("Serde error: {} | Raw string: {}", e, raw_json))?;

    Ok(response)
}

// 2. Propagación del async
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

// 3. Propagación del async
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
    let status_msg = bot
        .send_message(chat_id, "Resolving track...")
        .await?;

    // 4. Await en la llamada a la caché
    match ensure_cached(query).await {
        Ok(track) => {
            // 5. Traducción de la ruta por el sshfs
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

            let caption = format!(
                "{}\n{} - {}\n{}",
                track.title,
                artist_names(&track.artists),
                album_str,
                format_duration(track.duration_seconds)
            );

            bot.edit_message_text(chat_id, status_msg.id, "Sending...").await?;

            let path = std::path::PathBuf::from(&file_path);

            match bot
                .send_audio(chat_id, InputFile::file(path))
                .caption(caption)
                .await
            {
                Ok(_) => {
                    bot.delete_message(chat_id, status_msg.id).await.ok();
                }
                Err(e) => {
                    bot.edit_message_text(chat_id, status_msg.id, format!("Failed to send file: {e}"))
                        .await?;
                }
            }
        }
        Err(e) => {
            bot.edit_message_text(chat_id, status_msg.id, format!("Error: {e}"))
                .await?;
        }
    }
    Ok(())
}


#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    log::info!("starting music bot");

    // 1. Creamos un cliente HTTP personalizado con un timeout largo (ej. 5 minutos)
    let custom_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("Failed to build reqwest client");

    // 2. Le pasamos el token y el cliente a Teloxide
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

                    // 6. Await en el comando info
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