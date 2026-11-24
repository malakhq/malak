use super::{
    model::{Address, Event, EventKey},
    multiboard::BoardId,
};
use ratatui::crossterm::event::KeyCode;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Tab {
    #[default]
    Activity,
    Questions,
    Orders,
    Waiting,
    Trace,
}

impl Tab {
    pub const ALL: [Self; 5] = [
        Self::Activity,
        Self::Questions,
        Self::Orders,
        Self::Waiting,
        Self::Trace,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Activity => "Activity",
            Self::Questions => "Questions",
            Self::Orders => "Orders",
            Self::Waiting => "Waiting",
            Self::Trace => "Trace",
        }
    }

    fn step(self, offset: usize) -> Self {
        Self::ALL[(self as usize + offset) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Pane {
    Participants,
    #[default]
    Messages,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum RowKey {
    Participant(String),
    Archive,
    Message(Address),
    Event(EventKey),
    AllBoards,
    Board(BoardId),
    OnBoard(BoardId, Box<RowKey>),
}

impl RowKey {
    pub fn with_board(self, board: &BoardId) -> Self {
        Self::OnBoard(board.clone(), Box::new(self))
    }

    fn board(&self) -> Option<&BoardId> {
        match self {
            Self::Board(board) | Self::OnBoard(board, _) => Some(board),
            _ => None,
        }
    }

    pub fn activity(key: &EventKey, event: &Event) -> Self {
        event
            .address()
            .map_or_else(|| Self::Event(key.clone()), Self::Message)
    }
}

#[derive(Default)]
pub(super) struct Cursor {
    pub index: usize,
    pub first: usize,
    selected: Option<RowKey>,
    len: usize,
    tracking: bool,
    first_key: Option<RowKey>,
    last_live: HashSet<RowKey>,
}

impl Cursor {
    pub fn sync<'a>(&mut self, rows: impl ExactSizeIterator<Item = &'a RowKey> + Clone) {
        self.len = rows.len();
        if self.len == 0 {
            return;
        }
        if self.tracking {
            // Keep the same first visible row when the list grows above it.
            if let Some(first) = self
                .first_key
                .as_ref()
                .and_then(|key| rows.clone().position(|row| row == key))
            {
                self.index = self
                    .index
                    .saturating_add_signed(first as isize - self.first as isize);
                self.first = first;
            }
            if let Some(index) = self
                .selected
                .as_ref()
                .and_then(|key| rows.clone().position(|row| row == key))
            {
                self.index = index;
            }
        }
        self.index = self.index.min(self.len.saturating_sub(1));
        self.selected = rows.clone().nth(self.index).cloned();
    }

    pub fn show_from(&mut self, first: usize, key: &RowKey) {
        self.first = first;
        if self.first_key.as_ref() != Some(key) {
            self.first_key = Some(key.clone());
        }
    }

    pub fn new_above<'a>(
        &mut self,
        rows: impl ExactSizeIterator<Item = &'a RowKey> + Clone,
    ) -> Option<usize> {
        if !self.tracking {
            if self.last_live.len() != rows.len()
                || !rows.clone().all(|key| self.last_live.contains(key))
            {
                self.last_live.clear();
                self.last_live.extend(rows.cloned());
            }
            return None;
        }
        // A late arrival can be new even if its timestamp predates the head.
        (self.index > 0).then(|| {
            rows.take(self.first.min(self.index))
                .filter(|key| !self.last_live.contains(key))
                .count()
        })
    }

    pub fn pin(&mut self) {
        self.tracking = true;
    }

    fn select(&mut self, index: usize) {
        self.index = index.min(self.len.saturating_sub(1));
        self.pin();
        self.selected = None;
    }

    fn follow(&mut self) {
        *self = Self {
            len: self.len,
            ..Self::default()
        };
    }

    fn jump(&mut self, target: RowKey) {
        self.pin();
        self.selected = Some(target);
    }
}

#[derive(Default)]
pub(super) struct Visible {
    pub jumps: Vec<(char, RowKey)>,
    pub open: bool,
    pub archive: bool,
    pub copy: Option<String>,
    pub trace: Option<String>,
    pub trace_copy: Option<String>,
    pub board: Option<BoardId>,
    pub preview: Option<Address>,
    pub new_above: Option<usize>,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Startup {
    pub history_ready: bool,
    pub history_complete: bool,
}

pub(super) struct View {
    pub startup: Startup,
    pub stopped_at: Option<u64>,
    pub focus: Pane,
    pub tab: Tab,
    pub filter: Option<String>,
    pub jumping: bool,
    pub help: bool,
    pub copied: bool,
    pub trace_copied: bool,
    pub full_body: Option<BodyScroll>,
    pub activity_open: bool,
    pub archive_open: bool,
    pub boards: Option<BoardNavigation>,
    pub visible: Visible,
    participants: Cursor,
    messages: [Cursor; Tab::ALL.len()],
    trace_return: Option<Tab>,
}

#[derive(Default)]
pub(super) struct BoardNavigation {
    pub opened: HashSet<BoardId>,
    pub archives: HashSet<BoardId>,
    pub selected: Option<BoardId>,
}

impl BoardNavigation {
    fn toggle_archive(&mut self, board: &BoardId) {
        if !self.archives.remove(board) {
            self.archives.insert(board.clone());
        }
    }
}

#[derive(Default)]
pub(super) struct BodyScroll {
    pub offset: usize,
    pub shown: usize,
    pub total: usize,
}

impl BodyScroll {
    pub fn fit(&mut self, shown: usize, total: usize) {
        self.shown = shown.min(total);
        self.total = total;
        self.offset = self.offset.min(total - self.shown);
    }

    fn move_by(&mut self, lines: isize) {
        self.offset = self
            .offset
            .saturating_add_signed(lines)
            .min(self.total - self.shown);
    }
}

impl Default for View {
    fn default() -> Self {
        Self {
            startup: Startup::default(),
            stopped_at: None,
            focus: Pane::Messages,
            tab: Tab::Activity,
            filter: None,
            jumping: false,
            help: false,
            copied: false,
            trace_copied: false,
            full_body: None,
            activity_open: true,
            archive_open: false,
            boards: None,
            visible: Visible::default(),
            participants: Cursor::default(),
            messages: Default::default(),
            trace_return: None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Action {
    None,
    Redraw,
    Copy(String),
    CopyTrace(String),
    Trace(String),
    ReturnFromTrace,
    Quit,
}

impl View {
    pub fn for_boards() -> Self {
        Self {
            boards: Some(BoardNavigation::default()),
            ..Self::default()
        }
    }

    pub fn ready(&self) -> bool {
        self.startup.history_ready || self.stopped_at.is_some()
    }

    pub fn cursor(&mut self, pane: Pane) -> &mut Cursor {
        match pane {
            Pane::Participants => &mut self.participants,
            Pane::Messages => &mut self.messages[self.tab as usize],
        }
    }

    pub fn can_return(&self) -> bool {
        self.tab == Tab::Trace && self.trace_return.is_some()
    }

    fn switch(&mut self, tab: Tab) {
        self.tab = tab;
        self.focus = Pane::Messages;
        self.jumping = false;
        self.full_body = None;
    }

    fn filter(&mut self, id: Option<String>) {
        self.filter = id;
        self.messages = Default::default();
        self.switch(self.tab);
    }

    pub fn sync_boards(&mut self, keys: &[RowKey]) {
        self.participants.pin();
        self.participants.sync(keys.iter());
        let key = self
            .participants
            .selected
            .as_ref()
            .filter(|_| !keys.is_empty());
        let board = key.and_then(RowKey::board).cloned();
        let filter = match key {
            Some(RowKey::OnBoard(_, child)) => match child.as_ref() {
                RowKey::Participant(id) => Some(id.clone()),
                _ => None,
            },
            _ => None,
        };
        let boards = self.boards.as_mut().unwrap();
        if boards.selected != board || self.filter != filter {
            boards.selected = board;
            self.filter = filter;
            self.messages = Default::default();
            self.full_body = None;
        }
    }

    pub fn board_key(&mut self, key: KeyCode, can_expand: bool) -> Option<Action> {
        let boards = self.boards.as_mut()?;
        if self.help || self.jumping || self.full_body.is_some() {
            return None;
        }
        let selected = self.participants.selected.as_ref()?;
        let board = selected.board()?;
        match key {
            KeyCode::Char('a') if self.visible.archive => {
                boards.toggle_archive(board);
            }
            KeyCode::Right | KeyCode::Enter if self.focus == Pane::Participants && can_expand => {
                if matches!(selected, RowKey::Board(_)) {
                    boards.opened.insert(board.clone());
                } else if matches!(selected, RowKey::OnBoard(_, child) if matches!(child.as_ref(), RowKey::Archive))
                {
                    boards.toggle_archive(board);
                }
            }
            KeyCode::Left if self.focus == Pane::Participants => {
                boards.opened.remove(board);
                self.participants.jump(RowKey::Board(board.clone()));
            }
            _ => return None,
        }
        Some(Action::Redraw)
    }

    pub fn key(&mut self, key: KeyCode) -> Action {
        if key == KeyCode::Char('q') {
            return Action::Quit;
        }
        self.copied = false;
        self.trace_copied = false;
        if self.help {
            return match key {
                KeyCode::Esc | KeyCode::Char('?') => {
                    self.help = false;
                    Action::Redraw
                }
                _ => Action::None,
            };
        }
        if self.jumping {
            return self.jump_key(key);
        }
        if matches!(key, KeyCode::Char('g' | 'G') | KeyCode::Home | KeyCode::End) {
            self.full_body = None;
            let cursor = self.cursor(self.focus);
            if matches!(key, KeyCode::Char('g') | KeyCode::Home) {
                cursor.follow();
            } else {
                cursor.select(cursor.len.saturating_sub(1));
            }
            return Action::Redraw;
        }
        if self.full_body.is_some() {
            return self.body_key(key);
        }
        match key {
            KeyCode::Esc if self.can_return() => {
                let previous = self.trace_return.take().unwrap();
                self.switch(previous);
                return Action::ReturnFromTrace;
            }
            KeyCode::Esc if self.filter.is_some() => {
                if let Some(board) = self
                    .boards
                    .as_ref()
                    .and_then(|boards| boards.selected.clone())
                {
                    self.participants.jump(RowKey::Board(board));
                }
                self.filter(None);
            }
            KeyCode::Esc => return Action::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Pane::Participants => Pane::Messages,
                    Pane::Messages => Pane::Participants,
                };
            }
            KeyCode::Char('[') => self.switch(self.tab.step(Tab::ALL.len() - 1)),
            KeyCode::Char(']') => self.switch(self.tab.step(1)),
            KeyCode::Char(key @ '1'..='5') => self.switch(Tab::ALL[usize::from(key as u8 - b'1')]),
            KeyCode::Up | KeyCode::Down => {
                let messages = self.focus == Pane::Messages;
                let cursor = self.cursor(self.focus);
                let index = if key == KeyCode::Up {
                    cursor.index.saturating_sub(1)
                } else {
                    cursor
                        .index
                        .saturating_add(1)
                        .min(cursor.len.saturating_sub(1))
                };
                if messages && index == 0 {
                    cursor.follow();
                } else {
                    cursor.select(index);
                }
            }
            KeyCode::Enter if self.visible.open => match self.focus {
                Pane::Participants => {
                    if let Some(RowKey::Participant(id)) = &self.participants.selected {
                        self.filter(Some(id.clone()));
                    }
                }
                Pane::Messages => {
                    self.cursor(self.focus).pin();
                    self.full_body = Some(BodyScroll::default());
                }
            },
            KeyCode::Char('e') if self.focus == Pane::Messages && self.tab == Tab::Activity => {
                self.activity_open = !self.activity_open;
            }
            KeyCode::Char('a') if self.boards.is_none() && self.visible.archive => {
                self.archive_open = !self.archive_open;
            }
            KeyCode::Char('f') if !self.visible.jumps.is_empty() => self.jumping = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('c' | 'y') => return self.copy(key),
            KeyCode::Char('t') => return self.open_trace(),
            _ => return Action::None,
        }
        Action::Redraw
    }

    fn body_key(&mut self, key: KeyCode) -> Action {
        let body = self.full_body.as_mut().unwrap();
        match key {
            KeyCode::Esc | KeyCode::Enter => self.full_body = None,
            KeyCode::Up => body.move_by(-1),
            KeyCode::Down => body.move_by(1),
            KeyCode::Char('c' | 'y') => return self.copy(key),
            KeyCode::Char('t') => return self.open_trace(),
            _ => return Action::None,
        }
        Action::Redraw
    }

    fn open_trace(&mut self) -> Action {
        if self.tab != Tab::Trace
            && let Some(trace) = self.visible.trace.clone()
        {
            self.cursor(self.focus).pin();
            self.trace_return = Some(self.tab);
            self.messages[Tab::Trace as usize] = Cursor::default();
            self.switch(Tab::Trace);
            return Action::Trace(trace);
        }
        Action::None
    }

    fn copy(&self, key: KeyCode) -> Action {
        match key {
            KeyCode::Char('c') => self.visible.copy.clone().map_or(Action::None, Action::Copy),
            _ => self
                .visible
                .trace_copy
                .as_ref()
                .or(self.visible.trace.as_ref())
                .cloned()
                .map_or(Action::None, Action::CopyTrace),
        }
    }

    fn jump_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('f') => self.jumping = false,
            KeyCode::Char(key) => {
                let Some((_, target)) =
                    self.visible.jumps.iter().find(|(letter, _)| *letter == key)
                else {
                    return Action::None;
                };
                let target = target.clone();
                self.cursor(self.focus).jump(target);
                self.jumping = false;
            }
            _ => return Action::None,
        }
        Action::Redraw
    }
}

/// OSC 52 sends the exact text to the terminal's clipboard, including over SSH.
pub(super) fn copy_text(out: &mut impl std::io::Write, text: &str) -> std::io::Result<()> {
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    out.write_all(b"\x1b]52;c;")?;
    for chunk in text.as_bytes().chunks(3) {
        let [first, second, third] = std::array::from_fn(|i| chunk.get(i).copied().unwrap_or(0));
        out.write_all(&[
            BASE64[(first >> 2) as usize],
            BASE64[((first & 3) << 4 | second >> 4) as usize],
            if chunk.len() > 1 {
                BASE64[((second & 15) << 2 | third >> 6) as usize]
            } else {
                b'='
            },
            if chunk.len() > 2 {
                BASE64[(third & 63) as usize]
            } else {
                b'='
            },
        ])?;
    }
    out.write_all(b"\x07")
}
