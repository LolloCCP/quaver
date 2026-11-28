//! Play order for songs from a folder; what Spotify plays is ordered by Spotify.

use std::collections::VecDeque;

use rand::seq::SliceRandom;

#[derive(Default)]
pub struct Sequence {
    /// The list as shown, and the order it is played in (indexes into `tracks`).
    tracks: Vec<String>,
    order: Vec<usize>,
    at: usize,
    /// Songs the user queued; they go before the rest of the list.
    queued: VecDeque<String>,
    /// The song that plays.
    current: Option<String>,
    pub shuffle: bool,
    pub repeat: bool,
}

impl Sequence {
    /// Replaces the list and returns the song to play.
    pub fn start(&mut self, tracks: Vec<String>, first: &str) -> Option<String> {
        let first = tracks.iter().position(|t| t == first).unwrap_or(0);
        self.tracks = tracks;
        self.arrange(first);
        self.current = self.tracks.get(first).cloned();
        self.current.clone()
    }

    fn arrange(&mut self, current: usize) {
        self.order = (0..self.tracks.len()).collect();
        self.at = current;
        if self.shuffle {
            self.order.shuffle(&mut rand::rng());
            if let Some(i) = self.order.iter().position(|o| *o == current) {
                self.order.swap(0, i);
            }
            self.at = 0;
        }
    }

    /// Turns shuffle on or off around the current song.
    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        if let Some(current) = self.order.get(self.at).copied() {
            self.arrange(current);
        }
    }

    pub fn enqueue(&mut self, uri: String) {
        self.queued.push_back(uri);
    }

    pub fn clear_queue(&mut self) {
        self.queued.clear();
    }

    /// Moves on and returns the song to play.
    pub fn ran_out(&mut self) -> Option<String> {
        self.current.as_ref()?;
        let next = match self.queued.pop_front() {
            Some(uri) => uri,
            None => {
                let next = self.at + 1;
                let next = if next < self.order.len() { next } else if self.repeat { 0 } else { return None };
                let uri = self.tracks.get(*self.order.get(next)?)?.clone();
                self.at = next;
                uri
            }
        };
        self.current = Some(next);
        self.current.clone()
    }

    /// Moves back and returns the song to play.
    pub fn previous(&mut self) -> Option<String> {
        self.at = self.at.checked_sub(1)?;
        self.current = self.tracks.get(*self.order.get(self.at)?).cloned();
        self.current.clone()
    }

    /// Plays from a song of `upcoming`, dropping what is before it.
    pub fn jump(&mut self, uri: &str) -> Option<String> {
        if let Some(i) = self.queued.iter().position(|q| q == uri) {
            self.queued.drain(..=i);
        } else {
            let ahead = self.order.iter().skip(self.at + 1).position(|o| self.tracks[*o] == uri)?;
            self.at += 1 + ahead;
            self.queued.clear();
        }
        self.current = Some(uri.to_owned());
        self.current.clone()
    }

    /// Whether there is anything of ours to go on with.
    pub fn active(&self) -> bool {
        self.current.is_some()
    }

    /// See `Cmd::EditQueue`. False if a song isn't there, or would leave its part: the queue or the list.
    pub fn edit(&mut self, (uri, nth): (&str, usize), to: Option<(&str, usize)>) -> bool {
        let upcoming = self.upcoming(usize::MAX);
        let find = |uri: &str, nth| upcoming.iter().enumerate().filter(|(_, u)| *u == uri).nth(nth).map(|(i, _)| i);
        let Some(from) = find(uri, nth) else { return false };
        let to = match to {
            Some((uri, nth)) => match find(uri, nth) {
                Some(to) => Some(to),
                None => return false,
            },
            None => None,
        };
        let queued = self.queued.len();
        if to.is_some_and(|to| (to < queued) != (from < queued)) {
            return false;
        }
        if from < queued {
            if let Some(song) = self.queued.remove(from) && let Some(to) = to {
                self.queued.insert(to, song);
            }
        } else {
            let in_order = |i: usize| self.at + 1 + i - queued;
            let song = self.order.remove(in_order(from));
            if let Some(to) = to {
                self.order.insert(in_order(to), song);
            }
        }
        true
    }

    pub fn upcoming(&self, count: usize) -> Vec<String> {
        let rest = self.order.iter().skip(self.at + 1).map(|o| &self.tracks[*o]);
        self.queued.iter().chain(rest).take(count).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(text: &str) -> Vec<String> {
        text.chars().map(String::from).collect()
    }

    fn heard(s: &mut Sequence, first: &str, tracks: &str, songs: usize) -> String {
        let first = s.start(list(tracks), first);
        first.into_iter().chain((0..songs).map_while(|_| s.ran_out())).collect()
    }

    #[test]
    fn plays_on_from_the_chosen_song_and_stops_at_the_end() {
        assert_eq!(heard(&mut Sequence::default(), "b", "abcd", 5), "bcd");
    }

    #[test]
    fn repeat_wraps_around() {
        assert_eq!(heard(&mut Sequence { repeat: true, ..Default::default() }, "c", "abc", 4), "cabca");
    }

    #[test]
    fn queued_songs_come_first_in_order_then_the_list_goes_on() {
        let mut s = Sequence::default();
        s.start(list("abc"), "a");
        s.enqueue("q".into());
        s.enqueue("r".into());
        assert_eq!(s.upcoming(9), list("qrbc"));
        assert_eq!((0..4).map_while(|_| s.ran_out()).collect::<String>(), "qrbc");
        assert_eq!(s.ran_out(), None, "the list is over");
    }

    #[test]
    fn a_queued_song_survives_choosing_another_list() {
        let mut s = Sequence::default();
        s.start(list("ab"), "a");
        s.enqueue("q".into());
        assert_eq!(heard(&mut s, "x", "xy", 2), "xqy");
    }

    #[test]
    fn clearing_the_queue_goes_back_to_the_list() {
        let mut s = Sequence::default();
        s.start(list("ab"), "a");
        s.enqueue("q".into());
        s.clear_queue();
        assert_eq!(s.ran_out(), Some("b".into()));
    }

    #[test]
    fn songs_to_come_move_and_go_within_their_part() {
        let mut s = Sequence::default();
        s.start(list("abcd"), "a");
        s.enqueue("q".into());
        s.enqueue("r".into());
        assert!(s.edit(("r", 0), Some(("q", 0))));
        assert!(s.edit(("d", 0), Some(("b", 0))));
        assert_eq!(s.upcoming(9), list("rqdbc"));
        assert!(!s.edit(("b", 0), Some(("q", 0))), "into the queue");
        assert!(!s.edit(("x", 0), None), "not there");
        assert!(s.edit(("q", 0), None) && s.edit(("c", 0), None));
        assert_eq!((0..4).map_while(|_| s.ran_out()).collect::<String>(), "rdb");
    }

    #[test]
    fn going_back_and_jumping_ahead() {
        let mut s = Sequence::default();
        s.start(list("abcde"), "b");
        assert_eq!((s.previous(), s.previous()), (Some("a".into()), None), "already at the start");
        assert_eq!((s.jump("d"), s.ran_out()), (Some("d".into()), Some("e".into())));
    }

    #[test]
    fn shuffle_plays_everything_once() {
        let heard = heard(&mut Sequence { shuffle: true, ..Default::default() }, "d", "abcdefgh", 12);
        assert!(heard.starts_with('d'));
        let mut heard: Vec<_> = heard.chars().collect();
        heard.sort();
        assert_eq!(heard.into_iter().collect::<String>(), "abcdefgh");
    }

    #[test]
    fn shuffle_can_change_in_the_middle() {
        let mut s = Sequence::default();
        s.start(list("abcdef"), "a");
        s.set_shuffle(true);
        s.set_shuffle(false);
        assert_eq!((0..6).map_while(|_| s.ran_out()).collect::<String>(), "bcdef");
    }

    #[test]
    fn queueing_the_current_song_plays_it_once_more() {
        let mut s = Sequence::default();
        s.start(list("ab"), "a");
        s.enqueue("a".into());
        assert_eq!([s.ran_out(), s.ran_out(), s.ran_out()], [Some("a".into()), Some("b".into()), None]);
        assert!(!Sequence::default().active() && Sequence::default().ran_out().is_none());
    }
}
