use std::ops::ControlFlow;

use eyre::Result;
use semver::Version;

use atuin_client::{
    database::Context,
    database::{current_context, Database},
    history::History,
    settings::{ExitMode, FilterMode, Settings},
};

use super::super::{cursor::Cursor, history_list::ListState};

pub struct State<DB: Database> {
    pub db: DB,
    pub filter_mode: FilterMode,
    pub results_state: ListState,
    pub context: Context,
    pub input: Cursor,
    pub history: Vec<History>,
    pub history_count: i64,
    pub settings: Settings,
    pub update_needed: Option<Version>,
}

pub struct Guard<DB: Database> {
    initial_input: String,
    initial_filter_mode: FilterMode,
    inner: State<DB>,
}

#[derive(Clone)]
pub enum Line {
    Up,
    Down,
}
#[derive(Clone)]
pub enum For {
    Page,
    SingleLine,
}
#[derive(Clone)]
pub enum Towards {
    Left,
    Right,
}
#[derive(Clone)]
pub enum To {
    Word,
    Char,
    Edge,
}

#[derive(Clone)]
pub enum Event {
    Input(char),
    Selection(Line, For),
    Cursor(Towards, To),
    Delete(Towards, To),
    Clear,
    Exit,
    UpdateNeeded(Version),
    Cancel,
    SelectN(u32),
    CycleFilterMode,
}

impl<DB: Database> State<DB> {
    // this is a big blob of horrible! clean it up!
    // for now, it works. But it'd be great if it were more easily readable, and
    // modular. I'd like to add some more stats and stuff at some point
    #[allow(clippy::cast_possible_truncation)]
    pub async fn new(query: &[String], settings: Settings, db: DB) -> Result<Self> {
        let mut input = Cursor::from(query.join(" "));
        // Put the cursor at the end of the query by default
        input.end();

        let mut core = Self {
            history_count: db.history_count().await?,
            input,
            results_state: ListState::default(),
            context: current_context(),
            filter_mode: if settings.shell_up_key_binding {
                settings
                    .filter_mode_shell_up_key_binding
                    .unwrap_or(settings.filter_mode)
            } else {
                settings.filter_mode
            },
            update_needed: None,
            history: Vec::new(),
            db,
            settings,
        };
        core.refresh_query().await?;
        Ok(core)
    }
    pub async fn refresh_query(&mut self) -> Result<()> {
        let i = self.input.as_str();
        self.history = if i.is_empty() {
            self.db
                .list(self.filter_mode, &self.context, Some(200), true)
                .await?
        } else {
            self.db
                .search(
                    self.settings.search_mode,
                    self.filter_mode,
                    &self.context,
                    i,
                    Some(200),
                    None,
                    None,
                )
                .await?
        };

        self.results_state.select(0);
        Ok(())
    }

    fn handle(mut self, event: Event) -> ControlFlow<String, Self> {
        let len = self.history.len();
        match event {
            // moving the selection up and down
            Event::Selection(Line::Up, For::SingleLine) => {
                let i = self.results_state.selected() + 1;
                self.results_state.select(i.min(len - 1));
            }
            Event::Selection(Line::Down, For::SingleLine) => {
                let Some(i) = self.results_state.selected().checked_sub(1) else {
                    return ControlFlow::Break(String::new())
                };
                self.results_state.select(i);
            }
            Event::Selection(Line::Down, For::Page) => {
                let scroll_len =
                    self.results_state.max_entries() - self.settings.scroll_context_lines;
                let i = self.results_state.selected().saturating_sub(scroll_len);
                self.results_state.select(i);
            }
            Event::Selection(Line::Up, For::Page) => {
                let scroll_len =
                    self.results_state.max_entries() - self.settings.scroll_context_lines;
                let i = self.results_state.selected() + scroll_len;
                self.results_state.select(i.min(len - 1));
            }

            // moving the search cursor left and right
            Event::Cursor(Towards::Left, To::Char) => {
                self.input.left();
            }
            Event::Cursor(Towards::Right, To::Char) => self.input.right(),
            Event::Cursor(Towards::Left, To::Word) => self
                .input
                .prev_word(&self.settings.word_chars, self.settings.word_jump_mode),
            Event::Cursor(Towards::Right, To::Word) => self
                .input
                .next_word(&self.settings.word_chars, self.settings.word_jump_mode),
            Event::Cursor(Towards::Left, To::Edge) => self.input.start(),
            Event::Cursor(Towards::Right, To::Edge) => self.input.end(),

            // modifying the search
            Event::Input(c) => self.input.insert(c),
            Event::Delete(Towards::Left, To::Word) => self
                .input
                .remove_prev_word(&self.settings.word_chars, self.settings.word_jump_mode),
            Event::Delete(Towards::Left, To::Char) => self.input.back(),
            Event::Delete(Towards::Left, To::Edge) => self.input.clear_from_start(),
            Event::Delete(Towards::Right, To::Word) => self
                .input
                .remove_next_word(&self.settings.word_chars, self.settings.word_jump_mode),
            Event::Delete(Towards::Right, To::Char) => self.input.remove(),
            Event::Delete(Towards::Right, To::Edge) => self.input.clear_to_end(),
            Event::Clear => self.input.clear(),

            // exiting
            Event::Cancel => return ControlFlow::Break(String::new()),
            Event::Exit => {
                return ControlFlow::Break(match self.settings.exit_mode {
                    ExitMode::ReturnOriginal => String::new(),
                    ExitMode::ReturnQuery => self.input.into_inner(),
                })
            }
            Event::SelectN(n) => {
                let i = self.results_state.selected().saturating_add(n as usize);
                return ControlFlow::Break(if i < self.history.len() {
                    self.input.into_inner()
                } else {
                    self.history.swap_remove(i).command
                });
            }

            // misc
            Event::UpdateNeeded(version) => self.update_needed = Some(version),
            Event::CycleFilterMode => {
                pub static FILTER_MODES: [FilterMode; 4] = [
                    FilterMode::Global,
                    FilterMode::Host,
                    FilterMode::Session,
                    FilterMode::Directory,
                ];
                let i = self.filter_mode as usize;
                let i = (i + 1) % FILTER_MODES.len();
                self.filter_mode = FILTER_MODES[i];
            }
        }
        ControlFlow::Continue(self)
    }

    pub fn start_batch(self) -> Guard<DB> {
        Guard {
            initial_input: self.input.as_str().to_owned(),
            initial_filter_mode: self.filter_mode,
            inner: self,
        }
    }

    pub fn view(&mut self) -> View<'_> {
        View {
            history_count: self.history_count,
            input: &self.input,
            filter_mode: self.filter_mode,
            results_state: &mut self.results_state,
            update_needed: self.update_needed.as_ref(),
            history: &self.history,
        }
    }
}

impl<DB: Database> Guard<DB> {
    pub fn handle(mut self, event: Event) -> ControlFlow<String, Self> {
        match self.inner.handle(event) {
            ControlFlow::Continue(inner) => self.inner = inner,
            ControlFlow::Break(result) => return ControlFlow::Break(result),
        }
        ControlFlow::Continue(self)
    }

    pub async fn finish(self) -> Result<(State<DB>, bool)> {
        let Self {
            initial_input,
            initial_filter_mode,
            mut inner,
        } = self;
        let should_update =
            initial_input != inner.input.as_str() || initial_filter_mode != inner.filter_mode;
        if should_update {
            inner.refresh_query().await?;
        }

        Ok((inner, should_update))
    }
}

pub struct View<'a> {
    pub history_count: i64,
    pub input: &'a Cursor,
    pub filter_mode: FilterMode,
    pub results_state: &'a mut ListState,
    pub update_needed: Option<&'a Version>,
    pub history: &'a [History],
}
