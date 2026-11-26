//! Changes to the library: liked songs and one's own playlists.

use std::collections::HashSet;

use http::{HeaderMap, HeaderValue, Method, header::{ACCEPT, CONTENT_TYPE}};
use librespot_core::{Error, Session};
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, CreateListReply, Delta, Item, ItemAttributes, ListAttributes, ListAttributesPartialState, ListChanges, ListUpdateRequest, Op,
    UpdateListAttributes, op::Kind,
};
use protobuf::{Message, MessageField};

/// The collection's messages aren't among librespot's, so they are written and read by hand.
const COLLECTION: &str = "application/vnd.collection-v2.spotify.proto";

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn collection_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(COLLECTION));
    headers.insert(ACCEPT, HeaderValue::from_static(COLLECTION));
    headers
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            return out.push(byte);
        }
        out.push(byte | 0x80);
    }
}

fn bytes_field(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
    varint(out, u64::from(number << 3 | 2));
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn number_field(out: &mut Vec<u8>, number: u32, value: u64) {
    varint(out, u64::from(number << 3));
    varint(out, value);
}

enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

/// The fields of a message, or `None` if it is cut short.
fn fields(mut bytes: &[u8]) -> Option<Vec<(u32, Value<'_>)>> {
    fn read(bytes: &mut &[u8]) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = bytes.split_first()?;
            *bytes = rest;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let tag = read(&mut bytes)?;
        let value = match tag & 7 {
            0 => Value::Number(read(&mut bytes)?),
            2 => {
                let length = read(&mut bytes)? as usize;
                let (value, rest) = (bytes.get(..length)?, bytes.get(length..)?);
                bytes = rest;
                Value::Bytes(value)
            }
            kind @ (1 | 5) => {
                let length = if kind == 1 { 8 } else { 4 };
                let (value, rest) = (bytes.get(..length)?, bytes.get(length..)?);
                bytes = rest;
                Value::Bytes(value)
            }
            _ => return None,
        };
        out.push(((tag >> 3) as u32, value));
    }
    Some(out)
}

/// WriteRequest: liking a song, or taking it back.
fn write_request(user: &str, uri: &str, on: bool, now_s: i64) -> Vec<u8> {
    let mut item = Vec::new();
    bytes_field(&mut item, 1, uri.as_bytes());
    number_field(&mut item, 2, now_s as u64);
    if !on {
        number_field(&mut item, 3, 1);
    }
    let mut out = Vec::new();
    bytes_field(&mut out, 1, user.as_bytes());
    bytes_field(&mut out, 2, b"collection");
    bytes_field(&mut out, 3, &item);
    out
}

/// PageRequest for the liked songs.
fn page_request(user: &str, token: &str, limit: u64) -> Vec<u8> {
    let mut out = Vec::new();
    bytes_field(&mut out, 1, user.as_bytes());
    bytes_field(&mut out, 2, b"collection");
    if !token.is_empty() {
        bytes_field(&mut out, 3, token.as_bytes());
    }
    number_field(&mut out, 4, limit);
    out
}

/// PageResponse: the liked URIs in it, and the token for the next page.
fn page_response(bytes: &[u8]) -> Option<(Vec<String>, String)> {
    let (mut uris, mut next) = (Vec::new(), String::new());
    for (number, value) in fields(bytes)? {
        match (number, value) {
            (1, Value::Bytes(item)) => {
                let (mut uri, mut removed) = (String::new(), false);
                for (number, value) in fields(item)? {
                    match (number, value) {
                        (1, Value::Bytes(v)) => uri = String::from_utf8_lossy(v).into_owned(),
                        (3, Value::Number(v)) => removed = v != 0,
                        _ => {}
                    }
                }
                if !removed {
                    uris.push(uri);
                }
            }
            (2, Value::Bytes(v)) => next = String::from_utf8_lossy(v).into_owned(),
            _ => {}
        }
    }
    Some((uris, next))
}

/// Every liked song.
pub async fn liked(session: &Session) -> Result<HashSet<String>, Error> {
    let user = session.username();
    let (mut liked, mut token) = (HashSet::new(), String::new());
    loop {
        let body = page_request(&user, &token, 2000);
        let answer = session.spclient().request(&Method::POST, "/collection/v2/paging", Some(collection_headers()), Some(&body)).await?;
        let (uris, next) = page_response(&answer).ok_or_else(|| Error::data_loss("the liked songs could not be read"))?;
        liked.extend(uris.into_iter().filter(|u| u.starts_with("spotify:track:")));
        if next.is_empty() || next == token {
            return Ok(liked);
        }
        token = next;
    }
}

pub async fn like(session: &Session, uri: &str, on: bool) -> Result<(), Error> {
    let body = write_request(&session.username(), uri, on, now_ms() / 1000);
    session.spclient().request(&Method::POST, "/collection/v2/write", Some(collection_headers()), Some(&body)).await?;
    Ok(())
}

/// Items added at the end of a list, or at its start.
fn added(user: &str, uris: &[&str], first: bool) -> ListChanges {
    let now = now_ms();
    let mut add = Add::new();
    for uri in uris {
        let mut attributes = ItemAttributes::new();
        attributes.set_timestamp(now);
        let mut item = Item::new();
        item.set_uri((*uri).to_owned());
        item.attributes = MessageField::some(attributes);
        add.items.push(item);
    }
    if first {
        add.set_from_index(0);
        add.set_add_first(true);
    } else {
        add.set_add_last(true);
    }
    let mut op = Op::new();
    op.set_kind(Kind::ADD);
    op.add = MessageField::some(add);
    let mut info = ChangeInfo::new();
    info.set_user(user.to_owned());
    info.set_timestamp(now);
    let mut delta = Delta::new();
    delta.ops.push(op);
    delta.info = MessageField::some(info);
    let mut changes = ListChanges::new();
    changes.deltas.push(delta);
    changes
}

fn renamed(user: &str, name: &str) -> ListChanges {
    let mut attributes = ListAttributes::new();
    attributes.set_name(name.trim().to_owned());
    let mut state = ListAttributesPartialState::new();
    state.values = MessageField::some(attributes);
    let mut update = UpdateListAttributes::new();
    update.new_attributes = MessageField::some(state);
    let mut op = Op::new();
    op.set_kind(Kind::UPDATE_LIST_ATTRIBUTES);
    op.update_list_attributes = MessageField::some(update);
    let mut info = ChangeInfo::new();
    info.set_user(user.to_owned());
    info.set_timestamp(now_ms());
    let mut delta = Delta::new();
    delta.ops.push(op);
    delta.info = MessageField::some(info);
    let mut changes = ListChanges::new();
    changes.deltas.push(delta);
    changes
}

fn playlist_id(uri: &str) -> Option<&str> {
    let id = uri.strip_prefix("spotify:playlist:")?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

pub async fn add_to_playlist(session: &Session, playlist: &str, track: &str) -> Result<(), Error> {
    let id = playlist_id(playlist).ok_or_else(|| Error::invalid_argument("not a playlist"))?;
    let changes = added(&session.username(), &[track], false);
    session.spclient().request_with_protobuf(&Method::POST, &format!("/playlist/v2/playlist/{id}/changes"), None, &changes).await?;
    Ok(())
}

/// Makes a playlist and puts it at the top of the library, as Spotify's apps do. Returns its URI.
pub async fn create_playlist(session: &Session, name: &str) -> Result<String, Error> {
    let user = session.username();
    let mut attributes = ListAttributes::new();
    attributes.set_name(name.trim().to_owned());
    let mut info = ChangeInfo::new();
    info.set_user(user.clone());
    info.set_timestamp(now_ms());
    let mut request = ListUpdateRequest::new();
    request.attributes = MessageField::some(attributes);
    request.info = MessageField::some(info);
    let answer = session.spclient().request_with_protobuf(&Method::POST, "/playlist/v2/playlist", None, &request).await?;
    let uri = CreateListReply::parse_from_bytes(&answer)?.uri().to_owned();
    // The name given when making it isn't kept; it is set as a change of its own.
    let id = playlist_id(&uri).ok_or_else(|| Error::data_loss("Spotify answered with something that isn't a playlist"))?;
    session.spclient().request_with_protobuf(&Method::POST, &format!("/playlist/v2/playlist/{id}/changes"), None, &renamed(&user, name)).await?;
    let changes = added(&user, &[&uri], true);
    session.spclient().request_with_protobuf(&Method::POST, &format!("/playlist/v2/user/{user}/rootlist/changes"), None, &changes).await?;
    Ok(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liking_writes_the_collection_message() {
        let written = write_request("me", "spotify:track:1", false, 300);
        let mut expected = vec![0x0a, 2, b'm', b'e', 0x12, 10];
        expected.extend(b"collection");
        expected.extend([0x1a, 22, 0x0a, 15]);
        expected.extend(b"spotify:track:1");
        expected.extend([0x10, 0xac, 0x02, 0x18, 1]);
        assert_eq!(written, expected);
        assert!(!write_request("me", "u", true, 1).ends_with(&[0x18, 1]));
    }

    #[test]
    fn pages_of_liked_songs_are_read() {
        let mut page = Vec::new();
        for (uri, removed) in [("spotify:track:a", false), ("spotify:track:b", true)] {
            let mut item = Vec::new();
            bytes_field(&mut item, 1, uri.as_bytes());
            number_field(&mut item, 2, 1_700_000_000);
            if removed {
                number_field(&mut item, 3, 1);
            }
            bytes_field(&mut page, 1, &item);
        }
        bytes_field(&mut page, 2, b"next");
        assert_eq!(page_response(&page), Some((vec!["spotify:track:a".to_owned()], "next".to_owned())));
        assert_eq!(page_response(&page[..page.len() - 1]), None, "cut short");
        assert_eq!(page_request("me", "", 5), [0x0a, 2, b'm', b'e', 0x12, 10, b'c', b'o', b'l', b'l', b'e', b'c', b't', b'i', b'o', b'n', 0x20, 5]);
    }

    /// Likes a song that isn't liked and takes it back, and makes a playlist with it and deletes it.
    #[tokio::test]
    #[ignore = "needs the network and a saved login; changes the library and puts it back"]
    async fn library_changes_reach_spotify() {
        use librespot_core::{SessionConfig, SpotifyUri, cache::Cache};
        use librespot_protocol::playlist4_external::{Rem, SelectedListContent};
        let dir = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Rustbeat");
        let login = Cache::new(Some(&dir), None, None, None).unwrap().credentials().expect("not signed in");
        let session = Session::new(SessionConfig::default(), None);
        session.connect(login, false).await.unwrap();
        let before = liked(&session).await.unwrap();
        println!("LIVE liked songs: {}", before.len());
        let track = ["spotify:track:0DiWol3AO6WpXZgp0goxAV", "spotify:track:2Foc5Q5nqNiosCNqttzHof", "spotify:track:7oOOI85fVQvVnK5ynNMdW7"]
            .into_iter()
            .find(|t| !before.contains(*t))
            .expect("all test songs are already liked");
        like(&session, track, true).await.unwrap();
        let during = liked(&session).await.unwrap();
        println!("LIVE liked after like: {} has it {}", during.len(), during.contains(track));
        like(&session, track, false).await.unwrap();
        let after = liked(&session).await.unwrap();
        println!("LIVE liked after unlike: {} has it {}", after.len(), after.contains(track));
        assert!(during.contains(track) && !after.contains(track));

        let uri = create_playlist(&session, "Rustbeat test").await.unwrap();
        println!("LIVE created {uri}");
        add_to_playlist(&session, &uri, track).await.unwrap();
        let id = SpotifyUri::from_uri(&uri).unwrap();
        let list = SelectedListContent::parse_from_bytes(&session.spclient().get_playlist(&match id { SpotifyUri::Playlist { id, .. } => id, _ => unreachable!() }).await.unwrap()).unwrap();
        println!("LIVE playlist {:?} has {} songs: {:?}", list.attributes.name(), list.length(), list.contents.items.iter().map(|i| i.uri()).collect::<Vec<_>>());
        // Delete: take it out of the library again.
        let root = SelectedListContent::parse_from_bytes(&session.spclient().get_rootlist(0, None).await.unwrap()).unwrap();
        let index = root.contents.items.iter().position(|i| i.uri() == uri).expect("not in the library");
        println!("LIVE in library at {index}");
        let mut item = Item::new();
        item.set_uri(uri.clone());
        let mut rem = Rem::new();
        rem.set_from_index(index as i32);
        rem.set_length(1);
        rem.items.push(item);
        let mut op = Op::new();
        op.set_kind(Kind::REM);
        op.rem = MessageField::some(rem);
        let mut delta = Delta::new();
        delta.ops.push(op);
        let mut changes = ListChanges::new();
        changes.set_base_revision(root.revision().to_vec());
        changes.deltas.push(delta);
        let user = session.username();
        session.spclient().request_with_protobuf(&Method::POST, &format!("/playlist/v2/user/{user}/rootlist/changes"), None, &changes).await.unwrap();
        let root = SelectedListContent::parse_from_bytes(&session.spclient().get_rootlist(0, None).await.unwrap()).unwrap();
        println!("LIVE still in library: {}", root.contents.items.iter().any(|i| i.uri() == uri));
    }

    #[test]
    fn only_playlist_uris_become_addresses() {
        assert_eq!(playlist_id("spotify:playlist:37i9dQZF1"), Some("37i9dQZF1"));
        for bad in ["spotify:album:1", "spotify:playlist:", "spotify:playlist:a/../b"] {
            assert_eq!(playlist_id(bad), None, "{bad}");
        }
        let changes = added("me", &["spotify:track:1"], false);
        let add = &changes.deltas[0].ops[0].add;
        assert!(add.add_last() && add.items[0].uri() == "spotify:track:1");
    }
}
