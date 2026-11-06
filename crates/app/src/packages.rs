//! The Packages tab: the project's dependencies from its own manifests
//! (package.json, Cargo.toml, pyproject.toml / requirements.txt, go.mod),
//! what is installed against the newest on the registry, and a search of
//! the registry to add one. Adding, updating or removing runs the package
//! manager as a background job, so its output is a click away in the status
//! bar. A row opens to what it depends on and what pulls it in, from the
//! lockfile, and the audit tools that are installed mark rows with known
//! advisories.

use crate::icon::{icon, Icon};
use crate::theme::{self, u, OneLine};
use gpui::prelude::*;
use gpui::{
    div, px, relative, rgb, rgba, AnyElement, Context, ElementId, Entity, EventEmitter, FontWeight, SharedString, Window,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Eco {
    Npm,
    Cargo,
    Python,
    Go,
}

impl Eco {
    fn label(self) -> &'static str {
        match self {
            Eco::Npm => "npm",
            Eco::Cargo => "crates.io",
            Eco::Python => "PyPI",
            Eco::Go => "Go",
        }
    }

    fn page(self, name: &str) -> String {
        match self {
            Eco::Npm => format!("https://www.npmjs.com/package/{name}"),
            Eco::Cargo => format!("https://crates.io/crates/{name}"),
            Eco::Python => format!("https://pypi.org/project/{name}/"),
            Eco::Go => format!("https://pkg.go.dev/{name}"),
        }
    }

    /// The registry's newest version, from its JSON.
    fn latest_url(self, name: &str) -> Option<String> {
        Some(match self {
            Eco::Npm => format!("https://registry.npmjs.org/{name}/latest"),
            Eco::Cargo => format!("https://crates.io/api/v1/crates/{name}"),
            Eco::Python => format!("https://pypi.org/pypi/{name}/json"),
            Eco::Go => format!("https://proxy.golang.org/{}/@latest", go_escape(name)),
        })
    }

    fn latest_of(self, v: &Value) -> Option<(String, Option<String>)> {
        match self {
            Eco::Npm => Some((v["version"].as_str()?.to_string(), v["description"].as_str().map(str::to_string))),
            Eco::Cargo => Some((
                v["crate"]["max_stable_version"].as_str().or(v["crate"]["max_version"].as_str())?.to_string(),
                v["crate"]["description"].as_str().map(str::to_string),
            )),
            Eco::Python => Some((v["info"]["version"].as_str()?.to_string(), v["info"]["summary"].as_str().map(str::to_string))),
            Eco::Go => Some((v["Version"].as_str()?.to_string(), None)),
        }
    }

    /// The command that adds `name` (at its newest) to the project.
    fn add_cmd(self, manager: &str, name: &str, dev: bool) -> String {
        match self {
            Eco::Npm => match manager {
                "pnpm" => format!("pnpm add {}{name}@latest", if dev { "-D " } else { "" }),
                "yarn" => format!("yarn add {}{name}@latest", if dev { "-D " } else { "" }),
                "bun" => format!("bun add {}{name}@latest", if dev { "-d " } else { "" }),
                _ => format!("npm install {}{name}@latest", if dev { "-D " } else { "" }),
            },
            Eco::Cargo => format!("cargo add {}{name}", if dev { "--dev " } else { "" }),
            Eco::Python if manager == "uv" => format!("uv add {name}"),
            Eco::Python if manager == "poetry" => format!("poetry add {name}"),
            Eco::Python => format!("pip install -U {name}"),
            Eco::Go => format!("go get {name}@latest"),
        }
    }

    /// The command that takes `name` out of the project.
    fn remove_cmd(self, manager: &str, name: &str, dev: bool) -> String {
        match self {
            Eco::Npm => match manager {
                "pnpm" => format!("pnpm remove {name}"),
                "yarn" => format!("yarn remove {name}"),
                "bun" => format!("bun remove {name}"),
                _ => format!("npm uninstall {name}"),
            },
            Eco::Cargo => format!("cargo remove {}{name}", if dev { "--dev " } else { "" }),
            Eco::Python if manager == "uv" => format!("uv remove {name}"),
            Eco::Python if manager == "poetry" => format!("poetry remove {name}"),
            Eco::Python => format!("pip uninstall -y {name}"),
            Eco::Go => format!("go mod edit -droprequire={name} && go mod tidy"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Dep {
    pub eco: Eco,
    pub name: String,
    /// As the manifest asks: `^1.2.0`, `1.0`, `>=2`.
    pub wanted: String,
    /// What the lockfile or the install has.
    pub installed: Option<String>,
    pub dev: bool,
}

#[derive(Clone, Debug, Default)]
struct Latest {
    version: Option<String>,
    about: Option<String>,
    failed: bool,
}

#[derive(Clone, Debug)]
struct Found {
    eco: Eco,
    name: String,
    version: String,
    about: String,
}

/// Each package in a lockfile and what it depends on directly, as
/// (name, version). Several versions of one package are merged.
type Graph = HashMap<String, Vec<(String, String)>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Sev {
    Low,
    Moderate,
    High,
    Critical,
}

impl Sev {
    fn parse(s: &str) -> Option<Sev> {
        match s.to_ascii_lowercase().as_str() {
            "critical" => Some(Sev::Critical),
            "high" => Some(Sev::High),
            "moderate" | "medium" => Some(Sev::Moderate),
            "low" | "info" | "none" => Some(Sev::Low),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Sev::Critical => "critical",
            Sev::High => "high",
            Sev::Moderate => "moderate",
            Sev::Low => "low",
        }
    }

    fn color(self) -> u32 {
        match self {
            Sev::Critical | Sev::High => theme::palette().ansi[1],
            Sev::Moderate => theme::palette().ansi[3],
            Sev::Low => theme::dim(),
        }
    }
}

/// One known vulnerability in one package, from an audit tool.
#[derive(Clone, Debug, PartialEq)]
struct Advisory {
    name: String,
    sev: Sev,
    title: String,
}

pub enum PackagesEvent {
    /// Run this in a background job named so.
    Run { name: String, cmd: String, cwd: PathBuf },
    OpenUrl(String),
}

pub struct PackagesPage {
    /// Keys reach Deck's shortcuts while this page is in front.
    pub focus: gpui::FocusHandle,
    pub cwd: PathBuf,
    deps: Vec<Dep>,
    /// (eco, manager) for each manifest found: `pnpm`, `cargo`, `uv`.
    managers: HashMap<Eco, String>,
    latest: HashMap<(Eco, String), Latest>,
    filter: Filter,
    query: Entity<crate::field::Field>,
    results: Vec<Found>,
    /// "Search the registry's own site" rows, for registries without a
    /// search API: (registry, url).
    links: Vec<(Eco, String)>,
    searching: bool,
    last_query: String,
    /// The lockfile's dependency graph for each ecosystem that has one.
    graphs: HashMap<Eco, Graph>,
    /// Rows opened to their dependencies, with who pulls each in.
    expanded: HashMap<(Eco, String), Vec<String>>,
    /// The audit tools' findings, for the ecosystems that could be audited.
    advisories: HashMap<Eco, Vec<Advisory>>,
    /// Per row: the advisories on it or on anything under it, worst first.
    issues: HashMap<(Eco, String), Vec<Advisory>>,
    audit_gen: usize,
    /// The row whose Remove was clicked once and now asks to be confirmed.
    confirm: Option<(Eco, String)>,
    confirm_gen: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Outdated,
    Dev,
    Issues,
}

impl EventEmitter<PackagesEvent> for PackagesPage {}

impl PackagesPage {
    pub fn new(cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let root = crate::inspect::project_root(&cwd);
        let query = cx.new(|cx| {
            let mut f = crate::field::Field::new("Search npm, crates.io, PyPI and Go…", cx);
            // The search box draws the frame; the field none of its own.
            f.bare = true;
            f
        });
        cx.observe(&query, |p: &mut Self, _, cx| p.search_soon(cx)).detach();
        let mut p = Self {
            focus: cx.focus_handle(),
            cwd: root,
            deps: Vec::new(),
            managers: HashMap::new(),
            latest: HashMap::new(),
            filter: Filter::All,
            query,
            results: Vec::new(),
            links: Vec::new(),
            searching: false,
            last_query: String::new(),
            graphs: HashMap::new(),
            expanded: HashMap::new(),
            advisories: HashMap::new(),
            issues: HashMap::new(),
            audit_gen: 0,
            confirm: None,
            confirm_gen: 0,
        };
        p.reload(cx);
        p
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let (deps, managers, graphs) = read_project(&self.cwd);
        self.deps = deps;
        self.managers = managers;
        self.graphs = graphs;
        let open: Vec<(Eco, String)> = self.expanded.keys().cloned().collect();
        self.expanded.clear();
        for key in open {
            if self.deps.iter().any(|d| d.eco == key.0 && d.name == key.1) {
                let users = self.users_of(&key);
                self.expanded.insert(key, users);
            }
        }
        self.recompute_issues();
        self.fetch_latest(cx);
        self.audit(cx);
        cx.notify();
    }

    /// The installed audit tools, off the main thread; their findings mark
    /// the rows when they come back. A missing tool marks nothing.
    fn audit(&mut self, cx: &mut Context<Self>) {
        self.audit_gen += 1;
        let gen = self.audit_gen;
        let root = self.cwd.clone();
        let npm = self.manager(Eco::Npm) == "npm" && root.join("package-lock.json").exists();
        let cargo = self.managers.contains_key(&Eco::Cargo) && root.join("Cargo.lock").exists();
        if !npm && !cargo {
            self.advisories.clear();
            self.recompute_issues();
            return;
        }
        let task = cx.background_executor().spawn(async move { run_audits(&root, npm, cargo) });
        cx.spawn(async move |this, cx| {
            let found = task.await;
            let _ = this.update(cx, |p, cx| {
                if p.audit_gen == gen {
                    p.advisories = found;
                    p.recompute_issues();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Which advisories touch each row: its own, and those on anything it
    /// pulls in through the lockfile.
    fn recompute_issues(&mut self) {
        self.issues.clear();
        for d in &self.deps {
            let Some(found) = self.advisories.get(&d.eco).filter(|a| !a.is_empty()) else { continue };
            let under = self.graphs.get(&d.eco).map(|g| reachable(g, &d.name)).unwrap_or_default();
            let mut mine: Vec<Advisory> =
                found.iter().filter(|a| a.name == d.name || under.contains(&a.name)).cloned().collect();
            if mine.is_empty() {
                continue;
            }
            mine.sort_by(|a, b| b.sev.cmp(&a.sev).then(a.name.cmp(&b.name)));
            mine.dedup();
            self.issues.insert((d.eco, d.name.clone()), mine);
        }
    }

    /// The project's other dependencies that pull `key` in.
    fn users_of(&self, key: &(Eco, String)) -> Vec<String> {
        let Some(g) = self.graphs.get(&key.0) else { return Vec::new() };
        let roots: Vec<&str> = self.deps.iter().filter(|d| d.eco == key.0).map(|d| d.name.as_str()).collect();
        used_by(g, &roots, &key.1)
    }

    fn toggle(&mut self, key: (Eco, String), cx: &mut Context<Self>) {
        if self.expanded.remove(&key).is_none() {
            let users = self.users_of(&key);
            self.expanded.insert(key, users);
        }
        cx.notify();
    }

    /// The first click on Remove asks again for three seconds; the second
    /// within them removes.
    fn remove(&mut self, d: &Dep, cx: &mut Context<Self>) {
        let key = (d.eco, d.name.clone());
        if self.confirm.as_ref() == Some(&key) {
            self.confirm = None;
            let cmd = d.eco.remove_cmd(self.manager(d.eco), &d.name, d.dev);
            self.run(format!("remove {}", d.name), cmd, cx);
            cx.notify();
            return;
        }
        self.confirm = Some(key);
        self.confirm_gen += 1;
        let gen = self.confirm_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
            let _ = this.update(cx, |p, cx| {
                if p.confirm_gen == gen && p.confirm.take().is_some() {
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// The registry's newest for every dependency not yet asked about, a
    /// few at a time.
    fn fetch_latest(&mut self, cx: &mut Context<Self>) {
        let todo: Vec<(Eco, String)> = self
            .deps
            .iter()
            .filter(|d| !self.latest.contains_key(&(d.eco, d.name.clone())))
            .map(|d| (d.eco, d.name.clone()))
            .collect();
        for k in &todo {
            self.latest.insert(k.clone(), Latest::default());
        }
        for chunk in todo.chunks(8).map(|c| c.to_vec()) {
            cx.spawn(async move |this, cx| {
                let asks = chunk.iter().map(|(eco, name)| {
                    let url = eco.latest_url(name);
                    async move {
                        match url {
                            Some(url) => crate::net::get(&url, 12.).await.and_then(|b| serde_json::from_slice::<Value>(&b).ok()),
                            None => None,
                        }
                    }
                });
                let answers = futures::future::join_all(asks).await;
                let _ = this.update(cx, |p, cx| {
                    for ((eco, name), v) in chunk.into_iter().zip(answers) {
                        let found = v.as_ref().and_then(|v| eco.latest_of(v));
                        p.latest.insert(
                            (eco, name),
                            Latest {
                                failed: found.is_none(),
                                version: found.as_ref().map(|f| f.0.clone()),
                                about: found.and_then(|f| f.1),
                            },
                        );
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn search_soon(&mut self, cx: &mut Context<Self>) {
        let q = self.query.read(cx).text().trim().to_string();
        if q == self.last_query {
            return;
        }
        self.last_query = q.clone();
        if q.len() < 2 {
            self.results.clear();
            self.links.clear();
            cx.notify();
            return;
        }
        self.searching = true;
        // PyPI and Go have no search to ask, only exact names; they are
        // asked when the project uses them or the query reads like theirs.
        let py = self.managers.contains_key(&Eco::Python) || looks_python(&q);
        let go = self.managers.contains_key(&Eco::Go) || looks_go_module(&q);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            let still = this.update(cx, |p, _| p.last_query == q).unwrap_or(false);
            if !still {
                return;
            }
            let enc = crate::browser::encode_query(&q);
            let npm_url = format!("https://registry.npmjs.org/-/v1/search?text={enc}&size=8");
            let crates_url = format!("https://crates.io/api/v1/crates?q={enc}&per_page=8");
            let py_name = (py && pypi_name(&q)).then(|| q.clone());
            let go_path = (go && looks_go_module(&q)).then(|| q.clone());
            let pypi = async {
                match &py_name {
                    Some(n) => crate::net::get(&format!("https://pypi.org/pypi/{n}/json"), 10.).await,
                    None => None,
                }
            };
            let gomod = async {
                match &go_path {
                    Some(m) => crate::net::get(&format!("https://proxy.golang.org/{}/@latest", go_escape(m)), 10.).await,
                    None => None,
                }
            };
            let (npm, crates, pypi, gomod) = futures::join!(
                crate::net::get(&npm_url, 10.),
                crate::net::get(&crates_url, 10.),
                pypi,
                gomod
            );
            let mut found = Vec::new();
            if let Some(v) = npm.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                for o in v["objects"].as_array().into_iter().flatten() {
                    let pk = &o["package"];
                    if let (Some(n), Some(ver)) = (pk["name"].as_str(), pk["version"].as_str()) {
                        found.push(Found {
                            eco: Eco::Npm,
                            name: n.into(),
                            version: ver.into(),
                            about: pk["description"].as_str().unwrap_or("").into(),
                        });
                    }
                }
            }
            if let Some(v) = crates.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                for c in v["crates"].as_array().into_iter().flatten() {
                    if let (Some(n), Some(ver)) = (c["name"].as_str(), c["max_stable_version"].as_str().or(c["max_version"].as_str())) {
                        found.push(Found {
                            eco: Eco::Cargo,
                            name: n.into(),
                            version: ver.into(),
                            about: c["description"].as_str().unwrap_or("").trim().into(),
                        });
                    }
                }
            }
            if let Some(v) = pypi.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
                let info = &v["info"];
                if let (Some(n), Some(ver)) = (info["name"].as_str(), info["version"].as_str()) {
                    found.push(Found {
                        eco: Eco::Python,
                        name: n.into(),
                        version: ver.into(),
                        about: info["summary"].as_str().unwrap_or("").trim().into(),
                    });
                }
            }
            if let (Some(m), Some(v)) = (&go_path, gomod.and_then(|b| serde_json::from_slice::<Value>(&b).ok())) {
                if let Some(ver) = v["Version"].as_str() {
                    found.push(Found { eco: Eco::Go, name: m.clone(), version: ver.into(), about: String::new() });
                }
            }
            let mut links = Vec::new();
            if py {
                links.push((Eco::Python, format!("https://pypi.org/search/?q={enc}")));
            }
            if go {
                links.push((Eco::Go, format!("https://pkg.go.dev/search?q={enc}")));
            }
            let _ = this.update(cx, |p, cx| {
                if p.last_query == q {
                    // The project's own ecosystems first.
                    found.sort_by_key(|f| !p.managers.contains_key(&f.eco));
                    p.results = found;
                    p.links = links;
                    p.searching = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Point the page at another project folder.
    fn pick_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Project".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await {
                if let Some(dir) = paths.pop() {
                    let _ = this.update(cx, |p, cx| {
                        p.cwd = crate::inspect::project_root(&dir);
                        p.latest.clear();
                        p.expanded.clear();
                        p.reload(cx);
                    });
                }
            }
        })
        .detach();
    }

    fn manager(&self, eco: Eco) -> &str {
        self.managers.get(&eco).map_or("", String::as_str)
    }

    fn run(&self, job: String, cmd: String, cx: &mut Context<Self>) {
        cx.emit(PackagesEvent::Run { name: job, cmd, cwd: self.cwd.clone() });
    }

    fn outdated(&self, d: &Dep) -> bool {
        let Some(latest) = self.latest.get(&(d.eco, d.name.clone())).and_then(|l| l.version.as_deref()) else {
            return false;
        };
        let have = d.installed.as_deref().unwrap_or_else(|| d.wanted.trim_start_matches(['^', '~', '=', '>', '<', ' ']));
        newer(latest, have)
    }

    /// What a dependency row opens to: its own dependencies, who pulls it
    /// in, and the advisories on it.
    fn detail(&self, d: &Dep, users: &[String]) -> AnyElement {
        let chip = |name: String, version: Option<String>| {
            div()
                .flex()
                .items_center()
                .gap(u(4.))
                .h(u(22.))
                .px(u(7.))
                .rounded(u(6.))
                .bg(rgba(theme::tint()))
                .text_color(rgb(theme::text()))
                .child(name)
                .children(version.map(|v| div().text_color(rgb(theme::dim())).child(v)))
        };
        let line = |label: &'static str, body: AnyElement| {
            div()
                .flex()
                .items_start()
                .gap(u(10.))
                .child(div().w(u(84.)).flex_none().line_height(u(22.)).text_color(rgb(theme::dim())).child(label))
                .child(div().flex_1().min_w_0().child(body))
        };
        let note = |s: String| div().line_height(u(22.)).text_color(rgb(theme::text_2())).child(s).into_any_element();
        const MAX: usize = 60;
        let graph = self.graphs.get(&d.eco);
        let direct = graph.and_then(|g| g.get(&d.name));
        let depends = match (graph, direct) {
            (None, _) => note("No lockfile to read.".into()),
            (Some(_), None) => note("Not in the lockfile.".into()),
            (Some(_), Some(list)) if list.is_empty() => note("Nothing.".into()),
            (Some(_), Some(list)) => div()
                .flex()
                .flex_wrap()
                .gap(u(4.))
                .children(list.iter().take(MAX).map(|(n, v)| chip(n.clone(), (!v.is_empty()).then(|| v.clone()))))
                .when(list.len() > MAX, |x| x.child(note(format!("+{} more", list.len() - MAX))))
                .into_any_element(),
        };
        let used = if graph.is_none() {
            note("No lockfile to read.".into())
        } else if users.is_empty() {
            note("Only the project.".into())
        } else {
            div()
                .flex()
                .flex_wrap()
                .gap(u(4.))
                .children(users.iter().take(MAX).map(|n| chip(n.clone(), None)))
                .when(users.len() > MAX, |x| x.child(note(format!("+{} more", users.len() - MAX))))
                .into_any_element()
        };
        let issues = self.issues.get(&(d.eco, d.name.clone())).map(|list| {
            let body = div().flex().flex_col().children(list.iter().map(|a| {
                div()
                    .flex()
                    .items_center()
                    .gap(u(6.))
                    .min_h(u(22.))
                    .child(sev_badge(a.sev))
                    .when(a.name != d.name, |x| x.child(div().flex_none().text_color(rgb(theme::dim())).child(a.name.clone())))
                    .child(div().flex_1().min_w_0().one_line().text_color(rgb(theme::text_2())).child(a.title.clone()))
            }));
            line("Advisories", body.into_any_element())
        });
        div()
            .flex()
            .flex_col()
            .gap(u(6.))
            .pl(u(18.))
            .pb(u(10.))
            .text_size(u(12.))
            .child(line("Depends on", depends))
            .child(line("Used by", used))
            .children(issues)
            .into_any_element()
    }
}

impl Render for PackagesPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let outdated: Vec<&Dep> = self.deps.iter().filter(|d| self.outdated(d)).collect();
        let n_out = outdated.len();
        let seg = |id: &'static str, label: String, on: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .h(u(24.))
                .px(u(10.))
                .rounded(u(6.))
                .text_size(u(theme::TEXT_UI))
                .cursor_pointer()
                .when(on, |d| {
                    d.bg(rgb(theme::row_active())).border_1().border_color(rgba(theme::hairline())).text_color(rgb(theme::text()))
                })
                .when(!on, |d| d.text_color(rgb(theme::text_2())).hover(|s| s.text_color(rgb(theme::text()))))
                .child(label)
        };
        let audited = !self.advisories.is_empty();
        let filters = div()
            .flex()
            .gap(u(2.))
            .p(u(2.))
            .rounded(u(8.))
            .bg(rgba(theme::tint()))
            .child(seg("f-all", format!("All {}", self.deps.len()), self.filter == Filter::All).on_click(cx.listener(|p, _, _, cx| {
                p.filter = Filter::All;
                cx.notify();
            })))
            .child(seg("f-out", format!("Outdated {n_out}"), self.filter == Filter::Outdated).on_click(cx.listener(|p, _, _, cx| {
                p.filter = Filter::Outdated;
                cx.notify();
            })))
            .child(seg("f-dev", "Dev".into(), self.filter == Filter::Dev).on_click(cx.listener(|p, _, _, cx| {
                p.filter = Filter::Dev;
                cx.notify();
            })))
            .when(audited, |d| {
                d.child(seg("f-issues", format!("Issues {}", self.issues.len()), self.filter == Filter::Issues).on_click(
                    cx.listener(|p, _, _, cx| {
                        p.filter = Filter::Issues;
                        cx.notify();
                    }),
                ))
            });
        let update_all = (n_out > 0).then(|| {
            let cmds: Vec<String> = outdated.iter().map(|d| d.eco.add_cmd(self.manager(d.eco), &d.name, d.dev)).collect();
            small_button("update-all".into(), "Update all").on_click(cx.listener(move |p, _, _, cx| {
                p.run("update outdated".into(), cmds.join(" && "), cx);
            }))
        });
        let place = self
            .cwd
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| crate::chat::short_path(&self.cwd));
        // The project, as a control: a click picks another folder.
        let project = div()
            .id("pk-project")
            .flex()
            .items_center()
            .gap(u(6.))
            .h(u(28.))
            .px(u(8.))
            .rounded(u(6.))
            .cursor_pointer()
            .text_color(rgb(theme::text_2()))
            .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
            .child(icon(Icon::Folder, 13., theme::dim()))
            .child(place)
            .child(icon(Icon::ChevronDown, 10., theme::dim()))
            .on_click(cx.listener(|p, _, _, cx| p.pick_project(cx)));
        let header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(u(8.))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap(u(6.))
                    .text_size(u(15.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Packages"))
                    .child(div().text_color(rgb(theme::dim())).child("/"))
                    .child(project),
            )
            .children(update_all)
            .when(!self.deps.is_empty(), |d| d.child(filters))
            .child(
                div()
                    .id("pk-reload")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(u(28.))
                    .rounded(u(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(theme::row_hover())))
                    .child(icon(Icon::Refresh, 14., theme::text_2()))
                    .on_click(cx.listener(|p, _, _, cx| {
                        p.latest.clear();
                        p.reload(cx);
                    })),
            );

        // Search results, grouped by registry, the project's own first.
        let mut ecos: Vec<Eco> = Vec::new();
        for e in self.results.iter().map(|f| f.eco).chain(self.links.iter().map(|l| l.0)) {
            if !ecos.contains(&e) {
                ecos.push(e);
            }
        }
        ecos.sort_by_key(|e| !self.managers.contains_key(e));
        let mut found = div().flex().flex_col().gap(u(16.));
        for eco in ecos {
            let mut list = div().flex().flex_col().mx(u(-8.));
            for (i, f) in self.results.iter().enumerate().filter(|(_, f)| f.eco == eco) {
                let group: SharedString = format!("pk-row-{i}").into();
                let (job, add) = (format!("add {}", f.name), f.eco.add_cmd(self.manager(f.eco), &f.name, false));
                let have = self.deps.iter().any(|d| d.eco == f.eco && d.name.eq_ignore_ascii_case(&f.name));
                let page = f.eco.page(&f.name);
                list = list.child(
                    div()
                        .id(ElementId::NamedInteger("pk-found".into(), i as u64))
                        .group(group.clone())
                        .flex()
                        .items_center()
                        .gap(u(12.))
                        .min_h(u(48.))
                        .px(u(8.))
                        .py(u(6.))
                        .rounded(u(8.))
                        .hover(|s| s.bg(rgb(theme::row_hover())))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap(u(2.))
                                .child(
                                    div()
                                        .flex()
                                        .items_baseline()
                                        .gap(u(8.))
                                        .child(div().font_weight(FontWeight::MEDIUM).line_height(u(18.)).child(f.name.clone()))
                                        .child(div().text_size(u(theme::TEXT_SMALL)).text_color(rgb(theme::dim())).child(f.version.clone())),
                                )
                                .when(!f.about.is_empty(), |x| {
                                    x.child(
                                        div()
                                            .w_full()
                                            .one_line()
                                            .line_height(u(16.))
                                            .text_size(u(12.))
                                            .text_color(rgb(theme::text_2()))
                                            .child(f.about.clone()),
                                    )
                                }),
                        )
                        // Actions show on the row under the pointer; one in
                        // the project says so instead of offering to add.
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .gap(u(6.))
                                .when(!have, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                                .child(
                                    small_button(ElementId::NamedInteger("pk-docs".into(), i as u64), "Docs")
                                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PackagesEvent::OpenUrl(page.clone())))),
                                )
                                .child(if have {
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(u(4.))
                                        .h(u(28.))
                                        .px(u(8.))
                                        .text_size(u(12.))
                                        .text_color(rgb(theme::dim()))
                                        .child(icon(Icon::Check, 12., theme::palette().ansi[2]))
                                        .child("In project")
                                        .into_any_element()
                                } else {
                                    small_button(ElementId::NamedInteger("pk-add".into(), i as u64), "Add")
                                        .bg(rgb(theme::text()))
                                        .text_color(rgb(theme::pane()))
                                        .border_color(gpui::transparent_black())
                                        .on_click(cx.listener(move |p, _, _, cx| p.run(job.clone(), add.clone(), cx)))
                                        .into_any_element()
                                }),
                        ),
                );
            }
            if let Some((_, url)) = self.links.iter().find(|l| l.0 == eco) {
                let url = url.clone();
                let site = match eco {
                    Eco::Python => "Search on PyPI",
                    Eco::Go => "Search pkg.go.dev",
                    _ => "Search the registry",
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!("pk-link-{}", eco.label())))
                        .flex()
                        .items_center()
                        .gap(u(8.))
                        .min_h(u(40.))
                        .px(u(8.))
                        .rounded(u(8.))
                        .cursor_pointer()
                        .text_size(u(theme::TEXT_UI))
                        .text_color(rgb(theme::text_2()))
                        .hover(|s| s.bg(rgb(theme::row_hover())).text_color(rgb(theme::text())))
                        .child(icon(Icon::Search, 12., theme::dim()))
                        .child(format!("{site} for “{}”", self.last_query))
                        .child(icon(Icon::ArrowUpRight, 11., theme::dim()))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(PackagesEvent::OpenUrl(url.clone())))),
                );
            }
            let count = list_count(&self.results, eco);
            found = found.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(u(4.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(u(6.))
                            .h(u(24.))
                            .text_size(u(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(rgb(theme::dim()))
                            .child(eco.label())
                            .child(div().font_weight(FontWeight::NORMAL).child(format!("{count}"))),
                    )
                    .child(list),
            );
        }
        let search = div()
            .flex()
            .flex_col()
            .gap(u(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(u(8.))
                    .h(u(36.))
                    .px(u(12.))
                    .rounded(u(10.))
                    .bg(rgb(theme::raised()))
                    .border_1()
                    .border_color(rgba(theme::hairline()))
                    .child(icon(Icon::Search, 14., theme::dim()))
                    .child(div().flex_1().child(self.query.clone()))
                    .when(self.searching, |d| d.child(crate::icon::spinner("pk-search", 13., theme::dim()))),
            )
            .when(!self.results.is_empty() || !self.links.is_empty(), |d| d.child(found));
        let rows: Vec<&Dep> = self
            .deps
            .iter()
            .filter(|d| match self.filter {
                Filter::All => true,
                Filter::Outdated => self.outdated(d),
                Filter::Dev => d.dev,
                Filter::Issues => self.issues.contains_key(&(d.eco, d.name.clone())),
            })
            .collect();
        let mut table = div()
            .flex()
            .flex_col()
            .text_size(u(13.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .h(u(30.))
                    .text_size(u(theme::TEXT_SMALL))
                    .text_color(rgb(theme::dim()))
                    .border_b_1()
                    .border_color(rgba(theme::hairline()))
                    .child(div().flex_1().pl(u(18.)).child("Package"))
                    .child(div().w(u(110.)).child("Wanted"))
                    .child(div().w(u(110.)).child("Installed"))
                    .child(div().w(u(110.)).child("Latest"))
                    .child(div().w(u(210.))),
            );
        for (i, d) in rows.iter().enumerate() {
            let key = (d.eco, d.name.clone());
            let latest = self.latest.get(&key).cloned().unwrap_or_default();
            let behind = self.outdated(d);
            let page = d.eco.page(&d.name);
            let cmd = d.eco.add_cmd(self.manager(d.eco), &d.name, d.dev);
            let job = format!("update {}", d.name);
            let open = self.expanded.get(&key);
            let confirming = self.confirm.as_ref() == Some(&key);
            let worst = self.issues.get(&key).and_then(|l| l.first()).map(|a| a.sev);
            let group: SharedString = format!("dep-row-{i}").into();
            let dep = (*d).clone();
            let toggle_key = key.clone();
            let row = div()
                .id(ElementId::NamedInteger("dep-row".into(), i as u64))
                .group(group.clone())
                .flex()
                .items_center()
                .min_h(u(40.))
                .py(u(5.))
                .cursor_pointer()
                .on_click(cx.listener(move |p, _, _, cx| p.toggle(toggle_key.clone(), cx)))
                .child(
                    div()
                        .w(u(18.))
                        .flex_none()
                        .child(icon(if open.is_some() { Icon::ChevronDown } else { Icon::ChevronRight }, 10., theme::dim())),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(u(6.))
                                .child(div().font_weight(FontWeight::MEDIUM).one_line().child(d.name.clone()))
                                .when(d.dev, |x| x.child(div().text_size(u(theme::TEXT_META)).text_color(rgb(theme::dim())).child("dev")))
                                .child(div().text_size(u(theme::TEXT_META)).text_color(rgb(theme::dim())).child(d.eco.label()))
                                .children(worst.map(sev_badge)),
                        )
                        .children(latest.about.clone().map(|a| {
                            div().w_full().one_line().text_size(u(theme::TEXT_SMALL)).text_color(rgb(theme::text_2())).child(a)
                        })),
                )
                .child(div().w(u(110.)).text_color(rgb(theme::text_2())).one_line().child(d.wanted.clone()))
                .child(div().w(u(110.)).text_color(rgb(theme::text_2())).child(d.installed.clone().unwrap_or_else(|| "—".into())))
                .child(
                    div()
                        .w(u(110.))
                        .text_color(rgb(if behind { theme::palette().ansi[3] } else { theme::text_2() }))
                        .child(match (&latest.version, latest.failed) {
                            (Some(v), _) => v.clone(),
                            (None, true) => "—".into(),
                            (None, false) => "…".into(),
                        }),
                )
                .child(
                    div()
                        .w(u(210.))
                        .flex()
                        .justify_end()
                        .gap(u(6.))
                        .child(
                            small_button(ElementId::NamedInteger("dep-docs".into(), i as u64), "Docs")
                                .opacity(0.)
                                .group_hover(group.clone(), |s| s.opacity(1.))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.emit(PackagesEvent::OpenUrl(page.clone()))
                                })),
                        )
                        .when(behind, |x| {
                            x.child(small_button(ElementId::NamedInteger("dep-up".into(), i as u64), "Update").on_click(
                                cx.listener(move |p, _, _, cx| {
                                    cx.stop_propagation();
                                    p.run(job.clone(), cmd.clone(), cx)
                                }),
                            ))
                        })
                        .child(
                            small_button(
                                ElementId::NamedInteger("dep-rm".into(), i as u64),
                                if confirming { "Confirm" } else { "Remove" },
                            )
                            .when(confirming, |b| b.text_color(rgb(theme::palette().ansi[1])))
                            .when(!confirming, |b| b.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                            .on_click(cx.listener(move |p, _, _, cx| {
                                cx.stop_propagation();
                                p.remove(&dep, cx)
                            })),
                        ),
                );
            table = table.child(
                div()
                    .flex()
                    .flex_col()
                    .border_b_1()
                    .border_color(rgba(theme::hairline()))
                    .child(row)
                    .children(open.map(|users| self.detail(d, users))),
            );
        }
        if self.deps.is_empty() {
            table = table.child(
                div()
                    .py(u(24.))
                    .text_color(rgb(theme::dim()))
                    .child("No package.json, Cargo.toml, pyproject.toml, requirements.txt or go.mod at this project's root."),
            );
        }

        div()
            .id("packages-page")
            .track_focus(&self.focus)
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme::pane()))
            .text_color(rgb(theme::text()))
            .child(
                div()
                    .mx_auto()
                    .max_w(px(1100.))
                    .w(relative(1.))
                    .px(u(32.))
                    .py(u(24.))
                    .flex()
                    .flex_col()
                    .gap(u(24.))
                    .child(header)
                    .child(search)
                    .child(table),
            )
    }
}

fn list_count(results: &[Found], eco: Eco) -> usize {
    results.iter().filter(|f| f.eco == eco).count()
}

/// A small bordered button, 28 tall like every control on the page.
fn small_button(id: ElementId, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(u(28.))
        .px(u(10.))
        .rounded(u(6.))
        .border_1()
        .border_color(rgba(theme::hairline()))
        .text_size(u(12.))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::row_hover())))
        .child(label)
}

/// An advisory's severity as a small tinted label.
fn sev_badge(sev: Sev) -> gpui::Div {
    let c = sev.color();
    div()
        .flex()
        .flex_none()
        .items_center()
        .h(u(16.))
        .px(u(5.))
        .rounded(u(4.))
        .bg(rgba((c << 8) | 0x24))
        .text_size(u(theme::TEXT_META))
        .text_color(rgb(c))
        .child(sev.label())
}

/// `latest` is newer than `have`, comparing dotted numbers.
fn newer(latest: &str, have: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v').split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (nums(latest), nums(have));
    if b.is_empty()
        || b.iter().all(|x| *x == 0) && have.trim_start_matches('v').chars().any(|c| !c.is_ascii_digit() && c != '.')
    {
        return false;
    }
    a > b
}

/// A PyPI project name (PEP 508): letters, digits, `.`, `_`, `-`.
fn pypi_name(q: &str) -> bool {
    !q.is_empty()
        && q.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        && q.starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// A query that reads like a Python package's name.
fn looks_python(q: &str) -> bool {
    let l = q.to_ascii_lowercase();
    pypi_name(q)
        && (l.starts_with("py") || l.ends_with("py") || l.contains("python") || l.contains('_') || l.starts_with("django") || l.starts_with("flask"))
}

/// A Go module path: a host with a dot, then a path.
fn looks_go_module(q: &str) -> bool {
    let mut parts = q.split('/');
    let host = parts.next().unwrap_or("");
    host.contains('.')
        && parts.next().is_some_and(|p| !p.is_empty())
        && !q.contains(char::is_whitespace)
        && q.chars().all(|c| c.is_ascii_alphanumeric() || "./-_~".contains(c))
}

/// A module path as the Go proxy wants it: capitals as `!` and the lower.
fn go_escape(module: &str) -> String {
    let mut out = String::with_capacity(module.len());
    for c in module.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The dependencies the project's manifests list, which package manager
/// each ecosystem uses here, and the lockfiles' dependency graphs.
fn read_project(root: &Path) -> (Vec<Dep>, HashMap<Eco, String>, HashMap<Eco, Graph>) {
    let mut deps = Vec::new();
    let mut managers = HashMap::new();
    let mut graphs = HashMap::new();
    if let Some(v) = std::fs::read(root.join("package.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        let manager = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
            "bun"
        } else {
            "npm"
        };
        managers.insert(Eco::Npm, manager.to_string());
        for (key, dev) in [("dependencies", false), ("devDependencies", true)] {
            for (name, want) in v[key].as_object().into_iter().flatten() {
                let installed = std::fs::read(root.join("node_modules").join(name).join("package.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                    .and_then(|p| p["version"].as_str().map(str::to_string));
                deps.push(Dep {
                    eco: Eco::Npm,
                    name: name.clone(),
                    wanted: want.as_str().unwrap_or("").to_string(),
                    installed,
                    dev,
                });
            }
        }
        if let Some(lock) = std::fs::read(root.join("package-lock.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            graphs.insert(Eco::Npm, parse_npm_lock(&lock));
        } else if let Ok(text) = std::fs::read_to_string(root.join("pnpm-lock.yaml")) {
            if let Some(g) = parse_pnpm_lock(&text) {
                graphs.insert(Eco::Npm, g);
            }
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) {
        managers.insert(Eco::Cargo, "cargo".into());
        let (locked, graph) =
            std::fs::read_to_string(root.join("Cargo.lock")).map(|t| parse_cargo_lock(&t)).unwrap_or_default();
        if !graph.is_empty() {
            graphs.insert(Eco::Cargo, graph);
        }
        let mut section = String::new();
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with('[') {
                section = l.trim_matches(['[', ']']).to_string();
                continue;
            }
            let dev = section == "dev-dependencies" || section.ends_with(".dev-dependencies");
            let ok = section == "dependencies" || section == "workspace.dependencies" || dev || section == "build-dependencies";
            if !ok || l.is_empty() || l.starts_with('#') {
                continue;
            }
            let Some((name, rest)) = l.split_once('=') else { continue };
            let name = name.trim().to_string();
            let rest = rest.trim();
            if rest.contains("path =") || rest.contains("workspace = true") || rest.contains("git =") {
                continue;
            }
            let wanted = if rest.starts_with('"') {
                rest.trim_matches('"').to_string()
            } else {
                rest.split("version").nth(1).and_then(|r| r.split('"').nth(1)).unwrap_or("").to_string()
            };
            deps.push(Dep {
                eco: Eco::Cargo,
                installed: locked.get(&name).cloned(),
                name,
                wanted,
                dev,
            });
        }
    }
    let py_manager = if root.join("uv.lock").exists() {
        "uv"
    } else if root.join("poetry.lock").exists() {
        "poetry"
    } else {
        "pip"
    };
    if let Ok(text) = std::fs::read_to_string(root.join("pyproject.toml")) {
        managers.insert(Eco::Python, py_manager.into());
        let mut inside = false;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with("dependencies") && l.contains('[') {
                inside = true;
            }
            if inside {
                for part in l.split('"').skip(1).step_by(2) {
                    let (name, wanted) = split_requirement(part);
                    if !name.is_empty() {
                        deps.push(Dep { eco: Eco::Python, name, wanted, installed: None, dev: false });
                    }
                }
                if l.contains(']') {
                    inside = false;
                }
            }
        }
    } else if let Ok(text) = std::fs::read_to_string(root.join("requirements.txt")) {
        managers.insert(Eco::Python, py_manager.into());
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-')) {
            let (name, wanted) = split_requirement(l);
            deps.push(Dep { eco: Eco::Python, name, wanted, installed: None, dev: false });
        }
    }
    if let Ok(text) = std::fs::read_to_string(root.join("go.mod")) {
        managers.insert(Eco::Go, "go".into());
        let mut block = false;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with("require (") {
                block = true;
                continue;
            }
            if block && l == ")" {
                block = false;
                continue;
            }
            let body = if block { Some(l) } else { l.strip_prefix("require ") };
            if let Some(body) = body {
                let mut w = body.split_whitespace();
                if let (Some(name), Some(ver)) = (w.next(), w.next()) {
                    deps.push(Dep {
                        eco: Eco::Go,
                        name: name.into(),
                        wanted: ver.into(),
                        installed: Some(ver.into()),
                        dev: body.contains("// indirect"),
                    });
                }
            }
        }
    }
    deps.sort_by(|a, b| a.dev.cmp(&b.dev).then(a.name.cmp(&b.name)));
    (deps, managers, graphs)
}

fn split_requirement(s: &str) -> (String, String) {
    let at = s.find(|c: char| "<>=!~ ;[".contains(c)).unwrap_or(s.len());
    (s[..at].trim().to_string(), s[at..].trim().to_string())
}

/// A Cargo.lock's packages: each one's version (the first, when several)
/// and what each depends on.
fn parse_cargo_lock(text: &str) -> (HashMap<String, String>, Graph) {
    let mut versions: HashMap<String, String> = HashMap::new();
    // Each [[package]]'s name and its raw `dependencies` entries.
    let mut raw: Vec<(String, Vec<String>)> = Vec::new();
    let mut in_package = false;
    let mut in_deps = false;
    let entries = |s: &str| -> Vec<String> {
        s.split(',').map(|e| e.trim().trim_matches('"').to_string()).filter(|e| !e.is_empty()).collect()
    };
    for line in text.lines() {
        let l = line.trim();
        if in_deps {
            let (body, end) = match l.find(']') {
                Some(i) => (&l[..i], true),
                None => (l, false),
            };
            if let Some(last) = raw.last_mut() {
                last.1.extend(entries(body));
            }
            in_deps = !end;
            continue;
        }
        if l.starts_with('[') {
            in_package = l == "[[package]]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(n) = l.strip_prefix("name = ") {
            raw.push((n.trim_matches('"').to_string(), Vec::new()));
        } else if let Some(v) = l.strip_prefix("version = ") {
            if let Some((n, _)) = raw.last() {
                versions.entry(n.clone()).or_insert_with(|| v.trim_matches('"').to_string());
            }
        } else if let Some(rest) = l.strip_prefix("dependencies = [") {
            let (body, end) = match rest.find(']') {
                Some(i) => (&rest[..i], true),
                None => (rest, false),
            };
            if let Some(last) = raw.last_mut() {
                last.1.extend(entries(body));
            }
            in_deps = !end;
        }
    }
    let mut graph: Graph = HashMap::new();
    for (name, list) in raw {
        let out = graph.entry(name).or_default();
        for e in list {
            // `name`, `name version` or `name version (source)`.
            let mut w = e.split_whitespace();
            let Some(dep) = w.next() else { continue };
            let ver = w.next().map(str::to_string).or_else(|| versions.get(dep).cloned()).unwrap_or_default();
            if !out.iter().any(|(n, v)| n == dep && *v == ver) {
                out.push((dep.to_string(), ver));
            }
        }
    }
    (versions, graph)
}

/// A package-lock.json's graph: `packages` (lockfile v2 and v3) or the
/// nested `dependencies` with `requires` (v1).
fn parse_npm_lock(v: &Value) -> Graph {
    let mut graph: Graph = HashMap::new();
    if let Some(pk) = v["packages"].as_object() {
        // The hoisted copy speaks for the package; nested copies only
        // when there is none.
        let mut keys: Vec<&String> = pk.keys().collect();
        keys.sort_by_key(|k| k.matches("node_modules/").count());
        for path in keys {
            let Some((_, name)) = path.rsplit_once("node_modules/") else { continue };
            if graph.contains_key(name) {
                continue;
            }
            let p = &pk[path.as_str()];
            let mut out = Vec::new();
            for key in ["dependencies", "optionalDependencies"] {
                for (dep, range) in p[key].as_object().into_iter().flatten() {
                    let ver = pk
                        .get(&format!("{path}/node_modules/{dep}"))
                        .or_else(|| pk.get(&format!("node_modules/{dep}")))
                        .and_then(|d| d["version"].as_str())
                        .or(range.as_str())
                        .unwrap_or("");
                    out.push((dep.clone(), ver.to_string()));
                }
            }
            graph.insert(name.to_string(), out);
        }
    } else if let Some(top) = v["dependencies"].as_object() {
        fn walk(deps: &serde_json::Map<String, Value>, top: &serde_json::Map<String, Value>, graph: &mut Graph) {
            for (name, d) in deps {
                if !graph.contains_key(name) {
                    let out = d["requires"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(dep, range)| {
                            let ver = d["dependencies"][dep]["version"]
                                .as_str()
                                .or(top.get(dep).and_then(|t| t["version"].as_str()))
                                .or(range.as_str())
                                .unwrap_or("");
                            (dep.clone(), ver.to_string())
                        })
                        .collect();
                    graph.insert(name.clone(), out);
                }
                if let Some(nested) = d["dependencies"].as_object() {
                    walk(nested, top, graph);
                }
            }
        }
        walk(top, top, &mut graph);
    }
    graph
}

/// A pnpm-lock.yaml's graph, from its `packages:` (lockfile 6) or
/// `snapshots:` (lockfile 9) entries. `None` for the older layouts.
fn parse_pnpm_lock(text: &str) -> Option<Graph> {
    let unquote = |s: &str| s.trim().trim_matches(['\'', '"']).to_string();
    let mut graph: Graph = HashMap::new();
    let mut section = "";
    let mut current: Option<String> = None;
    let mut in_deps = false;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let l = line.trim();
        if indent == 0 {
            if let Some(v) = l.strip_prefix("lockfileVersion:") {
                let major = unquote(v).split('.').next().and_then(|m| m.parse::<u32>().ok()).unwrap_or(0);
                if major < 6 {
                    return None;
                }
            }
            section = if l == "packages:" || l == "snapshots:" { if l == "packages:" { "packages" } else { "snapshots" } } else { "" };
            current = None;
            in_deps = false;
            continue;
        }
        if section.is_empty() {
            continue;
        }
        if indent == 2 {
            in_deps = false;
            current = None;
            // `name@1.0.0:` or, with nothing under it, `name@1.0.0: {}`.
            let key = unquote(l.split(": ").next().unwrap_or(l).trim_end_matches(':'));
            let key = key.trim_start_matches('/');
            let key = key.split('(').next().unwrap_or(key);
            if let Some(at) = key.rfind('@').filter(|&i| i > 0) {
                let name = key[..at].to_string();
                graph.entry(name.clone()).or_default();
                current = Some(name);
            }
        } else if indent == 4 {
            in_deps = l == "dependencies:" || l == "optionalDependencies:";
        } else if indent == 6 && in_deps {
            let (Some(name), Some((dep, ver))) = (&current, l.split_once(": ")) else { continue };
            let dep = unquote(dep);
            let ver = unquote(ver);
            let ver = ver.split('(').next().unwrap_or("").to_string();
            let out = graph.entry(name.clone()).or_default();
            if !out.iter().any(|(n, v)| *n == dep && *v == ver) {
                out.push((dep, ver));
            }
        }
    }
    Some(graph)
}

/// Everything `from` pulls in, directly or not (not `from` itself).
fn reachable(graph: &Graph, from: &str) -> HashSet<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::from([from]);
    while let Some(n) = queue.pop_front() {
        for (dep, _) in graph.get(n).into_iter().flatten() {
            if dep != from && seen.insert(dep.clone()) {
                queue.push_back(dep);
            }
        }
    }
    seen
}

/// Which of `roots` (the project's own dependencies) pull `target` in.
fn used_by(graph: &Graph, roots: &[&str], target: &str) -> Vec<String> {
    let mut out: Vec<String> =
        roots.iter().filter(|r| **r != target && reachable(graph, r).contains(target)).map(|r| r.to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// A tool on the PATH, or where the usual installers put one; the app may
/// have started with a short PATH.
fn find_tool(name: &str) -> Option<PathBuf> {
    if let Some(p) = agent_core::exec::which(name) {
        return Some(p);
    }
    let home = agent_core::dirs::home_dir().unwrap_or_default();
    [home.join(".cargo").join("bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into()]
        .into_iter()
        .map(|d| d.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
        .find(|p| p.is_file())
}

/// Run `bin args` in `root` and read its JSON from stdout. The tool's own
/// folder goes first on PATH, so `npm` finds the `node` beside it.
fn json_of(bin: &Path, args: &[&str], root: &Path) -> Option<Value> {
    use std::process::{Command, Stdio};
    let mut dirs: Vec<PathBuf> = bin.parent().map(Path::to_path_buf).into_iter().collect();
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    let path = std::env::join_paths(dirs).unwrap_or_default();
    // `npm` is `npm.cmd` on Windows: through what it runs, as for agents.
    let (program, first) = agent_core::exec::launcher(&bin.to_string_lossy());
    let out = crate::os::no_window(&mut Command::new(program))
        .args(first)
        .args(args)
        .current_dir(root)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    // Both tools exit non-zero when they find something; the JSON says.
    serde_json::from_slice(&out.stdout).ok()
}

/// The audits that can run here. An ecosystem is in the answer only when
/// its tool ran and answered.
fn run_audits(root: &Path, npm: bool, cargo: bool) -> HashMap<Eco, Vec<Advisory>> {
    let mut out = HashMap::new();
    if npm {
        if let Some(found) = find_tool("npm").and_then(|b| json_of(&b, &["audit", "--json"], root)).and_then(|v| parse_npm_audit(&v)) {
            out.insert(Eco::Npm, found);
        }
    }
    if cargo {
        if let Some(found) =
            find_tool("cargo-audit").and_then(|b| json_of(&b, &["audit", "--json"], root)).and_then(|v| parse_cargo_audit(&v))
        {
            out.insert(Eco::Cargo, found);
        }
    }
    out
}

/// `npm audit --json`: `vulnerabilities` by package (npm 7 and later), or
/// `advisories` by id (npm 6).
fn parse_npm_audit(v: &Value) -> Option<Vec<Advisory>> {
    let mut out = Vec::new();
    if let Some(vulns) = v["vulnerabilities"].as_object() {
        for (name, x) in vulns {
            let Some(sev) = x["severity"].as_str().and_then(Sev::parse) else { continue };
            let via = x["via"].as_array();
            let title = via
                .into_iter()
                .flatten()
                .find_map(|w| w["title"].as_str().map(str::to_string))
                .or_else(|| {
                    let names: Vec<&str> = via.into_iter().flatten().filter_map(Value::as_str).collect();
                    (!names.is_empty()).then(|| format!("Through {}", names.join(", ")))
                })
                .unwrap_or_else(|| "Known vulnerability".into());
            out.push(Advisory { name: name.clone(), sev, title });
        }
        return Some(out);
    }
    if let Some(ads) = v["advisories"].as_object() {
        for a in ads.values() {
            if let (Some(name), Some(sev)) = (a["module_name"].as_str(), a["severity"].as_str().and_then(Sev::parse)) {
                out.push(Advisory { name: name.into(), sev, title: a["title"].as_str().unwrap_or("Known vulnerability").into() });
            }
        }
        return Some(out);
    }
    // An error report (no lockfile, offline) marks nothing.
    None
}

/// `cargo audit --json`: `vulnerabilities.list`, each with its package and
/// advisory; severity from the advisory's CVSS vector when it has one.
fn parse_cargo_audit(v: &Value) -> Option<Vec<Advisory>> {
    let list = v["vulnerabilities"]["list"].as_array()?;
    Some(
        list.iter()
            .filter_map(|x| {
                let a = &x["advisory"];
                let name = x["package"]["name"].as_str().or(a["package"].as_str())?;
                let sev = a["severity"]
                    .as_str()
                    .and_then(Sev::parse)
                    .or_else(|| a["cvss"].as_str().and_then(cvss3_sev))
                    .unwrap_or(Sev::Moderate);
                let title = a["title"].as_str().unwrap_or("Known vulnerability");
                let id = a["id"].as_str().map(|i| format!("{i}: ")).unwrap_or_default();
                Some(Advisory { name: name.into(), sev, title: format!("{id}{title}") })
            })
            .collect(),
    )
}

/// The severity a CVSS 3.x base vector scores to.
fn cvss3_sev(vector: &str) -> Option<Sev> {
    if !vector.starts_with("CVSS:3") {
        return None;
    }
    let m: HashMap<&str, &str> = vector.split('/').skip(1).filter_map(|p| p.split_once(':')).collect();
    let changed = *m.get("S")? == "C";
    let av = match *m.get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match *m.get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    let pr = match (*m.get("PR")?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let ui = match *m.get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let cia = |k: &str| match m.get(k).copied() {
        Some("H") => Some(0.56),
        Some("L") => Some(0.22),
        Some("N") => Some(0.),
        _ => None,
    };
    let (c, i, a): (f64, f64, f64) = (cia("C")?, cia("I")?, cia("A")?);
    let iss = 1. - (1. - c) * (1. - i) * (1. - a);
    let impact = if changed { 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15) } else { 6.42 * iss };
    let score = if impact <= 0. {
        0.
    } else {
        let e = 8.22 * av * ac * pr * ui;
        let s = if changed { 1.08 * (impact + e) } else { impact + e };
        (s.min(10.) * 10. - 1e-9).ceil() / 10.
    };
    Some(match score {
        s if s >= 9. => Sev::Critical,
        s if s >= 7. => Sev::High,
        s if s >= 4. => Sev::Moderate,
        _ => Sev::Low,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        assert!(newer("2.1.0", "2.0.9"));
        assert!(!newer("2.1.0", "2.1.0"));
        assert!(!newer("1.0.0", "2.0.0"));
        assert!(newer("1.10.0", "1.9.3"));
        assert!(newer("v1.10.0", "v1.9.3"));
        assert!(!newer("v1.10.0", "v0.0.0-20230101-abcdef"));
    }

    #[test]
    fn this_repo_reads() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (deps, m, graphs) = read_project(root);
        assert_eq!(m.get(&Eco::Cargo).map(String::as_str), Some("cargo"));
        assert!(deps.iter().any(|d| d.name == "image"));
        assert_eq!(split_requirement("requests>=2.31"), ("requests".into(), ">=2.31".into()));
        // The crate's Cargo.lock lives at the workspace root, not here.
        let _ = graphs;
        let lock = std::fs::read_to_string(root.join("../../Cargo.lock")).unwrap();
        let (versions, g) = parse_cargo_lock(&lock);
        assert!(versions.contains_key("serde_json"));
        assert!(g.get("app").is_some_and(|d| d.iter().any(|(n, _)| n == "serde_json")));
        assert!(used_by(&g, &["app", "serde_json"], "serde_json").contains(&"app".to_string()));
    }

    #[test]
    fn cargo_lock_graph() {
        let lock = r#"
version = 3

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "serde",
 "syn 2.0.1",
 "rand 0.8.5 (registry+https://github.com/rust-lang/crates.io-index)",
]

[[package]]
name = "serde"
version = "1.0.200"
dependencies = ["serde_derive"]

[[package]]
name = "serde_derive"
version = "1.0.200"
dependencies = [
 "syn 2.0.1",
]

[[package]]
name = "syn"
version = "2.0.1"

[[package]]
name = "syn"
version = "1.0.109"

[[package]]
name = "rand"
version = "0.8.5"

[metadata]
name = "not a package"
"#;
        let (versions, g) = parse_cargo_lock(lock);
        assert_eq!(versions.get("syn").map(String::as_str), Some("2.0.1"));
        assert_eq!(versions.get("serde").map(String::as_str), Some("1.0.200"));
        assert_eq!(
            g["app"],
            vec![
                ("serde".to_string(), "1.0.200".to_string()),
                ("syn".to_string(), "2.0.1".to_string()),
                ("rand".to_string(), "0.8.5".to_string()),
            ]
        );
        assert_eq!(g["serde"], vec![("serde_derive".to_string(), "1.0.200".to_string())]);
        assert!(g["syn"].is_empty());
        assert!(!g.contains_key("not a package"));
        // serde pulls syn in through serde_derive; rand pulls nothing.
        assert_eq!(used_by(&g, &["serde", "rand", "syn"], "syn"), vec!["serde".to_string()]);
        assert!(reachable(&g, "serde").contains("syn"));
    }

    #[test]
    fn npm_lock_graph() {
        let v: Value = serde_json::from_str(
            r#"{
              "lockfileVersion": 3,
              "packages": {
                "": { "dependencies": { "express": "^4.18.0" } },
                "node_modules/express": { "version": "4.18.2", "dependencies": { "debug": "2.6.9", "qs": "^6.11.0" } },
                "node_modules/express/node_modules/debug": { "version": "2.6.9", "dependencies": { "ms": "2.0.0" } },
                "node_modules/debug": { "version": "4.3.4", "dependencies": { "ms": "2.1.2" } },
                "node_modules/qs": { "version": "6.11.0" },
                "node_modules/@scope/pkg": { "version": "1.0.0", "optionalDependencies": { "qs": "*" } }
              }
            }"#,
        )
        .unwrap();
        let g = parse_npm_lock(&v);
        let mut express = g["express"].clone();
        express.sort();
        assert_eq!(express, vec![("debug".into(), "2.6.9".into()), ("qs".into(), "6.11.0".into())]);
        // The hoisted debug is the one shown.
        assert_eq!(g["debug"], vec![("ms".to_string(), "2.1.2".to_string())]);
        assert_eq!(g["@scope/pkg"], vec![("qs".to_string(), "6.11.0".to_string())]);
        assert!(!g.contains_key(""));

        let v1: Value = serde_json::from_str(
            r#"{ "lockfileVersion": 1, "dependencies": {
                "a": { "version": "1.0.0", "requires": { "b": "^2.0.0" } },
                "b": { "version": "2.1.0" } } }"#,
        )
        .unwrap();
        let g1 = parse_npm_lock(&v1);
        assert_eq!(g1["a"], vec![("b".to_string(), "2.1.0".to_string())]);
    }

    #[test]
    fn pnpm_lock_graph() {
        let v9 = "lockfileVersion: '9.0'\n\nimporters:\n\n  .:\n    dependencies:\n      react:\n        specifier: ^18\n        version: 18.2.0\n\npackages:\n\n  react@18.2.0:\n    resolution: {integrity: sha512-x}\n    engines: {node: '>=0.10.0'}\n\nsnapshots:\n\n  react@18.2.0:\n    dependencies:\n      loose-envify: 1.4.0\n\n  '@babel/core@7.0.0(supports-color@5.5.0)':\n    dependencies:\n      '@babel/types': 7.0.0\n      debug: 4.3.4(supports-color@5.5.0)\n    optionalDependencies:\n      fsevents: 2.3.3\n\n  loose-envify@1.4.0: {}\n";
        let g = parse_pnpm_lock(v9).unwrap();
        assert_eq!(g["react"], vec![("loose-envify".to_string(), "1.4.0".to_string())]);
        assert_eq!(
            g["@babel/core"],
            vec![
                ("@babel/types".to_string(), "7.0.0".to_string()),
                ("debug".to_string(), "4.3.4".to_string()),
                ("fsevents".to_string(), "2.3.3".to_string()),
            ]
        );
        assert!(g["loose-envify"].is_empty());
        let v6 = "lockfileVersion: '6.0'\n\npackages:\n\n  /react@18.2.0:\n    resolution: {integrity: sha512-x}\n    dependencies:\n      loose-envify: 1.4.0\n    dev: false\n";
        assert_eq!(parse_pnpm_lock(v6).unwrap()["react"], vec![("loose-envify".to_string(), "1.4.0".to_string())]);
        assert!(parse_pnpm_lock("lockfileVersion: 5.4\n").is_none());
    }

    #[test]
    fn npm_audit_reads() {
        let v: Value = serde_json::from_str(
            r#"{ "auditReportVersion": 2, "vulnerabilities": {
                "lodash": { "name": "lodash", "severity": "critical", "isDirect": false,
                    "via": [ { "source": 1, "title": "Prototype Pollution", "severity": "critical" } ] },
                "express": { "name": "express", "severity": "high", "isDirect": true, "via": ["qs", "body-parser"] }
            }, "metadata": {} }"#,
        )
        .unwrap();
        let mut a = parse_npm_audit(&v).unwrap();
        a.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(a[0], Advisory { name: "express".into(), sev: Sev::High, title: "Through qs, body-parser".into() });
        assert_eq!(a[1], Advisory { name: "lodash".into(), sev: Sev::Critical, title: "Prototype Pollution".into() });

        let v6: Value = serde_json::from_str(
            r#"{ "advisories": { "1": { "module_name": "minimist", "severity": "moderate", "title": "Prototype Pollution" } } }"#,
        )
        .unwrap();
        assert_eq!(parse_npm_audit(&v6).unwrap()[0].sev, Sev::Moderate);
        let err: Value = serde_json::from_str(r#"{ "error": { "code": "ENOLOCK" } }"#).unwrap();
        assert!(parse_npm_audit(&err).is_none());
    }

    #[test]
    fn cargo_audit_reads() {
        let v: Value = serde_json::from_str(
            r#"{ "database": {}, "vulnerabilities": { "found": true, "count": 2, "list": [
                { "advisory": { "id": "RUSTSEC-2023-0001", "package": "tokio", "title": "Data race",
                    "cvss": "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H" },
                  "package": { "name": "tokio", "version": "1.0.0" } },
                { "advisory": { "id": "RUSTSEC-2023-0002", "package": "time", "title": "Segfault", "cvss": null },
                  "package": { "name": "time", "version": "0.1.0" } }
            ] }, "warnings": {} }"#,
        )
        .unwrap();
        let a = parse_cargo_audit(&v).unwrap();
        assert_eq!(a[0], Advisory { name: "tokio".into(), sev: Sev::Critical, title: "RUSTSEC-2023-0001: Data race".into() });
        assert_eq!(a[1].sev, Sev::Moderate);
        let clean: Value = serde_json::from_str(r#"{ "vulnerabilities": { "found": false, "count": 0, "list": [] } }"#).unwrap();
        assert!(parse_cargo_audit(&clean).unwrap().is_empty());
    }

    #[test]
    fn cvss_scores() {
        assert_eq!(cvss3_sev("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"), Some(Sev::Critical)); // 9.8
        assert_eq!(cvss3_sev("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H"), Some(Sev::High)); // 7.5
        assert_eq!(cvss3_sev("CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:N/I:N/A:H"), Some(Sev::Moderate)); // 5.9
        assert_eq!(cvss3_sev("CVSS:3.1/AV:L/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N"), Some(Sev::Low)); // 1.6
        assert_eq!(cvss3_sev("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N"), Some(Sev::Low)); // 0.0
        assert_eq!(cvss3_sev("CVSS:4.0/AV:N"), None);
    }

    #[test]
    fn queries_and_commands() {
        assert!(looks_go_module("github.com/gin-gonic/gin"));
        assert!(!looks_go_module("react"));
        assert!(!looks_go_module("github.com"));
        assert!(looks_python("python-dateutil") && looks_python("numpy") && !looks_python("react"));
        assert_eq!(go_escape("github.com/BurntSushi/toml"), "github.com/!burnt!sushi/toml");
        assert_eq!(Eco::Npm.remove_cmd("pnpm", "x", false), "pnpm remove x");
        assert_eq!(Eco::Cargo.remove_cmd("cargo", "x", true), "cargo remove --dev x");
        assert_eq!(Eco::Python.remove_cmd("pip", "x", false), "pip uninstall -y x");
        assert_eq!(Eco::Go.remove_cmd("go", "a/b", false), "go mod edit -droprequire=a/b && go mod tidy");
    }
}
