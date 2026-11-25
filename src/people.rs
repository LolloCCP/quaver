//! Profiles: whom one follows, their public playlists, and what friends listen to.

use librespot_core::{Error, Session};
use serde_json::{Value, json};

use crate::backend::{Card, Shelf};

/// How a card says that it opens a profile. The window looks for it too.
pub const PROFILE: &str = "Profile";

pub struct Profile {
    pub title: String,
    /// Whether the account follows this one; nothing for its own.
    pub followed: Option<bool>,
    pub shelves: Vec<Shelf>,
}

/// The profile a link leads to, as people share it.
pub fn linked(text: &str) -> Option<String> {
    let text = text.trim();
    let id = text.strip_prefix("spotify:user:").or_else(|| {
        let path = text.strip_prefix("https://").unwrap_or(text).strip_prefix("open.spotify.com/")?;
        path.split_once("user/").filter(|(language, _)| language.is_empty() || language.starts_with("intl-")).map(|(_, id)| id)
    })?;
    let id = id.split(['?', '#', '/']).next()?;
    // It becomes part of an address.
    let valid = id.bytes().any(|b| b.is_ascii_alphanumeric()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    valid.then(|| format!("spotify:user:{id}"))
}

async fn get(session: &Session, path: &str) -> Result<Value, Error> {
    let json = session.spclient().request_as_json(&http::Method::GET, path, None, None).await?;
    Ok(serde_json::from_slice(&json)?)
}

/// One's own page shows friends and `shared` playlists, someone else's their public playlists.
pub async fn load(session: &Session, uri: &str, shared: Vec<Card>) -> Result<Profile, Error> {
    let own = uri.is_empty();
    let name = session.username();
    let id = if own { name.as_str() } else { uri.rsplit(':').next().unwrap_or_default() };
    let base = format!("/user-profile-view/v3/profile/{id}");
    let profile = get(session, &format!("{base}?playlist_limit=50&artist_limit=0&market=from_token")).await?;
    let mut shelves = vec![];
    if own {
        let activity = get(session, "/presence-view/v1/buddylist").await;
        shelves.push(Shelf { title: "What friends listen to".into(), cards: part(activity, activity_in) });
        shelves.push(Shelf { title: "Blends and group playlists".into(), cards: shared });
    } else {
        shelves.push(Shelf { title: "Public playlists".into(), cards: playlists_in(&profile) });
    }
    for (title, page) in [("Following", "following"), ("Followers", "followers")] {
        let people = get(session, &format!("{base}/{page}?market=from_token")).await;
        shelves.push(Shelf { title: title.into(), cards: part(people, people_in) });
    }
    // shortcut: a shelf draws all its cards, make it draw only what is seen if people need more.
    shelves.iter_mut().for_each(|s| s.cards.truncate(50));
    shelves.retain(|s| !s.cards.is_empty());
    let title = if own { "Friends".into() } else { text(&profile["name"]) };
    let followed = (!own && profile["is_current_user"] != true).then(|| profile["is_following"] == true);
    Ok(Profile { title, followed, shelves })
}

/// Some people keep some of this to themselves.
fn part(json: Result<Value, Error>, cards: fn(&Value) -> Vec<Card>) -> Vec<Card> {
    json.inspect_err(|e| log::info!("profile: {e}")).map(|json| cards(&json)).unwrap_or_default()
}

pub async fn follow(session: &Session, uri: &str, on: bool) -> Result<(), Error> {
    let method = if on { http::Method::POST } else { http::Method::DELETE };
    let body = json!({ "target_uris": [uri] }).to_string();
    let answer = session.spclient().request_as_json(&method, "/socialgraph/v2/following?format=json", None, Some(&body)).await?;
    match refused(&answer) {
        true => Err(Error::permission_denied("Spotify didn't accept that")),
        false => Ok(()),
    }
}

/// The answer has a status for each account, also when the request itself went well.
fn refused(answer: &[u8]) -> bool {
    let answer: Value = serde_json::from_slice(answer).unwrap_or_default();
    answer.as_array().into_iter().flatten().any(|a| a["status"].as_u64().is_some_and(|s| s >= 300))
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

fn picture(v: &Value) -> String {
    text(v).replacen("http://", "https://", 1)
}

fn playlists_in(profile: &Value) -> Vec<Card> {
    let playlists = profile["public_playlists"].as_array().into_iter().flatten();
    playlists.map(|p| Card { title: text(&p["name"]), subtitle: "Playlist".into(), uri: text(&p["uri"]), picture: picture(&p["image_url"]) }).collect()
}

/// Artists are followed too.
fn people_in(page: &Value) -> Vec<Card> {
    let profiles = page["profiles"].as_array().into_iter().flatten();
    profiles
        .filter_map(|p| {
            let uri = text(&p["uri"]);
            let subtitle = if uri.starts_with("spotify:artist:") { "Artist" } else { linked(&uri).map(|_| PROFILE)? };
            Some(Card { title: text(&p["name"]), subtitle: subtitle.into(), uri, picture: picture(&p["image_url"]) })
        })
        .collect()
}

/// Opens what a friend plays from, or else the album.
fn activity_in(list: &Value) -> Vec<Card> {
    let opens = |uri: &&str| ["playlist", "album", "artist"].iter().any(|k| uri.starts_with(&format!("spotify:{k}:")));
    let friends = list["friends"].as_array().into_iter().flatten();
    friends
        .filter_map(|f| {
            let track = &f["track"];
            let uri = [&track["context"]["uri"], &track["album"]["uri"]].into_iter().filter_map(Value::as_str).find(opens)?;
            let subtitle = format!("{} · {}", text(&track["name"]), text(&track["artist"]["name"]));
            Some(Card { title: text(&f["user"]["name"]), subtitle, uri: uri.to_owned(), picture: picture(&track["imageUrl"]) })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_links_are_told_from_searches() {
        for link in ["https://open.spotify.com/user/some.one-1?si=abc", "open.spotify.com/intl-it/user/some.one-1", " spotify:user:some.one-1 "] {
            assert_eq!(linked(link).as_deref(), Some("spotify:user:some.one-1"), "{link}");
        }
        for other in ["some one", "https://open.spotify.com/playlist/user/x", "https://evil.example/user/x", "spotify:user:", "spotify:user:..", "spotify:user:a:collection", "spotify:user:a%2Fb"] {
            assert_eq!(linked(other), None, "{other}");
        }
    }

    #[test]
    fn people_playlists_and_friends_become_cards() {
        let people = people_in(&json!({ "profiles": [
            { "uri": "spotify:artist:1", "name": "A" },
            { "uri": "spotify:user:u", "name": "U", "image_url": "https://i.scdn.co/x" },
            { "uri": "spotify:show:2", "name": "S" } ] }));
        assert_eq!(people.iter().map(|c| c.subtitle.as_str()).collect::<Vec<_>>(), ["Artist", PROFILE]);
        let lists = playlists_in(&json!({ "public_playlists": [{ "name": "L", "uri": "spotify:playlist:1", "image_url": "spotify:mosaic:a:b" }] }));
        assert_eq!((lists[0].title.as_str(), lists[0].picture.as_str()), ("L", "spotify:mosaic:a:b"));
        let friends = activity_in(&json!({ "friends": [
            { "user": { "name": "F" }, "track": { "name": "T", "imageUrl": "http://i.scdn.co/y", "artist": { "name": "R" }, "album": { "uri": "spotify:album:3" }, "context": { "uri": "spotify:user:f:collection" } } },
            { "user": { "name": "G" }, "track": { "name": "Episode" } } ] }));
        assert_eq!(friends, [Card { title: "F".into(), subtitle: "T · R".into(), uri: "spotify:album:3".into(), picture: "https://i.scdn.co/y".into() }]);
        assert!(people_in(&json!({})).is_empty() && playlists_in(&json!({})).is_empty() && activity_in(&json!({})).is_empty());
    }

    #[test]
    fn a_refused_follow_is_noticed() {
        assert!(!refused(br#"[{"status":200,"user_uri":"spotify:user:u"}]"#) && !refused(b""));
        assert!(refused(br#"[{"status":403,"user_uri":"spotify:user:u"}]"#));
    }
}
