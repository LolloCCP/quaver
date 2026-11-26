//! Tells Soloist what to play the way Spotify's own apps do, so that they see the same list, song
//! and queue, and can go on with it. Soloist's own API can't start a list at a song, nor take
//! something out of the queue.

use std::path::Path;

use librespot_core::{
    Error, Session,
    dealer::protocol::TransferOptions,
    spclient::TransferRequest,
};
use serde_json::{Value, json};

pub struct Connect {
    session: Session,
    soloist: String,
}

impl Connect {
    /// Soloist has to have been started before: its name in Spotify Connect is in its folder.
    pub fn new(session: Session, state: &Path) -> Self {
        let soloist = std::fs::read_to_string(state.join(".device_id")).unwrap_or_default().trim().to_owned();
        Self { session, soloist }
    }

    async fn send(&self, device: &str, command: Value) -> Result<(), Error> {
        let path = format!("/connect-state/v1/player/command/from/{}/to/{device}", self.session.device_id());
        let body = json!({ "command": command }).to_string();
        self.session.spclient().request_as_json(&http::Method::POST, &path, None, Some(&body)).await.map(drop)
    }

    /// What plays where.
    async fn state(&self) -> Result<Value, Error> {
        let json = self.session.spclient().request_as_json(&http::Method::GET, "/connect-state/v1/cluster", None, None).await?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// Plays a list from one of its songs, or from its start.
    pub async fn play(&self, context: &str, track: &str) -> Result<(), Error> {
        self.send(&self.soloist, play(json!({ "uri": context, "url": format!("context://{context}") }), track)).await
    }

    /// Plays songs that are no list of Spotify's.
    pub async fn play_songs(&self, songs: &[String], track: &str) -> Result<(), Error> {
        let songs: Vec<_> = songs.iter().map(|uri| json!({ "uri": uri })).collect();
        self.send(&self.soloist, play(json!({ "pages": [{ "tracks": songs }] }), track)).await
    }

    /// Goes on here with what plays, or last played, on another device.
    pub async fn take_over(&self) -> Result<(), Error> {
        let transfer_options = TransferOptions { restore_paused: Some("resume".into()), ..Default::default() };
        let from = self.session.device_id().to_owned();
        self.session.spclient().transfer(&from, &self.soloist, Some(&TransferRequest { transfer_options })).await.map(drop)
    }

    /// Plays a song that is to come, or else just that song.
    pub async fn jump(&self, uri: &str) -> Result<(), Error> {
        match next(&self.state().await?).iter().find(|t| t["uri"] == uri) {
            Some(track) => self.send(&self.soloist, json!({ "endpoint": "skip_next", "track": track, "logging_params": {} })).await,
            None => self.play_songs(&[uri.to_owned()], uri).await,
        }
    }

    pub async fn clear_queue(&self) -> Result<(), Error> {
        let state = self.state().await?;
        // The queue is wherever the music is.
        let device = state["active_device_id"].as_str().filter(|d| !d.is_empty()).unwrap_or(&self.soloist);
        self.send(device, without_queue(&state)).await
    }
}

fn next(state: &Value) -> &[Value] {
    state["player_state"]["next_tracks"].as_array().map_or(&[], Vec::as_slice)
}

fn play(context: Value, track: &str) -> Value {
    let mut options = json!({});
    if !track.is_empty() {
        options["skip_to"] = json!({ "track_uri": track });
    }
    json!({ "endpoint": "play", "context": context, "play_origin": { "feature_identifier": "rustbeat" }, "options": options, "logging_params": {} })
}

fn without_queue(state: &Value) -> Value {
    let player = &state["player_state"];
    let kept: Vec<_> = next(state).iter().filter(|t| t["provider"] != "queue").collect();
    json!({ "endpoint": "set_queue", "next_tracks": kept, "prev_tracks": player["prev_tracks"], "queue_revision": player["queue_revision"], "logging_params": {} })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_starts_at_a_song_or_at_its_start() {
        let command = play(json!({ "uri": "list" }), "song");
        assert_eq!((&command["context"]["uri"], &command["options"]["skip_to"]["track_uri"]), (&json!("list"), &json!("song")));
        assert_eq!(play(json!({ "uri": "list" }), "")["options"], json!({}));
    }

    #[test]
    fn clearing_the_queue_keeps_the_list() {
        let track = |uri: &str, provider: &str| json!({ "uri": uri, "uid": uri, "provider": provider });
        let state = json!({ "player_state": { "queue_revision": "7", "prev_tracks": [track("p", "context")],
            "next_tracks": [track("q", "queue"), track("a", "context"), track("r", "queue"), track("x", "autoplay")] } });
        let command = without_queue(&state);
        assert_eq!(command["next_tracks"], json!([track("a", "context"), track("x", "autoplay")]));
        assert_eq!((&command["prev_tracks"], &command["queue_revision"]), (&json!([track("p", "context")]), &json!("7")));
        assert!(next(&json!({})).is_empty());
    }
}
