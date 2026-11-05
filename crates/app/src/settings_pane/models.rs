//! Settings › Models: the local runtimes on this Mac, the models Ollama has,
//! pulling more, and which model each of Deck's own AI jobs runs on.
//!
//! Anything that talks to a server runs on the background executor (a pull
//! on a thread of its own); drawing only reads the last survey, taken every
//! few seconds while the page is on screen.

use super::{button, group, row, Page, Row, Section, SettingsPane};
use crate::field::{Field, FieldEvent};
use crate::icon::{icon, Icon};
use crate::local::{self, Kind, Route, Survey};
use crate::menu::{self, Entry, Item};
use crate::refine;
use crate::theme::{self, u, OneLine};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    div, relative, rgb, rgba, AnyElement, Context, Div, ElementId, Entity, MouseButton,
    MouseDownEvent, Pixels, Point, SharedString, Stateful, Subscription, Window,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RUNTIMES: &str = "Runtimes";
const INSTALLED: &str = "Installed models";
const GET: &str = "Get a model";
const ROUTING: &str = "Routing";

/// One click away, sized for a Mac laptop: the name, what it is good for,
/// and its download.
const SUGGESTED: [(&str, &str, &str); 5] = [
    ("qwen2.5-coder:7b", "Code and shell commands", "4.7 GB"),
    (
        "llama3.2:3b",
        "Quick and light, fine for rewriting prompts",
        "2.0 GB",
    ),
    ("gemma3:4b", "A good small all-rounder", "3.3 GB"),
    (
        "qwen3:8b",
        "A stronger all-rounder; wants 16 GB of memory",
        "5.2 GB",
    ),
    ("deepseek-r1:8b", "Reasons step by step; slower", "5.2 GB"),
];

/// The page's own state.
pub(super) struct Models {
    /// The last look at runtimes and models; `None` before the first.
    survey: Option<Survey>,
    /// The every-few-seconds survey is running.
    polling: bool,
    /// When the page last drew, to tell whether it is still on screen.
    seen: Instant,
    /// Model → what is being done to it ("Loading…").
    busy: HashMap<String, &'static str>,
    /// The model whose Delete was clicked once.
    confirm: Option<String>,
    starting: bool,
    pull: Option<Pull>,
    /// How the last pull ended.
    pulled: Option<SharedString>,
    /// The last action that failed: its section's title and why.
    failed: Option<(&'static str, SharedString)>,
    field: Entity<Field>,
    tests: HashMap<&'static str, Test>,
    /// The route picker, while open.
    pub(super) menu: Option<menu::Open>,
    _field: Subscription,
}

impl Models {
    pub(super) fn new(cx: &mut Context<SettingsPane>) -> Self {
        let field = cx.new(|cx| Field::new("qwen2.5-coder:7b", cx));
        let _field = cx.subscribe(&field, |p: &mut SettingsPane, _, ev: &FieldEvent, cx| {
            if let FieldEvent::Submit(name) = ev {
                p.pull(name.clone(), cx);
            }
        });
        Self {
            survey: None,
            polling: false,
            seen: Instant::now(),
            busy: HashMap::new(),
            confirm: None,
            starting: false,
            pull: None,
            pulled: None,
            failed: None,
            field,
            tests: HashMap::new(),
            menu: None,
            _field,
        }
    }
}

/// A pull under way. Dropping it (the tab closing) stops it.
struct Pull {
    name: String,
    progress: local::Progress,
    cancel: Arc<AtomicBool>,
}

impl Drop for Pull {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

enum Test {
    Running,
    Done(Result<(String, Duration), String>),
}

#[derive(Clone, Copy)]
enum Act {
    Load,
    Unload,
    Delete,
}

impl Act {
    fn doing(self) -> &'static str {
        match self {
            Act::Load => "Loading…",
            Act::Unload => "Unloading…",
            Act::Delete => "Deleting…",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Act::Load => "load",
            Act::Unload => "unload",
            Act::Delete => "delete",
        }
    }

    fn run(self, name: &str) -> Result<(), String> {
        match self {
            Act::Load => local::load(name),
            Act::Unload => local::unload(name),
            Act::Delete => local::delete(name),
        }
    }
}

impl SettingsPane {
    /// The page drew: keep surveying every few seconds while it stays on
    /// screen. A pane out of view does not draw, so the loop stops itself.
    pub(super) fn watch_models(&mut self, cx: &mut Context<Self>) {
        self.models.seen = Instant::now();
        if self.models.polling {
            return;
        }
        self.models.polling = true;
        cx.spawn(async move |this, cx| loop {
            let survey = cx
                .background_executor()
                .spawn(async { local::survey() })
                .await;
            let drawn = this.update(cx, |p, cx| {
                p.models.survey = Some(survey);
                cx.notify();
            });
            if drawn.is_err() {
                return;
            }
            cx.background_executor().timer(Duration::from_secs(3)).await;
            let again = this.update(cx, |p, _| {
                let on = p.page == Page::Models && p.models.seen.elapsed() < Duration::from_secs(4);
                p.models.polling = on;
                on
            });
            if !again.unwrap_or(false) {
                return;
            }
        })
        .detach();
    }

    fn start_ollama(&mut self, cx: &mut Context<Self>) {
        if self.models.starting {
            return;
        }
        self.models.starting = true;
        self.models.failed = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let got = cx
                .background_executor()
                .spawn(async { local::start(Kind::Ollama) })
                .await;
            let survey = cx
                .background_executor()
                .spawn(async { local::survey() })
                .await;
            let _ = this.update(cx, |p, cx| {
                p.models.starting = false;
                p.models.failed = got.err().map(|e| (RUNTIMES, sentence(&e).into()));
                p.models.survey = Some(survey);
                cx.notify();
            });
        })
        .detach();
    }

    fn act(&mut self, name: String, what: Act, cx: &mut Context<Self>) {
        if self.models.busy.contains_key(&name) {
            return;
        }
        self.models.confirm = None;
        self.models.failed = None;
        self.models.busy.insert(name.clone(), what.doing());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let n = name.clone();
            let got = cx
                .background_executor()
                .spawn(async move { what.run(&n) })
                .await;
            let survey = cx
                .background_executor()
                .spawn(async { local::survey() })
                .await;
            let _ = this.update(cx, |p, cx| {
                p.models.busy.remove(&name);
                if let Err(e) = got {
                    let why = format!("Couldn't {} {name}: {}", what.verb(), sentence(&e));
                    p.models.failed = Some((INSTALLED, why.into()));
                }
                p.models.survey = Some(survey);
                cx.notify();
            });
        })
        .detach();
    }

    /// Delete asks once: the first click arms it, the second deletes.
    fn delete_clicked(&mut self, name: String, cx: &mut Context<Self>) {
        if self.models.confirm.as_deref() == Some(name.as_str()) {
            self.act(name, Act::Delete, cx);
        } else {
            self.models.confirm = Some(name);
            cx.notify();
        }
    }

    fn pull(&mut self, name: String, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() || self.models.pull.is_some() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<local::Progress>();
        let (done, finished) = futures::channel::oneshot::channel();
        let (n, stop) = (name.clone(), cancel.clone());
        // Minutes of blocking reads: a thread of its own, not the pool's.
        std::thread::spawn(move || {
            let mut sent: Option<Instant> = None;
            let got = local::pull(&n, &stop, |p| {
                if p.done || sent.is_none_or(|t| t.elapsed() >= Duration::from_millis(100)) {
                    sent = Some(Instant::now());
                    let _ = tx.unbounded_send(p.clone());
                }
            });
            let _ = done.send(got);
        });
        self.models.pull = Some(Pull {
            name: name.clone(),
            progress: local::Progress::default(),
            cancel,
        });
        self.models.pulled = None;
        self.models.field.update(cx, |f, cx| f.set_text("", cx));
        cx.notify();
        cx.spawn(async move |this, cx| {
            while let Some(p) = rx.next().await {
                let shown = this.update(cx, |pane, cx| {
                    if let Some(pull) = pane.models.pull.as_mut() {
                        pull.progress = p;
                    }
                    cx.notify();
                });
                if shown.is_err() {
                    return;
                }
            }
            let got = finished
                .await
                .unwrap_or_else(|_| Err("the pull stopped".into()));
            let survey = cx
                .background_executor()
                .spawn(async { local::survey() })
                .await;
            let _ = this.update(cx, |pane, cx| {
                pane.models.pull = None;
                pane.models.pulled = Some(
                    match got {
                        Ok(()) => format!("Pulled {name}."),
                        Err(e) if e == "stopped" => format!(
                            "Stopped pulling {name}. Pulling it again picks up where it left off."
                        ),
                        Err(e) => format!("Couldn't pull {name}: {}", sentence(&e)),
                    }
                    .into(),
                );
                pane.models.survey = Some(survey);
                cx.notify();
            });
        })
        .detach();
    }

    fn test_route(&mut self, job: &'static str, cx: &mut Context<Self>) {
        if matches!(self.models.tests.get(job), Some(Test::Running)) {
            return;
        }
        let route = refine::chosen(&self.settings, job);
        self.models.tests.insert(job, Test::Running);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let got = cx
                .background_executor()
                .spawn(async move { refine::test(&route) })
                .await;
            let _ = this.update(cx, |p, cx| {
                p.models.tests.insert(job, Test::Done(got));
                cx.notify();
            });
        })
        .detach();
    }

    fn pick_route(&mut self, job: &'static str, route: Route, cx: &mut Context<Self>) {
        self.models.tests.remove(job);
        self.change(cx, |s| refine::choose(s, job, route));
    }

    /// Every local model a job could run on now.
    fn local_routes(&self) -> Vec<Route> {
        let Some(s) = &self.models.survey else {
            return Vec::new();
        };
        s.models
            .iter()
            .filter(|m| m.chats())
            .map(|m| Route::Local(Kind::Ollama, m.name.clone()))
            .chain(s.served.iter().map(|(k, m)| Route::Local(*k, m.clone())))
            .collect()
    }

    fn open_routes(
        &mut self,
        job: &'static str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = refine::chosen(&self.settings, job);
        let this = cx.entity().downgrade();
        let pick = |route: Route, label: String| -> Entry {
            let on = route == current;
            let this = this.clone();
            let item = Item::new(label, move |_, cx| {
                let r = route.clone();
                let _ = this.update(cx, |p, cx| p.pick_route(job, r, cx));
            });
            if on { item.icon(Icon::Check) } else { item }.into()
        };
        let mut entries: Vec<Entry> = refine::MODELS
            .iter()
            .map(|(id, _)| {
                let r = Route::Claude(id.to_string());
                let label = r.label();
                pick(r, label)
            })
            .collect();
        entries.push(Entry::Separator);
        let locals = self.local_routes();
        if locals.is_empty() {
            entries.push(
                Item::new("No local models yet", |_, _| {})
                    .disabled(true)
                    .into(),
            );
        }
        for r in &locals {
            entries.push(pick(r.clone(), r.label()));
        }
        // The choice stays visible after its model went away.
        if matches!(current, Route::Local(..)) && !locals.contains(&current) {
            entries.push(pick(
                current.clone(),
                format!("{} (not here)", current.label()),
            ));
        }
        entries.push(Entry::Separator);
        entries.push(pick(Route::Off, "Off".into()));
        self.models.menu = Some(menu::open(
            entries,
            at,
            Some(self.focus.clone()),
            window,
            cx,
            |p: &mut Self, _, _| p.models.menu = None,
        ));
        cx.notify();
    }

    // -- the page -----------------------------------------------------------------

    pub(super) fn page_models(&self, cx: &mut Context<Self>) -> Vec<Section> {
        let none = Survey::default();
        let s = self.models.survey.as_ref().unwrap_or(&none);
        let looking = self.models.survey.is_none();
        vec![
            self.runtimes_section(s, looking, cx),
            self.installed_section(s, looking, cx),
            self.get_section(s, looking, cx),
            self.routing_section(s, cx),
        ]
    }

    fn failed_in(&self, section: &'static str) -> Option<SharedString> {
        self.models
            .failed
            .as_ref()
            .filter(|(at, _)| *at == section)
            .map(|(_, why)| why.clone())
    }

    fn runtimes_section(&self, s: &Survey, looking: bool, cx: &mut Context<Self>) -> Section {
        if looking {
            return group(
                RUNTIMES,
                vec![row("Looking for runtimes…", nothing())],
                None,
            );
        }
        let rows = s
            .runtimes
            .iter()
            .map(|r| {
                let dot = div().flex_none().size(u(7.)).rounded_full();
                let dot = if r.running {
                    dot.bg(rgb(green()))
                } else if r.installed {
                    dot.bg(rgb(theme::dim()))
                } else {
                    dot.border_1().border_color(rgba(theme::input_edge()))
                };
                let mut control = div().flex().items_center().gap(u(12.)).child(
                    div()
                        .flex()
                        .items_center()
                        .gap(u(6.))
                        .text_size(u(12.))
                        .text_color(rgb(theme::text_2()))
                        .child(dot)
                        .child(r.status()),
                );
                if r.kind == Kind::Ollama && r.installed && !r.running {
                    control = control.child(if self.models.starting {
                        quiet("Starting…")
                    } else {
                        button("ollama-start", "Start")
                            .on_click(cx.listener(|p, _, _, cx| p.start_ollama(cx)))
                            .into_any_element()
                    });
                }
                let about = if r.running {
                    r.url.trim_start_matches("http://").to_string()
                } else if r.installed {
                    r.kind.start_hint().to_string()
                } else {
                    format!("To install: {}.", r.kind.install_hint())
                };
                row(r.name, control.into_any_element()).about(about)
            })
            .collect();
        group(RUNTIMES, rows, self.failed_in(RUNTIMES))
    }

    fn installed_section(&self, s: &Survey, looking: bool, cx: &mut Context<Self>) -> Section {
        let ollama = s.runtimes.iter().find(|r| r.kind == Kind::Ollama);
        let mut rows = Vec::new();
        if looking {
            rows.push(row("Looking…", nothing()));
        } else if !s.running(Kind::Ollama) && s.served.is_empty() {
            rows.push(if ollama.is_some_and(|r| r.installed) {
                row("Ollama isn't running", nothing()).about("Start it above to see its models.")
            } else {
                row("No local models yet", nothing()).about(
                    "Install Ollama to run models on this Mac. Nothing you send one leaves it.",
                )
            });
        } else {
            for (ix, m) in s.models.iter().enumerate() {
                rows.push(self.model_row(ix, m, s, cx));
            }
            for (kind, id) in &s.served {
                rows.push(row(id.clone(), nothing()).about(format!("In {}", kind.name())));
            }
            if rows.is_empty() {
                rows.push(
                    row("No local models yet", self.pull_field(cx))
                        .about("Pull one by name, or pick a suggestion below."),
                );
            }
        }
        let note = s
            .error
            .as_ref()
            .map(|e| SharedString::from(sentence(e)))
            .or_else(|| self.failed_in(INSTALLED));
        group(INSTALLED, rows, note)
    }

    fn model_row(&self, ix: usize, m: &local::Model, s: &Survey, cx: &mut Context<Self>) -> Row {
        let loaded = s.loaded.iter().find(|l| l.name == m.name);
        let mut control = div().flex().items_center().gap(u(8.));
        if let Some(l) = loaded {
            control = control.child(badge(l.badge(local::now())));
        }
        match self.models.busy.get(&m.name) {
            Some(doing) => control = control.child(quiet(*doing)),
            None => {
                if m.chats() {
                    let (label, act) = if loaded.is_some() {
                        ("Unload", Act::Unload)
                    } else {
                        ("Load", Act::Load)
                    };
                    let name = m.name.clone();
                    control = control.child(
                        button(
                            ElementId::NamedInteger("model-load".into(), ix as u64),
                            label,
                        )
                        .on_click(cx.listener(move |p, _, _, cx| p.act(name.clone(), act, cx))),
                    );
                }
                let armed = self.models.confirm.as_deref() == Some(m.name.as_str());
                let name = m.name.clone();
                control = control.child(
                    button(
                        ElementId::NamedInteger("model-delete".into(), ix as u64),
                        if armed {
                            "Click Again to Delete"
                        } else {
                            "Delete"
                        },
                    )
                    .when(armed, |b| b.text_color(rgb(theme::DANGER)))
                    .on_click(cx.listener(move |p, _, _, cx| p.delete_clicked(name.clone(), cx))),
                );
            }
        }
        let mut about = m.detail();
        if !m.chats() {
            about.push_str(" · embeddings only");
        }
        row(m.name.clone(), control.into_any_element()).about(about)
    }

    fn pull_field(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(u(8.))
            .child(div().w(u(200.)).child(self.models.field.clone()))
            .when(self.models.pull.is_none(), |d| {
                d.child(button("pull", "Pull").on_click(cx.listener(|p, _, _, cx| {
                    let name = p.models.field.read(cx).text().to_string();
                    p.pull(name, cx);
                })))
            })
            .into_any_element()
    }

    fn get_section(&self, s: &Survey, looking: bool, cx: &mut Context<Self>) -> Section {
        let up = s.running(Kind::Ollama);
        let mut rows = Vec::new();
        if let Some(p) = &self.models.pull {
            let stopping = p.cancel.load(Ordering::Relaxed);
            let control = div()
                .flex()
                .items_center()
                .gap(u(12.))
                .child(bar(p.progress.fraction()))
                .child(if stopping {
                    quiet("Stopping…")
                } else {
                    button("pull-cancel", "Cancel")
                        .on_click(cx.listener(|p, _, _, cx| {
                            if let Some(pull) = &p.models.pull {
                                pull.cancel.store(true, Ordering::Relaxed);
                            }
                            cx.notify();
                        }))
                        .into_any_element()
                });
            rows.push(
                row(format!("Pulling {}", p.name), control.into_any_element())
                    .about(p.progress.describe()),
            );
        }
        // With none installed, the field sits in the empty list above.
        let empty = s.models.is_empty() && s.served.is_empty();
        if up && !empty {
            rows.push(
                row("Pull by name", self.pull_field(cx))
                    .about("Any model from ollama.com/library, like qwen2.5-coder:7b."),
            );
        }
        for (ix, (name, good_for, size)) in SUGGESTED.iter().enumerate() {
            let have = s.models.iter().any(|m| local::same_model(&m.name, name));
            let control = if have {
                div()
                    .flex()
                    .items_center()
                    .gap(u(6.))
                    .text_size(u(12.))
                    .text_color(rgb(theme::dim()))
                    .child(icon(Icon::Check, 12., theme::dim()))
                    .child("Installed")
                    .into_any_element()
            } else if up && self.models.pull.is_none() {
                let name = name.to_string();
                button(ElementId::NamedInteger("suggest".into(), ix as u64), "Pull")
                    .on_click(cx.listener(move |p, _, _, cx| p.pull(name.clone(), cx)))
                    .into_any_element()
            } else {
                nothing()
            };
            rows.push(row(*name, control).about(format!("{good_for}. {size}.")));
        }
        let note =
            self.models.pulled.clone().or_else(|| {
                (!looking && !up).then(|| "Pulling a model needs Ollama running.".into())
            });
        group(GET, rows, note)
    }

    fn routing_section(&self, s: &Survey, cx: &mut Context<Self>) -> Section {
        let rows = refine::JOBS
            .iter()
            .enumerate()
            .map(|(ix, (job, name, does))| {
                let job: &'static str = job;
                let current = refine::chosen(&self.settings, job);
                let local = matches!(current, Route::Local(..));
                let gone = local && self.models.survey.is_some() && !s.has(&current);
                let label = if gone {
                    format!("{} (not here)", current.label())
                } else {
                    current.label()
                };
                let picker = picker(ElementId::NamedInteger("route".into(), ix as u64), label)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |p, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            p.open_routes(job, ev.position, window, cx);
                        }),
                    );
                let test = match self.models.tests.get(job) {
                    Some(Test::Running) => quiet("Testing…"),
                    _ if current == Route::Off => nothing(),
                    _ => button(
                        ElementId::NamedInteger("route-test".into(), ix as u64),
                        "Test",
                    )
                    .on_click(cx.listener(move |p, _, _, cx| p.test_route(job, cx)))
                    .into_any_element(),
                };
                let mut about = vec![does.to_string()];
                match self.models.tests.get(job) {
                    Some(Test::Done(Ok((reply, took)))) => about.push(format!(
                        "Answered in {:.1} s: “{reply}”.",
                        took.as_secs_f32()
                    )),
                    Some(Test::Done(Err(e))) => {
                        about.push(format!("The test failed: {}", sentence(e)))
                    }
                    _ => {}
                }
                if let Some((why, ago)) = refine::fallback(job).filter(|_| local) {
                    about.push(format!(
                        "Fell back to Claude {}: {}",
                        ago_words(ago),
                        sentence(&why)
                    ));
                }
                let control = div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .child(picker)
                    .child(test);
                row(*name, control.into_any_element()).about(about.join(" "))
            })
            .collect();
        let claude = Route::Claude(self.settings.refine_model.clone()).label();
        group(
            ROUTING,
            rows,
            Some(format!("A local model that can't answer (its server is down, the model is gone) hands the job to {claude}, and the reason shows under it.").into()),
        )
    }
}

// -- pieces -----------------------------------------------------------------------

fn nothing() -> AnyElement {
    div().into_any_element()
}

/// The status bar's green, for what is running.
fn green() -> u32 {
    theme::palette().ansi[2]
}

/// A word or two in place of a button: "Loading…".
fn quiet(text: impl Into<SharedString>) -> AnyElement {
    div()
        .text_size(u(12.))
        .text_color(rgb(theme::dim()))
        .child(text.into())
        .into_any_element()
}

/// "Loaded · 5.1 GB · unloads in 4m", as a pill.
fn badge(text: String) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(u(6.))
        .h(u(22.))
        .px(u(8.))
        .rounded(u(6.))
        .bg(rgb(theme::surface()))
        .text_size(u(theme::TEXT_SMALL))
        .text_color(rgb(theme::text_2()))
        .child(div().size(u(6.)).rounded_full().bg(rgb(green())))
        .child(text)
        .into_any_element()
}

/// A pull's progress.
fn bar(fraction: Option<f32>) -> AnyElement {
    div()
        .w(u(160.))
        .h(u(6.))
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(theme::surface_hover()))
        .child(
            div()
                .h_full()
                .rounded_full()
                .bg(rgb(theme::text()))
                .w(relative(fraction.unwrap_or(0.).clamp(0., 1.))),
        )
        .into_any_element()
}

/// A button that opens a menu of choices, showing the current one.
fn picker(id: ElementId, label: String) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap(u(6.))
        .h(u(26.))
        .pl(u(10.))
        .pr(u(8.))
        .rounded(u(6.))
        .bg(rgb(theme::surface()))
        .border_1()
        .border_color(rgba(theme::hairline()))
        .text_size(u(12.))
        .text_color(rgb(theme::text()))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::surface_hover())))
        .child(div().max_w(u(220.)).one_line().child(label))
        .child(icon(Icon::ChevronDown, 11., theme::dim()))
}

/// "just now", "4m ago", "2h ago".
fn ago_words(d: Duration) -> String {
    match d.as_secs() {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// An error as the end of a sentence: one full stop.
fn sentence(s: &str) -> String {
    let s = s.trim().trim_end_matches('.');
    format!("{s}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_for_times_and_errors() {
        assert_eq!(ago_words(Duration::from_secs(5)), "just now");
        assert_eq!(ago_words(Duration::from_secs(250)), "4m ago");
        assert_eq!(ago_words(Duration::from_secs(7300)), "2h ago");
        assert_eq!(sentence("Ollama isn't running"), "Ollama isn't running.");
        assert_eq!(sentence("done. "), "done.");
    }

    #[test]
    fn suggestions_are_pullable_names() {
        for (name, ..) in SUGGESTED {
            assert!(
                Route::parse(&format!("ollama:{name}")).is_some(),
                "{name} makes a route"
            );
            assert!(name.contains(':'), "{name} names its size");
        }
    }
}
