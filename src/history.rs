//! The pages that were visited, for going back and forward.

#[derive(Clone, Debug, PartialEq)]
pub enum Page {
    Home,
    Liked,
    Queue,
    List { title: String, uri: String, picture: String },
    Search(String),
    People(String),
}

pub struct History {
    pages: Vec<Page>,
    at: usize,
}

/// More than anyone steps back through.
const MAX_PAGES: usize = 100;

impl Default for History {
    fn default() -> Self {
        Self { pages: vec![Page::Home], at: 0 }
    }
}

impl History {
    /// Going somewhere new forgets what was ahead. Showing the current page again changes nothing.
    pub fn visit(&mut self, page: Page) {
        if self.pages[self.at] == page {
            return;
        }
        self.pages.truncate(self.at + 1);
        // Typing on refines one search instead of making many.
        if matches!((&self.pages[self.at], &page), (Page::Search(_), Page::Search(_))) {
            self.pages[self.at] = page;
            return;
        }
        self.pages.push(page);
        if self.pages.len() > MAX_PAGES {
            self.pages.remove(0);
        }
        self.at = self.pages.len() - 1;
    }

    pub fn can_go_back(&self) -> bool {
        self.at > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.at + 1 < self.pages.len()
    }

    pub fn back(&mut self) -> Option<Page> {
        self.at = self.at.checked_sub(1)?;
        Some(self.pages[self.at].clone())
    }

    pub fn forward(&mut self) -> Option<Page> {
        if !self.can_go_forward() {
            return None;
        }
        self.at += 1;
        Some(self.pages[self.at].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(n: usize) -> Page {
        Page::List { title: n.to_string(), uri: n.to_string(), picture: String::new() }
    }

    #[test]
    fn back_and_forward_retrace_the_way() {
        let mut h = History::default();
        assert!(!h.can_go_back() && !h.can_go_forward() && h.back().is_none() && h.forward().is_none());
        h.visit(list(1));
        h.visit(list(2));
        assert_eq!((h.back(), h.back(), h.back()), (Some(list(1)), Some(Page::Home), None));
        assert_eq!((h.forward(), h.forward(), h.forward()), (Some(list(1)), Some(list(2)), None));
    }

    #[test]
    fn a_new_page_forgets_what_was_ahead_but_the_same_page_does_not() {
        let mut h = History::default();
        h.visit(list(1));
        h.visit(list(2));
        h.back();
        h.visit(list(1));
        assert!(h.can_go_forward(), "showing the page again is not a visit");
        h.visit(Page::Liked);
        assert!(!h.can_go_forward());
        assert_eq!(h.back(), Some(list(1)));
    }

    #[test]
    fn typing_is_one_search() {
        let mut h = History::default();
        for q in ["pi", "pia", "piano"] {
            h.visit(Page::Search(q.into()));
        }
        assert_eq!((h.back(), h.forward()), (Some(Page::Home), Some(Page::Search("piano".into()))));
    }

    #[test]
    fn it_does_not_grow_forever() {
        let mut h = History::default();
        (0..500).for_each(|n| h.visit(list(n)));
        assert_eq!(h.pages.len(), MAX_PAGES);
        assert_eq!(h.back(), Some(list(498)));
    }
}
