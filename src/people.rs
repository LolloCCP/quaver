//! Profiles: whom one follows, their public playlists, and what friends listen to.

use futures_util::{StreamExt, stream};
use librespot_core::{Error, Session, SpotifyUri};
use librespot_metadata::{Album, Artist, Metadata, image::Image};
use serde_json::{Value, json};

use crate::backend::{Card, Shelf};

/// How a card says that it opens a profile. The window looks for it too.
pub const PROFILE: &str = "Profile";
/// The card on an artist's page that opens their songs, instead of the page again.
pub const POPULAR: &str = "Popular songs";
/// Releases per shelf of an artist's page; each is a request of its own.
const PER_SHELF: usize = 30;

pub struct Profile {
    pub title: String,
    /// Whether the account follows this one; nothing for its own.
    pub followed: Option<bool>,
    pub shelves: Vec<Shelf>,
    /// A few lines about an artist.
    pub about: String,
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
    if uri.starts_with("spotify:artist:") {
        return artist(session, uri).await;
    }
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
    Ok(Profile { title, followed, shelves, about: String::new() })
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

/// An artist's page: their popular songs, their albums and their singles.
async fn artist(session: &Session, uri: &str) -> Result<Profile, Error> {
    let artist = Artist::get(session, &SpotifyUri::from_uri(uri)?).await?;
    let albums: Vec<_> = artist.albums_current().cloned().collect();
    let singles: Vec<_> = artist.singles_current().cloned().collect();
    // Artists' own pictures often aren't served; their first album's cover stands in.
    let mut albums = records(session, albums).await;
    let picture = medium(&artist.portraits).or_else(|| albums.first().map(|a| a.picture.clone())).unwrap_or_default();
    let mut shelves = vec![Shelf { title: "Popular".into(), cards: vec![Card { title: POPULAR.into(), subtitle: artist.name.clone(), uri: uri.to_owned(), picture }] }];
    shelves.push(Shelf { title: "Albums".into(), cards: std::mem::take(&mut albums) });
    shelves.push(Shelf { title: "Singles and EPs".into(), cards: records(session, singles).await });
    shelves.retain(|s| !s.cards.is_empty());
    let about = artist.biographies.first().map(|b| plain(&b.text, 420)).unwrap_or_default();
    Ok(Profile { title: artist.name, followed: None, shelves, about })
}

/// Albums as cards, newest first as Spotify lists them; what can't be loaded is left out.
async fn records(session: &Session, uris: Vec<SpotifyUri>) -> Vec<Card> {
    let load = |uri: SpotifyUri| async move {
        let album = Album::get(session, &uri).await.inspect_err(|e| log::warn!("album: {e}")).ok()?;
        let year = album.date.year();
        let picture = medium(&album.covers).or_else(|| medium(&album.cover_group)).unwrap_or_default();
        Some(Card { title: album.name, subtitle: if year > 1 { year.to_string() } else { String::new() }, uri: uri.to_uri(), picture })
    };
    let found: Vec<Option<Card>> = stream::iter(uris.into_iter().take(PER_SHELF)).map(load).buffered(6).collect().await;
    found.into_iter().flatten().collect()
}

/// The picture closest to a card's size, as an image id.
fn medium(images: &[Image]) -> Option<String> {
    images.iter().min_by_key(|i| (i.width - 300).abs()).map(|i| i.id.to_base16())
}

/// Biographies come with links and HTML entities in them; this keeps the words, cut at `limit` characters.
fn plain(html: &str, limit: usize) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = [("&amp;", "&"), ("&quot;", "\""), ("&#39;", "'"), ("&apos;", "'"), ("&lt;", "<"), ("&gt;", ">"), ("&nbsp;", " ")]
        .iter()
        .fold(text, |t, (entity, c)| t.replace(entity, c));
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let cut: String = text.chars().take(limit).collect();
    format!("{}…", cut.rsplit_once(' ').map_or(cut.as_str(), |(words, _)| words).trim_end_matches([',', '.', ';', ':']))
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
    fn biographies_lose_their_markup_and_stop_at_a_word() {
        let bio = "<a href=\"spotify:artist:1\">Ana &amp; Bo</a> made   records.\nThen more records";
        assert_eq!(plain(bio, 100), "Ana & Bo made records. Then more records");
        assert_eq!(plain(bio, 24), "Ana & Bo made records…");
        assert_eq!(plain("", 10), "");
    }

    #[tokio::test]
    #[ignore = "needs the network and a saved login; prints counts only"]
    async fn artist_page() {
        use librespot_core::{SessionConfig, cache::Cache};
        let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let login = Cache::new(Some(&dir), None, None, None).unwrap().credentials().expect("not signed in");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(login, false).await.unwrap();
        let profile = load(&session, "spotify:artist:4tZwfgrHOc3mvqYlEYSvVi", vec![]).await.unwrap();
        println!("{}: about {} chars", profile.title, profile.about.chars().count());
        for shelf in &profile.shelves {
            println!("{}: {} cards, {} with pictures, first {:?}", shelf.title, shelf.cards.len(), shelf.cards.iter().filter(|c| !c.picture.is_empty()).count(), shelf.cards.first().map(|c| (&c.title, &c.subtitle)));
        }
        assert!(profile.shelves.len() == 3);
    }

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
