use grep_regex::RegexMatcherBuilder;
use grep_searcher::{sinks, BinaryDetection, SearcherBuilder};
use helix_core::{movement::Direction, Position, RopeReader, Selection};
use helix_loader::find_workspace;
use helix_view::{
    align_view,
    graphics::{CursorKind, Rect},
    input::Event,
    Align, Editor,
};
use ignore::{DirEntry, WalkBuilder, WalkState};
use nucleo::Status;
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, BorderType, Borders, Row, Widget},
};

use super::{Injector, Picker, MIN_AREA_WIDTH_FOR_PREVIEW};
use crate::{
    compositor::{Component, Context, EventResult},
    ctrl, filter_picker_entry,
    job::Callback,
    key, shift,
    ui::{self, menu::Item, overlay::overlaid, Prompt},
};

/// Called whenever the search query string changes.
/// The callback should return a future that injects new options into the picker.
/// Atomic bool is used to signal that the callback should be cancelled,
/// as a new callback should be created.
pub type DynQueryCallback<T> = Box<dyn Fn(&str, &mut Editor, &Injector<T>, Arc<AtomicBool>)>;

/// A picker that updates its contents via a callback whenever the
/// query string changes. Useful for live grep, workspace symbols, etc.
pub struct InteractivePicker<T: ui::menu::Item + Send + Sync> {
    pub file_picker: Picker<T>,
    search_focused: bool,
    search_query_callback: DynQueryCallback<T>,
    search_prompt: Prompt,
    previous_search: String,
    cancel_notify: Arc<AtomicBool>,
}

impl<T: ui::menu::Item + Send + Sync> InteractivePicker<T> {
    pub fn new(
        mut file_picker: Picker<T>,
        search_focused: bool,
        search_query_callback: DynQueryCallback<T>,
    ) -> Self {
        let search_prompt = Prompt::new(
            "search:".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: ui::PromptEvent| {},
        );

        file_picker.prompt.set_prompt("path:".into());

        Self {
            file_picker,
            search_focused,
            search_query_callback,
            search_prompt,
            previous_search: String::new(),
            cancel_notify: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn render_picker(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let status = self.file_picker.matcher.tick(10);
        let snapshot = self.file_picker.matcher.snapshot();
        if status.changed {
            self.file_picker.cursor = self
                .file_picker
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1))
        }

        // -- Render the frame:
        // clear area
        let background = cx.editor.theme.get("ui.background");
        surface.clear_with(area, background);

        // don't like this but the lifetime sucks
        let block = Block::default().borders(Borders::ALL);

        // calculate the inner area inside the box
        let inner = block.inner(area);

        block.render(area, surface);

        // -- Render the search input bar:
        self.render_search_prompt(inner, surface, status, cx);

        // -- render file picker
        self.file_picker
            .render_picker_inner(inner.clip_top(2), surface, status, cx);
    }

    pub fn render_search_prompt(
        &mut self,
        area: Rect,
        surface: &mut Surface,
        status: Status,
        cx: &mut Context,
    ) {
        let text_style = cx.editor.theme.get("ui.text");
        let snapshot = self.file_picker.matcher.snapshot();

        let inner = area;
        let area = inner.clip_left(1).with_height(1);
        // render the prompt first since it will clear its background
        self.search_prompt.render(area, surface, cx);

        let count = format!(
            "{}{}/{}",
            if status.running { "(running) " } else { "" },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );
        surface.set_stringn(
            (area.x + area.width).saturating_sub(count.len() as u16 + 1),
            area.y,
            &count,
            (count.len()).min(area.width as usize),
            text_style,
        );

        // -- Separator
        let sep_style = cx.editor.theme.get("ui.background.separator");
        let borders = BorderType::line_symbols(BorderType::Plain);
        for x in inner.left()..inner.right() {
            if let Some(cell) = surface.get_mut(x, inner.y + 1) {
                cell.set_symbol(borders.horizontal).set_style(sep_style);
            }
        }
    }
}

impl<T: Item + Send + Sync + 'static> Component for InteractivePicker<T> {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        // +---------+ +---------+
        // |search   | |preview  |
        // +---------+ |         |
        // |prompt   | |         |
        // +---------+ |         |
        // |picker   | |         |
        // |         | |         |
        // +---------+ +---------+

        let render_preview = self.file_picker.show_preview
            && self.file_picker.file_fn.is_some()
            && area.width > MIN_AREA_WIDTH_FOR_PREVIEW;

        let picker_width = if render_preview {
            area.width / 2
        } else {
            area.width
        };

        let picker_area = area.with_width(picker_width);

        self.render_picker(picker_area, surface, cx);

        if render_preview {
            let preview_area = area.clip_left(picker_width);
            self.file_picker.render_preview(preview_area, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let Event::Key(key_event) = event {
            match key_event {
                key!(Tab) | shift!(Tab) => {
                    self.search_focused = !self.search_focused;
                    return EventResult::Consumed(None);
                }
                ctrl!('j') => {
                    // So that idle timeout retriggers
                    cx.editor.reset_idle_timer();
                    self.file_picker.move_by(1, Direction::Forward);
                    return EventResult::Consumed(None);
                }
                ctrl!('k') => {
                    // So that idle timeout retriggers
                    cx.editor.reset_idle_timer();
                    self.file_picker.move_by(1, Direction::Backward);
                    return EventResult::Consumed(None);
                }
                _ => {}
            }
        }

        if !self.search_focused {
            return self.file_picker.handle_event(event, cx);
        }

        // it's ok to call every time because events that should be ignored get ignored anyways.
        let event_result = self.search_prompt.handle_event(event, cx);
        let event_result = self
            .file_picker
            .handle_event_inner(event, cx, move |_, _, _| event_result);

        let current_search = self.search_prompt.line();

        if self.previous_search == *current_search {
            return event_result;
        }

        self.previous_search.clone_from(current_search);

        self.file_picker.matcher.restart(false);

        // if picker is brought back by last_picker,
        // shutdown should be set to false again for new injectors
        self.file_picker.shutdown = Arc::new(AtomicBool::new(false));

        let injector = self.file_picker.injector();

        // notify callback that it should be cancelled
        self.cancel_notify
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.cancel_notify = Arc::new(AtomicBool::new(false));

        let _new_options = (self.search_query_callback)(
            &current_search,
            cx.editor,
            &injector,
            self.cancel_notify.clone(),
        );

        cx.jobs.callback(async move {
            let callback = Callback::EditorCompositor(Box::new(move |editor, _compositor| {
                editor.reset_idle_timer();
            }));
            anyhow::Ok(callback)
        });

        event_result
    }

    fn cursor(&self, area: Rect, ctx: &Editor) -> (Option<Position>, CursorKind) {
        if self.search_focused {
            let block = Block::default().borders(Borders::ALL);
            // calculate the inner area inside the box
            let inner = block.inner(area);

            // prompt area
            let area = inner.clip_left(1).with_height(1);

            self.search_prompt.cursor(area, ctx)
        } else {
            self.file_picker.cursor(area.clip_top(2), ctx)
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        self.file_picker.required_size(viewport)
    }

    fn id(&self) -> Option<&'static str> {
        Some(super::ID)
    }
}

#[derive(Debug)]
pub struct FileResult {
    path: PathBuf,
    /// 0 indexed lines
    line_num: Option<usize>,
}

impl FileResult {
    fn new(path: &Path, line_num: Option<usize>) -> Self {
        Self {
            path: path.to_path_buf(),
            line_num,
        }
    }
}

impl ui::menu::Item for FileResult {
    type Data = Option<PathBuf>;

    fn format(&self, current_path: &Self::Data) -> Row {
        let relative_path = helix_stdx::path::get_relative_path(&self.path)
            .to_string_lossy()
            .into_owned();
        if current_path
            .as_ref()
            .map(|p| p == &self.path)
            .unwrap_or(false)
        {
            format!("{} (*)", relative_path).into()
        } else {
            relative_path.into()
        }
    }
}

pub fn spawn_interactive_search(search_focused: bool, cx: &mut crate::commands::Context) {
    let config = cx.editor.config();
    let file_picker_config = &config.file_picker;
    let dedup_symlinks = file_picker_config.deduplicate_links;

    let root = find_workspace().0;
    if !root.exists() {
        cx.editor.set_error("Workspace directory does not exist");
        return;
    }
    let absolute_root = root.canonicalize().unwrap_or_else(|_| root.clone());

    let current_path = doc_mut!(cx.editor).path().cloned();

    let picker = Picker::new(
        Vec::new(),
        current_path,
        move |cx, FileResult { path, line_num }, action| {
            let doc = match cx.editor.open(path, action) {
                Ok(id) => doc_mut!(cx.editor, &id),
                Err(e) => {
                    cx.editor
                        .set_error(format!("Failed to open file '{}': {}", path.display(), e));
                    return;
                }
            };

            let Some(line_num) = line_num.as_ref().copied() else { return } ;

            let view = view_mut!(cx.editor);
            let text = doc.text();
            if line_num >= text.len_lines() {
                cx.editor.set_error(
                    "The line you jumped to does not exist anymore because the file has changed.",
                );
                return;
            }
            let start = text.line_to_char(line_num);
            let end = text.line_to_char((line_num + 1).min(text.len_lines()));

            doc.set_selection(view.id, Selection::single(start, end));
            if action.align_view(view, doc.id()) {
                align_view(doc, view, Align::Center);
            }
        },
    )
    .with_preview(|_editor, FileResult { path, line_num }| {
        Some((
            path.clone().into(),
            line_num.as_ref().copied().map(|l| ((l, l))),
        ))
    });

    let mut walk_builder = WalkBuilder::new(root);

    walk_builder
        .hidden(file_picker_config.hidden)
        .parents(file_picker_config.parents)
        .ignore(file_picker_config.ignore)
        .follow_links(file_picker_config.follow_symlinks)
        .git_ignore(file_picker_config.git_ignore)
        .git_global(file_picker_config.git_global)
        .git_exclude(file_picker_config.git_exclude)
        .max_depth(file_picker_config.max_depth)
        .filter_entry(move |entry| filter_picker_entry(entry, &absolute_root, dedup_symlinks));

    let mut files = walk_builder.build().filter_map(|entry| {
        let entry = entry.ok()?;
        if !entry.file_type()?.is_file() {
            return None;
        }
        Some(entry.into_path())
    });
    let timeout = std::time::Instant::now() + std::time::Duration::from_millis(30);

    let injector = picker.injector();

    let mut hit_timeout = false;
    for file in &mut files {
        if injector.push(FileResult::new(&file, None)).is_err() {
            break;
        }
        if std::time::Instant::now() >= timeout {
            hit_timeout = true;
            break;
        }
    }
    if hit_timeout {
        let injector_ = injector.clone();

        std::thread::spawn(move || {
            for file in files {
                if injector_.push(FileResult::new(&file, None)).is_err() {
                    break;
                }
            }
        });
    }

    let interactive = InteractivePicker::new(picker, search_focused, Box::new(dyn_search_callback));

    cx.push_layer(Box::new(overlaid(interactive)));
}

pub fn dyn_search_callback(
    search_query: &str,
    editor: &mut Editor,
    injector: &Injector<FileResult>,
    should_cancel: Arc<AtomicBool>,
) {
    let documents: Vec<_> = editor
        .documents()
        .map(|doc| (doc.path().cloned(), doc.text().to_owned()))
        .collect();

    let smart_case = editor.config().search.smart_case;

    if let Ok(matcher) = RegexMatcherBuilder::new()
        .case_smart(smart_case)
        .build(search_query)
    {
        let search_root = helix_stdx::env::current_working_dir();
        if !search_root.exists() {
            editor.set_error("Current working directory does not exist");
            return;
        }

        let config = editor.config();
        let file_picker_config = &config.file_picker;
        let dedup_symlinks = file_picker_config.deduplicate_links;
        let absolute_root = search_root
            .canonicalize()
            .unwrap_or_else(|_| search_root.clone());
        let injector_ = injector.clone();

        let mut walk_builder = WalkBuilder::new(search_root);

        walk_builder
            .hidden(file_picker_config.hidden)
            .parents(file_picker_config.parents)
            .ignore(file_picker_config.ignore)
            .follow_links(file_picker_config.follow_symlinks)
            .git_ignore(file_picker_config.git_ignore)
            .git_global(file_picker_config.git_global)
            .git_exclude(file_picker_config.git_exclude)
            .max_depth(file_picker_config.max_depth)
            .filter_entry(move |entry| filter_picker_entry(entry, &absolute_root, dedup_symlinks));

        // if search query len is 0 or 1, just search all files in the workspace
        if search_query.len() < 2 {
            let mut files = walk_builder.build().filter_map(|entry| {
                let entry = entry.ok()?;
                if !entry.file_type()?.is_file() {
                    return None;
                }
                Some(entry.into_path())
            });
            let timeout = std::time::Instant::now() + std::time::Duration::from_millis(30);

            let mut hit_timeout = false;
            for file in &mut files {
                if injector.push(FileResult::new(&file, None)).is_err() {
                    break;
                }
                if std::time::Instant::now() >= timeout {
                    hit_timeout = true;
                    break;
                }
            }
            if hit_timeout {
                let injector_ = injector.clone();

                std::thread::spawn(move || {
                    for file in files {
                        if injector_.push(FileResult::new(&file, None)).is_err() {
                            break;
                        }
                    }
                });
            }
            return;
        }

        std::thread::spawn(move || {
            let searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();

            walk_builder.add_custom_ignore_filename(helix_loader::config_dir().join("ignore"));
            walk_builder.add_custom_ignore_filename(".helix/ignore");

            walk_builder.build_parallel().run(|| {
                let mut searcher = searcher.clone();
                let matcher = matcher.clone();
                let injector = injector_.clone();
                let documents = &documents;
                let should_cancel_ = should_cancel.clone();

                Box::new(move |entry: Result<DirEntry, ignore::Error>| -> WalkState {
                    if should_cancel_.load(std::sync::atomic::Ordering::Relaxed) {
                        return WalkState::Quit;
                    }

                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => return WalkState::Continue,
                    };

                    match entry.file_type() {
                        Some(entry) if entry.is_file() => {}
                        // skip everything else
                        _ => return WalkState::Continue,
                    };

                    let mut stop = false;
                    let sink = sinks::UTF8(|line_num, _| {
                        stop = injector
                            .push(FileResult::new(entry.path(), Some(line_num as usize - 1)))
                            .is_err();

                        Ok(!stop)
                    });
                    let doc = documents.iter().find(|&(doc_path, _)| {
                        doc_path
                            .as_ref()
                            .map_or(false, |doc_path| doc_path == entry.path())
                    });

                    let result = if let Some((_, doc)) = doc {
                        // there is already a buffer for this file
                        // search the buffer instead of the file because it's faster
                        // and captures new edits without requiring a save
                        if searcher.multi_line_with_matcher(&matcher) {
                            // in this case a continous buffer is required
                            // convert the rope to a string
                            let text = doc.to_string();
                            searcher.search_slice(&matcher, text.as_bytes(), sink)
                        } else {
                            searcher.search_reader(&matcher, RopeReader::new(doc.slice(..)), sink)
                        }
                    } else {
                        searcher.search_path(&matcher, entry.path(), sink)
                    };

                    if let Err(err) = result {
                        log::error!("Global search error: {}, {}", entry.path().display(), err);
                    }
                    if stop {
                        WalkState::Quit
                    } else {
                        WalkState::Continue
                    }
                })
            });
        });
    } else {
        // Otherwise do nothing
        // log::warn!("Global Search Invalid Pattern")
    }
}
