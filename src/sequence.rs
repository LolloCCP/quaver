//! Play order for a player that can't start a playlist at a given song: we keep the order
//! and hand over one song ahead of time, so songs still follow each other without a gap.

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
    /// The song the player already holds as its next one.
    ahead: Option<Ahead>,
    /// Songs the player holds that are no longer wanted. It can't drop them, so they are skipped.
    stale: Vec<String>,
    /// The song we last asked for.
    expected: Option<String>,
    pub shuffle: bool,
    pub repeat: bool,
}

struct Ahead {
    uri: String,
    from_queue: bool,
}

#[derive(Debug, PartialEq)]
pub enum Started {
    /// Our song, or a repeat of it: hand over the one after it.
    Ours,
    /// Left over from an earlier list.
    Skip,
    /// Someone else is in control, such as the phone.
    Foreign,
}

impl Sequence {
    /// Replaces the list and returns the song to play.
    pub fn start(&mut self, tracks: Vec<String>, first: &str) -> Option<String> {
        let first = tracks.iter().position(|t| t == first).unwrap_or(0);
        self.tracks = tracks;
        self.arrange(first);
        self.drop_ahead();
        self.expected = self.tracks.get(first).cloned();
        self.expected.clone()
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

    fn drop_ahead(&mut self) {
        if let Some(ahead) = self.ahead.take() {
            if ahead.from_queue {
                self.queued.push_front(ahead.uri.clone());
            }
            self.stale.push(ahead.uri);
        }
    }

    /// Turns shuffle on or off around the current song.
    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        if let Some(current) = self.order.get(self.at).copied() {
            self.arrange(current);
            self.drop_ahead();
        }
    }

    pub fn enqueue(&mut self, uri: String) {
        self.queued.push_back(uri);
        if self.ahead.as_ref().is_some_and(|a| !a.from_queue) {
            self.drop_ahead();
        }
    }

    pub fn clear_queue(&mut self) {
        if self.ahead.as_ref().is_some_and(|a| a.from_queue) {
            self.drop_ahead();
        }
        self.queued.clear();
    }

    fn next_in_list(&self) -> Option<&String> {
        let next = self.at + 1;
        let next = if next < self.order.len() { next } else if self.repeat { 0 } else { return None };
        self.tracks.get(*self.order.get(next)?)
    }

    /// The song the player should now be given as its next one, if it doesn't have one yet.
    pub fn hand_over(&mut self) -> Option<String> {
        if self.ahead.is_some() || self.expected.is_none() {
            return None;
        }
        let (uri, from_queue) = match self.queued.pop_front() {
            Some(uri) => (uri, true),
            None => (self.next_in_list()?.clone(), false),
        };
        // The same song still waiting in the player from before serves just as well
        // only if nothing else waits in front of it.
        if self.stale.first() == Some(&uri) {
            self.stale.remove(0);
            self.ahead = Some(Ahead { uri, from_queue });
            return None;
        }
        self.ahead = Some(Ahead { uri: uri.clone(), from_queue });
        Some(uri)
    }

    /// The player started a song.
    pub fn started(&mut self, uri: &str) -> Started {
        if self.expected.as_deref() == Some(uri) {
            return Started::Ours;
        }
        // Waiting songs play in the order they were handed over, old ones first.
        if let Some(i) = self.stale.iter().position(|s| s == uri) {
            self.stale.drain(..=i);
            return Started::Skip;
        }
        if self.ahead.as_ref().is_some_and(|a| a.uri == uri) {
            let ahead = self.ahead.take().expect("checked above");
            if !ahead.from_queue {
                self.at = if self.at + 1 < self.order.len() { self.at + 1 } else { 0 };
            }
            self.expected = Some(ahead.uri);
            return Started::Ours;
        }
        *self = Self { shuffle: self.shuffle, repeat: self.repeat, ..Default::default() };
        Started::Foreign
    }

    /// The player was started anew: it holds nothing of ours any more.
    pub fn player_restarted(&mut self) {
        if let Some(ahead) = self.ahead.take().filter(|a| a.from_queue) {
            self.queued.push_front(ahead.uri);
        }
        self.stale.clear();
    }

    /// Moves back and returns the song to play.
    pub fn previous(&mut self) -> Option<String> {
        self.at = self.at.checked_sub(1)?;
        self.drop_ahead();
        self.expected = self.tracks.get(*self.order.get(self.at)?).cloned();
        self.expected.clone()
    }

    /// Plays from a song of `upcoming`, dropping what is before it.
    pub fn jump(&mut self, uri: &str) -> Option<String> {
        self.drop_ahead();
        if let Some(i) = self.queued.iter().position(|q| q == uri) {
            self.queued.drain(..=i);
        } else {
            let ahead = self.order.iter().skip(self.at + 1).position(|o| self.tracks[*o] == uri)?;
            self.at += 1 + ahead;
            self.queued.clear();
        }
        self.expected = Some(uri.to_owned());
        self.expected.clone()
    }

    pub fn current(&self) -> Option<String> {
        self.expected.clone()
    }

    /// Whether there is anything of ours to go on with.
    pub fn active(&self) -> bool {
        self.expected.is_some()
    }

    pub fn upcoming(&self, count: usize) -> Vec<String> {
        let ahead = self.ahead.iter().filter(|a| a.from_queue).map(|a| &a.uri);
        let rest = self.order.iter().skip(self.at + 1).map(|o| &self.tracks[*o]);
        ahead.chain(&self.queued).chain(rest).take(count).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(text: &str) -> Vec<String> {
        text.chars().map(String::from).collect()
    }

    /// A player that behaves as documented: plays what it is told, then its queue.
    #[derive(Default)]
    struct Player {
        queue: VecDeque<String>,
        heard: String,
    }

    impl Player {
        fn play(&mut self, s: &mut Sequence, uri: Option<String>) {
            if let Some(uri) = uri {
                self.begin(s, uri);
            }
        }

        fn begin(&mut self, s: &mut Sequence, uri: String) {
            match s.started(&uri) {
                Started::Skip => return self.next(s),
                Started::Ours => self.heard += &uri,
                Started::Foreign => self.heard += "!",
            }
            self.give(s);
        }

        fn give(&mut self, s: &mut Sequence) {
            self.queue.extend(s.hand_over());
        }

        fn next(&mut self, s: &mut Sequence) {
            if let Some(uri) = self.queue.pop_front() {
                self.begin(s, uri);
            }
        }
    }

    fn run(s: &mut Sequence, p: &mut Player, songs: usize) {
        for _ in 0..songs {
            p.next(s);
        }
    }

    #[test]
    fn plays_on_from_the_chosen_song_and_stops_at_the_end() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("abcd"), "b");
        p.play(&mut s, first);
        run(&mut s, &mut p, 5);
        assert_eq!(p.heard, "bcd");
    }

    #[test]
    fn repeat_wraps_around() {
        let (mut s, mut p) = (Sequence { repeat: true, ..Default::default() }, Player::default());
        let first = s.start(list("abc"), "c");
        p.play(&mut s, first);
        run(&mut s, &mut p, 4);
        assert_eq!(p.heard, "cabca");
    }

    #[test]
    fn a_new_list_never_lets_the_old_one_through() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        for (tracks, first) in [("abc", "a"), ("xyz", "x"), ("mno", "m")] {
            let first = s.start(list(tracks), first);
            p.play(&mut s, first);
        }
        run(&mut s, &mut p, 2);
        assert_eq!(p.heard, "axmno");
    }

    #[test]
    fn queued_songs_come_first_in_order_then_the_list_goes_on() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("abc"), "a");
        p.play(&mut s, first);
        for q in ["q", "r"] {
            s.enqueue(q.into());
            p.give(&mut s);
        }
        assert_eq!(s.upcoming(9), list("qrbc"));
        run(&mut s, &mut p, 4);
        assert_eq!(p.heard, "aqrbc");
    }

    #[test]
    fn a_queued_song_survives_choosing_another_list() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("ab"), "a");
        p.play(&mut s, first);
        s.enqueue("q".into());
        p.give(&mut s);
        let first = s.start(list("xy"), "x");
        p.play(&mut s, first);
        run(&mut s, &mut p, 2);
        assert_eq!(p.heard, "axqy");
    }

    #[test]
    fn clearing_the_queue_goes_back_to_the_list() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("ab"), "a");
        p.play(&mut s, first);
        s.enqueue("q".into());
        p.give(&mut s);
        s.clear_queue();
        p.give(&mut s);
        run(&mut s, &mut p, 2);
        assert_eq!(p.heard, "ab");
    }

    #[test]
    fn going_back_and_jumping_ahead() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("abcde"), "b");
        p.play(&mut s, first);
        let back = s.previous();
        p.play(&mut s, back);
        assert_eq!(s.previous(), None, "already at the start");
        let jump = s.jump("d");
        p.play(&mut s, jump);
        run(&mut s, &mut p, 1);
        assert_eq!(p.heard, "bade");
    }

    #[test]
    fn shuffle_plays_everything_once() {
        let (mut s, mut p) = (Sequence { shuffle: true, ..Default::default() }, Player::default());
        let first = s.start(list("abcdefgh"), "d");
        p.play(&mut s, first);
        run(&mut s, &mut p, 12);
        assert!(p.heard.starts_with('d'));
        let mut heard: Vec<_> = p.heard.chars().collect();
        heard.sort();
        assert_eq!(heard.into_iter().collect::<String>(), "abcdefgh");
    }

    #[test]
    fn shuffle_can_change_in_the_middle() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("abcdef"), "a");
        p.play(&mut s, first);
        s.set_shuffle(true);
        p.give(&mut s);
        s.set_shuffle(false);
        p.give(&mut s);
        run(&mut s, &mut p, 6);
        assert_eq!(p.heard, "abcdef");
    }

    #[test]
    fn a_restarted_player_is_given_everything_again() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("abc"), "a");
        p.play(&mut s, first);
        s.enqueue("q".into());
        p.give(&mut s);
        p = Player { heard: p.heard, ..Default::default() };
        s.player_restarted();
        let again = s.current();
        p.play(&mut s, again);
        run(&mut s, &mut p, 4);
        assert_eq!(p.heard, "aaqbc");
    }

    #[test]
    fn another_device_takes_over() {
        let (mut s, mut p) = (Sequence::default(), Player::default());
        let first = s.start(list("abc"), "a");
        p.play(&mut s, first);
        p.begin(&mut s, "z".into());
        assert_eq!(p.heard, "a!");
        assert!(!s.active() && s.hand_over().is_none());
    }
}
