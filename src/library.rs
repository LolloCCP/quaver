//! Changes to the library: liked songs and one's own playlists.

use std::collections::HashSet;

use http::{HeaderMap, HeaderValue, Method, header::{ACCEPT, CONTENT_TYPE}};
use librespot_core::{Error, Session};
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, CreateListReply, Delta, Item, ItemAttributes, ItemAttributesPartialState, ListAttributeKind, ListAttributes, ListAttributesPartialState,
    ListChanges, ListUpdateRequest, Mov, Op, Rem, SelectedListContent, UpdateItemAttributes, UpdateListAttributes, op::Kind,
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
    changes(user, op, None)
}

fn renamed(user: &str, name: &str) -> ListChanges {
    let mut attributes = ListAttributes::new();
    attributes.set_name(name.trim().to_owned());
    changes(user, attributes_op(attributes, vec![]), None)
}

/// One change by `user`, made on top of `revision` when that is known.
fn changes(user: &str, op: Op, revision: Option<&[u8]>) -> ListChanges {
    let mut info = ChangeInfo::new();
    info.set_user(user.to_owned());
    info.set_timestamp(now_ms());
    let mut delta = Delta::new();
    delta.ops.push(op);
    delta.info = MessageField::some(info);
    let mut changes = ListChanges::new();
    if let Some(revision) = revision {
        changes.set_base_revision(revision.to_vec());
    }
    changes.deltas.push(delta);
    changes
}

/// Sets the list's attributes in `values`; those in `cleared` are emptied.
fn attributes_op(values: ListAttributes, cleared: Vec<ListAttributeKind>) -> Op {
    let mut state = ListAttributesPartialState::new();
    state.values = MessageField::some(values);
    state.no_value = cleared.into_iter().map(Into::into).collect();
    let mut update = UpdateListAttributes::new();
    update.new_attributes = MessageField::some(state);
    let mut op = Op::new();
    op.set_kind(Kind::UPDATE_LIST_ATTRIBUTES);
    op.update_list_attributes = MessageField::some(update);
    op
}

/// What the edit dialog shows and changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Details {
    pub name: String,
    pub description: String,
    pub collaborative: bool,
    /// Shown on one's profile.
    pub public: bool,
}

/// A playlist's items from `from` on, with its revision and full length.
async fn items(session: &Session, id: &str, from: usize) -> Result<SelectedListContent, Error> {
    let path = format!("/playlist/v2/playlist/{id}?decorate=revision,attributes,length&from={from}&length={PAGE}");
    Ok(SelectedListContent::parse_from_bytes(&session.spclient().request(&Method::GET, &path, None, None).await?)?)
}

const PAGE: usize = 500;

/// The page of the library's own list that has `playlist`, and its place in the whole list and in the page.
async fn in_rootlist(session: &Session, playlist: &str) -> Result<(SelectedListContent, usize, usize), Error> {
    let mut from = 0;
    loop {
        let root = SelectedListContent::parse_from_bytes(&session.spclient().get_rootlist(from, Some(PAGE)).await?)?;
        if let Some(i) = root.contents.items.iter().position(|i| i.uri() == playlist) {
            return Ok((root, from + i, i));
        }
        from += root.contents.items.len();
        if root.contents.items.is_empty() || from >= root.length() as usize {
            return Err(Error::not_found("the playlist isn't in the library"));
        }
    }
}

pub async fn details(session: &Session, playlist: &str) -> Result<Details, Error> {
    let id = playlist_id(playlist).ok_or_else(|| Error::invalid_argument("not a playlist"))?;
    let list = items(session, id, 0).await?;
    let (root, _, i) = in_rootlist(session, playlist).await?;
    let public = root.contents.items[i].attributes.public();
    let a = &list.attributes;
    Ok(Details { name: a.name().to_owned(), description: a.description().to_owned(), collaborative: a.collaborative(), public })
}

pub async fn set_details(session: &Session, playlist: &str, old: &Details, new: &Details) -> Result<(), Error> {
    let id = playlist_id(playlist).ok_or_else(|| Error::invalid_argument("not a playlist"))?;
    let user = session.username();
    let mut values = ListAttributes::new();
    values.set_name(new.name.trim().to_owned());
    values.set_collaborative(new.collaborative);
    let description = new.description.trim();
    let mut cleared = vec![];
    if description.is_empty() {
        cleared.push(ListAttributeKind::LIST_DESCRIPTION);
    } else {
        values.set_description(description.to_owned());
    }
    let path = format!("/playlist/v2/playlist/{id}/changes");
    session.spclient().request_with_protobuf(&Method::POST, &path, None, &changes(&user, attributes_op(values, cleared), None)).await?;
    // Being on one's profile belongs to the library's entry, not to the playlist.
    if old.public != new.public {
        let (root, index, _) = in_rootlist(session, playlist).await?;
        let mut attributes = ItemAttributes::new();
        attributes.set_public(new.public);
        let mut state = ItemAttributesPartialState::new();
        state.values = MessageField::some(attributes);
        let mut update = UpdateItemAttributes::new();
        update.set_index(index as i32);
        update.new_attributes = MessageField::some(state);
        let mut op = Op::new();
        op.set_kind(Kind::UPDATE_ITEM_ATTRIBUTES);
        op.update_item_attributes = MessageField::some(update);
        let path = format!("/playlist/v2/user/{user}/rootlist/changes");
        session.spclient().request_with_protobuf(&Method::POST, &path, None, &changes(&user, op, Some(root.revision()))).await?;
    }
    Ok(())
}

/// Takes the playlist out of the library, which is what deleting it is on Spotify.
pub async fn delete_playlist(session: &Session, playlist: &str) -> Result<(), Error> {
    let (root, index, _) = in_rootlist(session, playlist).await?;
    let mut item = Item::new();
    item.set_uri(playlist.to_owned());
    let mut rem = Rem::new();
    rem.set_from_index(index as i32);
    rem.set_length(1);
    rem.items.push(item);
    let mut op = Op::new();
    op.set_kind(Kind::REM);
    op.rem = MessageField::some(rem);
    let user = session.username();
    let path = format!("/playlist/v2/user/{user}/rootlist/changes");
    session.spclient().request_with_protobuf(&Method::POST, &path, None, &changes(&user, op, Some(root.revision()))).await?;
    Ok(())
}

/// A song of a list, as the app shows it: the `nth` time (from 0) `uri` is in it.
/// The app leaves out what it can't show, such as episodes, so its rows can't be counted on.
pub struct Song<'a> {
    pub uri: &'a str,
    pub nth: usize,
}

/// Where each song is in the playlist, and its revision; the playlist is read only as far as needed.
async fn find(session: &Session, id: &str, songs: &[&Song<'_>]) -> Result<(Vec<usize>, Vec<u8>), Error> {
    let (mut seen, mut found) = (std::collections::HashMap::<String, usize>::new(), vec![None; songs.len()]);
    let mut from = 0;
    loop {
        let list = items(session, id, from).await?;
        for (i, item) in list.contents.items.iter().enumerate() {
            let count = seen.entry(item.uri().to_owned()).or_default();
            for (song, place) in songs.iter().zip(&mut found) {
                if song.uri == item.uri() && song.nth == *count {
                    *place = Some(from + i);
                }
            }
            *count += 1;
        }
        from += list.contents.items.len();
        if let Some(found) = found.iter().copied().collect::<Option<Vec<_>>>() {
            return Ok((found, list.revision().to_vec()));
        }
        if list.contents.items.is_empty() || from >= list.length() as usize {
            return Err(Error::not_found("the song is no longer in the playlist"));
        }
    }
}

pub async fn remove_song(session: &Session, playlist: &str, song: &Song<'_>) -> Result<(), Error> {
    let id = playlist_id(playlist).ok_or_else(|| Error::invalid_argument("not a playlist"))?;
    let (at, revision) = find(session, id, &[song]).await?;
    let mut item = Item::new();
    item.set_uri(song.uri.to_owned());
    let mut rem = Rem::new();
    rem.set_from_index(at[0] as i32);
    rem.set_length(1);
    rem.items.push(item);
    let mut op = Op::new();
    op.set_kind(Kind::REM);
    op.rem = MessageField::some(rem);
    let path = format!("/playlist/v2/playlist/{id}/changes");
    session.spclient().request_with_protobuf(&Method::POST, &path, None, &changes(&session.username(), op, Some(&revision))).await?;
    Ok(())
}

/// Moves `song` to the place of `other`, which moves one step towards where `song` was.
pub async fn move_song(session: &Session, playlist: &str, song: &Song<'_>, other: &Song<'_>) -> Result<(), Error> {
    let id = playlist_id(playlist).ok_or_else(|| Error::invalid_argument("not a playlist"))?;
    let (at, revision) = find(session, id, &[song, other]).await?;
    let path = format!("/playlist/v2/playlist/{id}/changes");
    session.spclient().request_with_protobuf(&Method::POST, &path, None, &changes(&session.username(), moved(at[0], at[1]), Some(&revision))).await?;
    Ok(())
}

/// Spotify counts the place to move to before the item is taken out.
fn moved(from: usize, to: usize) -> Op {
    let mut mov = Mov::new();
    mov.set_from_index(from as i32);
    mov.set_length(1);
    mov.set_to_index(if to > from { to + 1 } else { to } as i32);
    let mut op = Op::new();
    op.set_kind(Kind::MOV);
    op.mov = MessageField::some(mov);
    op
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
        let other = ["spotify:track:0DiWol3AO6WpXZgp0goxAV", "spotify:track:2Foc5Q5nqNiosCNqttzHof"].into_iter().find(|t| *t != track).unwrap();
        add_to_playlist(&session, &uri, other).await.unwrap();
        let songs = async || items(&session, playlist_id(&uri).unwrap(), 0).await.unwrap().contents.items.iter().map(|i| i.uri().to_owned()).collect::<Vec<_>>();
        move_song(&session, &uri, &Song { uri: other, nth: 0 }, &Song { uri: track, nth: 0 }).await.unwrap();
        assert_eq!(songs().await, [other, track], "moved up");
        move_song(&session, &uri, &Song { uri: other, nth: 0 }, &Song { uri: track, nth: 0 }).await.unwrap();
        assert_eq!(songs().await, [track, other], "moved down");
        remove_song(&session, &uri, &Song { uri: track, nth: 0 }).await.unwrap();
        assert_eq!(songs().await, [other], "removed");
        let old = details(&session, &uri).await.unwrap();
        let new = Details { name: "Rustbeat test 2".into(), description: "About it".into(), collaborative: true, public: !old.public };
        set_details(&session, &uri, &old, &new).await.unwrap();
        assert_eq!(details(&session, &uri).await.unwrap(), new);
        delete_playlist(&session, &uri).await.unwrap();
        assert!(in_rootlist(&session, &uri).await.is_err(), "still in the library");
    }

    #[test]
    fn only_playlist_uris_become_addresses() {
        assert_eq!(playlist_id("spotify:playlist:37i9dQZF1"), Some("37i9dQZF1"));
        for bad in ["spotify:album:1", "spotify:playlist:", "spotify:playlist:a/../b"] {
            assert_eq!(playlist_id(bad), None, "{bad}");
        }
        let mov = moved(1, 3).mov.unwrap();
        assert_eq!((mov.from_index(), mov.to_index()), (1, 4), "down: counted before it is taken out");
        assert_eq!(moved(3, 1).mov.to_index(), 1, "up");
        let changes = added("me", &["spotify:track:1"], false);
        let add = &changes.deltas[0].ops[0].add;
        assert!(add.add_last() && add.items[0].uri() == "spotify:track:1");
    }
}
