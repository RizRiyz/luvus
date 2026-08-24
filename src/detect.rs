//! Agent detection (M3, docs/07). Two questions, two very different answers.
//!
//! **Which agent is this?** The pane's running processes (docs/07 §2.1), passed
//! in as `running`. An agent is a program, so this is ground truth; a pane that
//! merely *prints* "claude" is a shell. Where processes can't be seen (Windows,
//! a remote pane) it falls back to text, ranked by how deliberate that text is —
//! spawn command, then OSC title, then output — with names that double as
//! ordinary words (`amp`, `cursor`, `droid`, `grok`, `pi`) believed only from
//! the first two. Identity is configurable: see `[identity]` in the manifests.
//!
//! **What state is it in?** Inferred from what's on screen via a small
//! **manifest** engine: a set of rules, each tied to a screen region (the OSC
//! title or the recent bottom text), a priority, and one or more conditions
//! (substrings / a spinner glyph). The highest-priority matching rule wins. Built-in rules cover the
//! markers common to modern agent CLIs plus a few per-agent quirks; **users can
//! add or override rules** by dropping `*.toml` files in `~/.luvus/manifests/`
//! (loaded at startup, merged by priority) so detection can be fixed or extended
//! for any agent without recompiling.
//!
//! A recognised agent is *working* only when a rule proves it (a spinner, an
//! interrupt hint) — raw output never counts, so a launching CLI's welcome
//! screen or a scrolling log can't fake the state. Plain shells (no markers to
//! match) fall back to output activity, gated by whether the user typed
//! recently, so keystroke echo at a prompt is never misread as work. luvus's
//! status debounce (docs/07, `QUIET_DWELL`) supplies stability: a momentary
//! non-match can't flip a working agent to idle.

use std::path::Path;

use serde::Deserialize;

use crate::ui::theme::State;

/// One agent luvus can recognise, and how far each identifying string may be
/// trusted. Splitting the two matters because the haystack for `screen` is
/// *whatever the pane happens to be printing*, which nobody controls.
struct KnownAgent {
    /// Canonical name: what the UI shows and `agent.list` returns.
    name: &'static str,
    /// Strings distinctive enough to believe anywhere, pane output included.
    distinct: &'static [&'static str],
    /// Strings that are also ordinary words. Believed only where a human or the
    /// agent itself put them deliberately — the spawned command and the OSC
    /// title — never from incidental output. Without this, `error: cursor is out
    /// of bounds` names the pane cursor, and "for example" named it amp.
    ambiguous: &'static [&'static str],
}

/// Agents we recognise. `distinct`/`ambiguous` are matched as whole words (see
/// [`contains_agent_word`]); the canonical `name` is what gets reported.
const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent {
        name: "claude",
        distinct: &["claude"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "codex",
        distinct: &["codex"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "gemini",
        distinct: &["gemini"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "aider",
        distinct: &["aider"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "opencode",
        distinct: &["opencode"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "copilot",
        distinct: &["copilot"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "kimi",
        distinct: &["kimi"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "qwen",
        distinct: &["qwen"],
        ambiguous: &[],
    },
    KnownAgent {
        name: "kiro",
        distinct: &["kiro"],
        ambiguous: &[],
    },
    // The CLI binary is distinctive even though the brand name is not.
    KnownAgent {
        name: "cursor",
        distinct: &["cursor-agent"],
        ambiguous: &["cursor"],
    },
    KnownAgent {
        name: "amp",
        distinct: &[],
        ambiguous: &["amp"],
    },
    KnownAgent {
        name: "droid",
        distinct: &[],
        ambiguous: &["droid"],
    },
    KnownAgent {
        name: "grok",
        distinct: &[],
        ambiguous: &["grok"],
    },
    // Pi (pi.dev): the npm binary is distinctive, but the bare brand name is an
    // ordinary word (and a substring of "api"/"pip"…), so believe it only from
    // the spawn command or OSC title — never incidental output.
    KnownAgent {
        name: "pi",
        distinct: &["pi-coding-agent"],
        ambiguous: &["pi"],
    },
    // `fx` is a short, common token in filenames and prose, so never infer it
    // from arbitrary screen output. Its process name and OSC title are
    // deliberate identity signals and therefore safe.
    KnownAgent {
        name: "fx",
        distinct: &[],
        ambiguous: &["fx"],
    },
];

/// The runtime form of [`KnownAgent`]: owned, so `~/.luvus/manifests/*.toml` can
/// refine a built-in agent or teach luvus one it has never heard of.
#[derive(Clone)]
struct AgentIdent {
    name: String,
    distinct: Vec<String>,
    ambiguous: Vec<String>,
}

impl AgentIdent {
    /// Every identifying string, deliberate placement assumed.
    fn all(&self) -> impl Iterator<Item = &String> {
        self.distinct.iter().chain(self.ambiguous.iter())
    }
}

/// The built-in registry, in runtime form.
fn builtin_agents() -> Vec<AgentIdent> {
    KNOWN_AGENTS
        .iter()
        .map(|a| AgentIdent {
            name: a.name.to_string(),
            distinct: a.distinct.iter().map(|s| s.to_string()).collect(),
            ambiguous: a.ambiguous.iter().map(|s| s.to_string()).collect(),
        })
        .collect()
}

// ── markers (matched case-insensitively) ─────────────────────────────────────

/// Confirmation / permission prompts that mean the agent is waiting on the user.
const BLOCKED_PROMPTS: &[&str] = &[
    "do you want to proceed",
    "do you want to continue",
    "waiting for approval",
    "waiting for user confirmation",
    "waiting for confirmation",
    "run this command?",
    "allow command?",
    "allow this command",
    "allow editing file",
    "allow creating file",
    "allow execution",
    "apply this change",
    "confirm tool call",
    "invoke tool",
    "write to this file?",
    "proceed (y)",
    "run (once) (y)",
    "skip (esc or n)",
    "esc or n or p",
    "reject & propose changes",
    "press enter to confirm",
    "enter to submit answer",
    "yes, allow",
    "no, cancel",
    "allow all for this session",
    "allow all for every session",
    "deny with feedback",
    "keep (n)",
    "(y) (enter)",
    "yes (y)",
    "(y/n)",
    "[y/n]",
    "yes/no",
    "❯ 1.",
    "1. yes",
    "press enter to continue",
];

/// OSC-title strings that flag a confirmation (e.g. codex, amp).
const BLOCKED_TITLES: &[&str] = &["action required", "confirmation needed"];

/// Interrupt hints an agent shows only while generating.
const WORKING_HINTS: &[&str] = &[
    "esc to interrupt",
    "esc to cancel",
    "esc to stop",
    "esc interrupt",
    "esc again to cancel",
    "ctrl+c to stop",
    "ctrl+c to interrupt",
    "ctrl-c to interrupt",
    "interrupt to stop",
];

// ── engine ───────────────────────────────────────────────────────────────────

/// The part of the screen a rule looks at.
#[derive(Clone, Copy, PartialEq)]
enum Region {
    /// The OSC window title the agent sets.
    Title,
    /// The recent bottom text of the pane.
    Screen,
}

/// One condition on a region's (lowercased) text. All strings are stored
/// lowercase.
enum Cond {
    /// The region contains at least one of these substrings.
    Any(Vec<String>),
    /// The region contains all of these substrings.
    All(Vec<String>),
    /// The region contains none of these substrings.
    Not(Vec<String>),
    /// A line in the region starts with a spinner glyph — a braille cell
    /// (U+2800..=U+28FF, what most CLIs animate) or a moon phase (U+1F311..=
    /// U+1F318, Kimi's background-agent spinner). A running spinner means work.
    Spinner,
}

impl Cond {
    fn holds(&self, low: &str) -> bool {
        match self {
            Cond::Any(subs) => subs.iter().any(|s| low.contains(s)),
            Cond::All(subs) => subs.iter().all(|s| low.contains(s)),
            Cond::Not(subs) => !subs.iter().any(|s| low.contains(s)),
            Cond::Spinner => low
                .lines()
                .any(|l| l.trim_start().chars().next().is_some_and(is_spinner_glyph)),
        }
    }
}

/// A spinner glyph: a *non-blank* braille cell (the common animated spinner), a
/// moon phase (Kimi animates 🌑..🌘 for background agents), or a half-circle
/// (◐◑◒◓) — the busy spinner newer Claude Code builds animate instead of braille.
///
/// The braille range deliberately starts at `U+2801`, **excluding `U+2800`**
/// (Braille Pattern Blank). That codepoint is an invisible cell many TUIs use for
/// padding/indentation, and it is *not* Unicode whitespace, so `trim_start`
/// leaves it at the start of a line — which used to make an idle agent whose UI
/// pads a line with it read as a running spinner (stuck "working"). A real
/// animated spinner only ever shows non-blank frames, so this loses nothing. The
/// half-circle range is safe here because Claude's *done* line begins with a
/// different glyph (a star ✻), not a half-circle.
fn is_spinner_glyph(c: char) -> bool {
    ('\u{2801}'..='\u{28FF}').contains(&c)
        || ('\u{1F311}'..='\u{1F318}').contains(&c)
        || ('\u{25D0}'..='\u{25D3}').contains(&c)
}

/// A detection rule: `state` is chosen when every `cond` holds in `region`.
/// `agent` empty means it applies to every agent.
struct Rule {
    agent: String,
    state: State,
    priority: i32,
    region: Region,
    conds: Vec<Cond>,
}

/// The active detection set: which agents exist and how to recognise them
/// (`agents`), plus the state rules (`rules`). Both come from the built-ins
/// merged with `~/.luvus/manifests/*.toml`.
pub struct Manifests {
    rules: Vec<Rule>,
    agents: Vec<AgentIdent>,
}

impl Manifests {
    /// Just the compiled-in defaults (test helper; production uses `load`).
    #[cfg(test)]
    pub fn builtin() -> Manifests {
        Manifests {
            rules: builtin_rules(),
            agents: builtin_agents(),
        }
    }

    /// Built-in defaults plus every valid `*.toml` in `dir`. Malformed files are
    /// skipped (logged), never fatal, so a bad manifest can't break detection.
    pub fn load(dir: &Path) -> Manifests {
        let mut rules = builtin_rules();
        let mut agents = builtin_agents();
        // Official OTA manifests (`luvus server update-manifest`) live in a
        // `managed/` subdir, kept apart from the user's own `*.toml` so an update
        // never clobbers a hand-written rule. Load managed first, then the user's,
        // so equal-priority user rules can still override.
        let managed = load_dir(&dir.join("managed"));
        for mf in managed.into_iter().chain(load_dir(dir)) {
            mf.apply_identity(&mut agents);
            rules.extend(mf.into_rules());
        }
        Manifests { rules, agents }
    }

    /// Total detection rules in effect (built-in + managed + user), for the
    /// `manifest.reload` reply so a caller can confirm an update took.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Stable, bounded discovery data for UHP administration.
    pub fn agent_names(&self) -> Vec<String> {
        self.agents.iter().map(|agent| agent.name.clone()).collect()
    }

    /// True if `name` is a recognised agent (not a plain shell). Drives whether
    /// a pane appears in the AGENTS list.
    pub fn is_agent(&self, name: &str) -> bool {
        let low = name.to_lowercase();
        self.agents.iter().any(|a| a.name == low)
    }

    fn evaluate(&self, agent: &str, regions: &Regions) -> Option<RuleMatch> {
        let mut best: Option<RuleMatch> = None;
        for r in &self.rules {
            if !(r.agent.is_empty() || r.agent == agent) {
                continue;
            }
            let text = regions.get(r.region);
            if r.conds.iter().all(|c| c.holds(text))
                && best
                    .as_ref()
                    .is_none_or(|matched| r.priority >= matched.priority)
            {
                best = Some(RuleMatch {
                    state: r.state,
                    priority: r.priority,
                    region: r.region,
                });
            }
        }
        best
    }
}

struct RuleMatch {
    state: State,
    priority: i32,
    region: Region,
}

fn any(subs: &[&str]) -> Cond {
    Cond::Any(subs.iter().map(|s| s.to_lowercase()).collect())
}
fn all(subs: &[&str]) -> Cond {
    Cond::All(subs.iter().map(|s| s.to_lowercase()).collect())
}

/// How much of the live terminal grid must reach the rule engine. Most agents
/// keep state beside their bottom prompt, but fx can pin its transient activity
/// above a tall blank footer area. Expand only a confirmed fx pane so every
/// other pane retains the existing extraction cost and false-positive surface.
pub(crate) fn screen_rows(known_agent: &str, running: &[String], manifests: &Manifests) -> u16 {
    if known_agent.eq_ignore_ascii_case("fx") || manifests.process_has_agent(running, "fx") {
        40
    } else {
        14
    }
}

/// The compiled-in default rules (generic first, then per-agent).
fn builtin_rules() -> Vec<Rule> {
    let gen = |state, priority, region, conds| Rule {
        agent: String::new(),
        state,
        priority,
        region,
        conds,
    };
    let per = |agent: &str, state, priority, region, conds| Rule {
        agent: agent.to_string(),
        state,
        priority,
        region,
        conds,
    };
    vec![
        // Generic: title confirmation, selection menus, permission prompts.
        gen(
            State::Blocked,
            330,
            Region::Title,
            vec![any(BLOCKED_TITLES)],
        ),
        gen(
            State::Blocked,
            320,
            Region::Screen,
            vec![all(&["enter to select", "esc to cancel"])],
        ),
        gen(
            State::Blocked,
            320,
            Region::Screen,
            vec![all(&["enter to confirm", "esc to cancel"])],
        ),
        gen(
            State::Blocked,
            320,
            Region::Screen,
            vec![all(&["enter select", "esc cancel"])],
        ),
        gen(
            State::Blocked,
            300,
            Region::Screen,
            vec![any(BLOCKED_PROMPTS)],
        ),
        // Generic: spinner, then bare interrupt hint.
        gen(State::Working, 120, Region::Title, vec![Cond::Spinner]),
        gen(State::Working, 110, Region::Screen, vec![Cond::Spinner]),
        gen(
            State::Working,
            100,
            Region::Screen,
            vec![any(WORKING_HINTS)],
        ),
        // Per-agent quirks.
        per(
            "gemini",
            State::Blocked,
            310,
            Region::Screen,
            vec![any(&["│ apply this change", "│ allow execution"])],
        ),
        per(
            "droid",
            State::Blocked,
            310,
            Region::Screen,
            vec![any(&["> yes, allow", "> no, cancel"])],
        ),
        per(
            "cursor",
            State::Working,
            105,
            Region::Screen,
            vec![any(&["ctrl+c to stop"])],
        ),
        // fx suppresses its activity row while it needs user input. Its
        // narrowest approval and question hints retain these paired controls,
        // so they remain reliable even in a small pane.
        per(
            "fx",
            State::Blocked,
            325,
            Region::Screen,
            vec![all(&["enter confirm", "esc cancel"])],
        ),
        per(
            "fx",
            State::Blocked,
            325,
            Region::Screen,
            vec![all(&["enter answer", "esc cancel"])],
        ),
        per(
            "fx",
            State::Blocked,
            325,
            Region::Screen,
            vec![any(&["permission needed ·", "needs permission"])],
        ),
        // Native fx activity rows. Match stable UI copy rather than raw PTY
        // traffic: fx probes terminal capabilities while idle, so recent bytes
        // alone are not evidence that the model is working.
        per(
            "fx",
            State::Working,
            125,
            Region::Screen,
            vec![any(&["• thinking", "retrying request in"])],
        ),
        per(
            "fx",
            State::Working,
            120,
            Region::Screen,
            vec![any(&[
                "listing |",
                "reading |",
                "writing |",
                "editing |",
                "running |",
                "delegating |",
                "opening |",
            ])],
        ),
        per(
            "fx",
            State::Working,
            120,
            Region::Screen,
            vec![all(&["• (", "↑", "↓"])],
        ),
        // Streamed response rows intentionally have no bullet or verb. The
        // live token counter is active only while the idle input rail is absent;
        // requiring both prevents a completed turn's retained counter from
        // pinning fx to Working at its prompt.
        per(
            "fx",
            State::Working,
            115,
            Region::Screen,
            vec![all(&["(↑", "↓"]), Cond::Not(vec!["┃".to_string()])],
        ),
        // Newer Claude Code dropped the "esc to interrupt" hint and animates a
        // non-braille spinner (✢/✻/✳), so the generic working rules miss it and
        // the pane read as idle while generating. Its live line is
        // "✢ Inferring… (12s · ↓ 3.2k tokens)", the streaming token counter of
        // its **built-in** spinner (not the user-customizable statusLine). Match
        // `↓ ` AND `tokens)` together, so a custom statusLine that happens to
        // contain one of them (e.g. `↓3` git state) can't false-trigger working.
        // The done line ("✻ Cogitated for 18s") has neither, so it stays idle,
        // and the glyph is deliberately *not* added to `is_spinner_glyph`: the
        // same ✻ appears in the done state and would misread it as working.
        per(
            "claude",
            State::Working,
            115,
            Region::Screen,
            vec![all(&["↓ ", "tokens)"])],
        ),
        // Kimi's approval panel (permission prompt) shows the footer
        // "↑/↓ select · N choose · ↵ confirm" while it waits — a very specific
        // three-word combination, so it can't be mistaken for the spinner that
        // was on screen a moment earlier.
        per(
            "kimi",
            State::Blocked,
            310,
            Region::Screen,
            vec![all(&["select", "choose", "confirm"])],
        ),
        // Grok Build's permission card (docs/35) always shows the two persistent
        // option labels "Always allow" + "Never allow" together, for every tool
        // (command, edit, MCP call). That pair is unmistakable and catches an
        // "Allow Edit?" the generic prompt list would miss.
        per(
            "grok",
            State::Blocked,
            315,
            Region::Screen,
            vec![all(&["always allow", "never allow"])],
        ),
        // Its bash-command card carries a distinct reject affordance.
        per(
            "grok",
            State::Blocked,
            315,
            Region::Screen,
            vec![any(&["no, reject (type to add feedback)"])],
        ),
        // Grok's busy footer is `Ctrl+c:cancel` (its own punctuation, not the
        // generic "ctrl+c to interrupt"), so give it a per-agent working hint.
        per(
            "grok",
            State::Working,
            105,
            Region::Screen,
            vec![any(&["ctrl+c:cancel"])],
        ),
    ]
}

// ── user manifests (TOML) ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ManifestFile {
    /// Which agent these rules apply to; `"generic"` (default) means all.
    #[serde(default = "default_generic")]
    agent: String,
    #[serde(default)]
    rule: Vec<RuleSpec>,
    /// How to *recognise* this agent (docs/07). Absent means "leave identity
    /// alone and only add state rules", which is what most manifests want.
    #[serde(default)]
    identity: Option<IdentitySpec>,
}

/// Identity patterns for one agent. Matched as whole words; see
/// [`contains_agent_word`] for why the two lists are not interchangeable.
#[derive(Deserialize)]
struct IdentitySpec {
    /// Distinctive enough to trust anywhere, pane output included.
    #[serde(default)]
    distinct: Vec<String>,
    /// Also an ordinary word, so trusted only in the spawned command or the
    /// agent's own OSC title.
    #[serde(default)]
    ambiguous: Vec<String>,
    /// Replace the built-in patterns instead of adding to them. Needed to
    /// *remove* a default, e.g. dropping `cursor` so only `cursor-agent` counts.
    #[serde(default)]
    replace: bool,
}

#[derive(Deserialize)]
struct RuleSpec {
    /// `working` | `blocked` | `idle`.
    state: String,
    #[serde(default)]
    priority: i32,
    /// `screen` (default) | `title`.
    #[serde(default = "default_screen")]
    region: String,
    /// The region must contain at least one of these.
    #[serde(default)]
    any: Vec<String>,
    /// The region must contain all of these.
    #[serde(default)]
    all: Vec<String>,
    /// The region must contain none of these.
    #[serde(default)]
    not: Vec<String>,
    /// The region must show a running spinner glyph.
    #[serde(default)]
    spinner: bool,
}

fn default_generic() -> String {
    "generic".to_string()
}
fn default_screen() -> String {
    "screen".to_string()
}

/// Whether `s` parses as a detection manifest, so `server update-manifest` can
/// reject a garbled download before writing it into the managed dir.
pub(crate) fn manifest_parses(s: &str) -> bool {
    toml::from_str::<ManifestFile>(s).is_ok()
}

fn load_dir(dir: &Path) -> Vec<ManifestFile> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    // Sorted so a merge is deterministic across machines (read_dir order is not).
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<ManifestFile>(&s).ok());
        match parsed {
            Some(mf) => out.push(mf),
            None => eprintln!(
                "luvus: skipping invalid detection manifest {}",
                path.display()
            ),
        }
    }
    out
}

impl ManifestFile {
    /// Merge this file's `[identity]` into the live registry. A manifest may
    /// refine a built-in agent or introduce one luvus has never heard of, so
    /// detection can follow a new CLI without waiting for a release.
    fn apply_identity(&self, agents: &mut Vec<AgentIdent>) {
        let Some(id) = &self.identity else { return };
        if self.agent.eq_ignore_ascii_case("generic") {
            return; // identity is meaningless without an agent to attach it to
        }
        let name = self.agent.to_lowercase();
        let lc = |v: &Vec<String>| v.iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
        let entry = match agents.iter_mut().find(|a| a.name == name) {
            Some(e) => e,
            None => {
                // Start empty: seeding the name into `distinct` here would
                // silently outrank a manifest that deliberately declared that
                // same name `ambiguous`. The name-as-default is applied below,
                // and only when the manifest supplied no patterns at all.
                agents.push(AgentIdent {
                    name: name.clone(),
                    distinct: Vec::new(),
                    ambiguous: Vec::new(),
                });
                agents.last_mut().expect("just pushed")
            }
        };
        if id.replace {
            entry.distinct.clear();
            entry.ambiguous.clear();
        }
        entry.distinct.extend(lc(&id.distinct));
        entry.ambiguous.extend(lc(&id.ambiguous));
        entry.distinct.dedup();
        entry.ambiguous.dedup();
        // An agent with no pattern at all could never be matched, so fall back
        // to its own name rather than registering something inert.
        if entry.distinct.is_empty() && entry.ambiguous.is_empty() {
            entry.distinct.push(name);
        }
    }

    fn into_rules(self) -> Vec<Rule> {
        let agent = if self.agent.eq_ignore_ascii_case("generic") {
            String::new()
        } else {
            self.agent.to_lowercase()
        };
        self.rule
            .into_iter()
            .filter_map(|spec| spec.into_rule(&agent))
            .collect()
    }
}

impl RuleSpec {
    fn into_rule(self, agent: &str) -> Option<Rule> {
        let state = match self.state.to_lowercase().as_str() {
            "working" => State::Working,
            "blocked" => State::Blocked,
            "idle" => State::Idle,
            "done" => State::Done,
            _ => return None, // unknown state → skip this rule
        };
        let region = match self.region.to_lowercase().as_str() {
            "title" => Region::Title,
            "screen" => Region::Screen,
            _ => return None,
        };
        let lc = |v: Vec<String>| v.into_iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
        let mut conds = Vec::new();
        if !self.any.is_empty() {
            conds.push(Cond::Any(lc(self.any)));
        }
        if !self.all.is_empty() {
            conds.push(Cond::All(lc(self.all)));
        }
        if !self.not.is_empty() {
            conds.push(Cond::Not(lc(self.not)));
        }
        if self.spinner {
            conds.push(Cond::Spinner);
        }
        if conds.is_empty() {
            return None; // a rule with no condition would match everything → skip
        }
        Some(Rule {
            agent: agent.to_string(),
            state,
            priority: self.priority,
            region,
            conds,
        })
    }
}

// ── public API ──────────────────────────────────────────────────────────────

/// Result of classifying a pane.
pub struct Detection {
    pub state: State,
    pub agent: String,
    /// Where identity came from. Stable, machine-readable values are exposed by
    /// `agent.explain`; no consumer needs to reverse-engineer detection order.
    pub identity_source: &'static str,
    /// Why the state was chosen. A manifest match includes its region/priority;
    /// otherwise this names the deliberately conservative fallback.
    pub state_source: &'static str,
    pub rule_priority: Option<i32>,
    pub rule_region: Option<&'static str>,
}

/// The recent-screen and title regions, lowercased once for matching.
struct Regions {
    screen: String,
    title: String,
}

impl Regions {
    fn get(&self, r: Region) -> &str {
        match r {
            Region::Title => &self.title,
            Region::Screen => &self.screen,
        }
    }
}

/// Classify a pane from its title, bottom-buffer text, whether it produced
/// output recently, whether the user typed into it recently, and the active
/// rule set. `base_command` is the spawned program, used as a fallback label.
#[allow(clippy::too_many_arguments)]
pub fn classify(
    title: Option<&str>,
    bottom: &str,
    recent_activity: bool,
    recent_input: bool,
    base_command: &str,
    known_agent: &str,
    // `running`: command lines actually live in the pane (docs/07). Ground
    // truth when available; `&[]` means "could not tell" (Windows, remote, `ps`
    // failed), never "nothing is running".
    running: &[String],
    manifests: &Manifests,
) -> Detection {
    let regions = Regions {
        screen: bottom.to_lowercase(),
        title: title.map(|t| t.to_lowercase()).unwrap_or_default(),
    };
    // Identity has two regimes, and which one applies is decided by whether we
    // could see the pane's processes at all.
    //
    // `running` non-empty means the scan worked, so it is the *whole* answer: an
    // agent is a program, and a pane that is not running one is a plain shell no
    // matter what it prints. Letting text vote here is what put "amp" and "kiro"
    // in the sidebar for panes running neither.
    //
    // `running` empty means we could not tell (Windows, a remote pane, `ps`
    // failed, or the pane has no child yet). Only then do the text heuristics
    // run — including `known_agent` stickiness, which covers the frames where an
    // agent's UI does not print its own name (Claude Code's bottom rows are a
    // prompt box and a model label, so a repaint would otherwise resolve to the
    // bare shell and read its own redraw as work).
    let (agent, identity_source) = if running.is_empty() {
        manifests
            .detect_agent(title, &regions.screen, base_command)
            .or_else(|| {
                manifests
                    .is_agent(known_agent)
                    .then(|| (known_agent.to_string(), "prior_identity"))
            })
            .unwrap_or_else(|| (base_command.to_string(), "command_fallback"))
    } else {
        manifests
            .agent_in_processes(running)
            .map(|agent| (agent, "process_tree"))
            .unwrap_or_else(|| (base_command.to_string(), "command_fallback"))
    };

    // A recognised agent is *working* only on positive evidence — a spinner or
    // an on-screen generating hint matched by a rule. Its output alone proves
    // nothing: a launching CLI prints a whole welcome screen while completely
    // idle, so no rule match means Idle. Plain shells have no markers to match,
    // so they keep the activity fallback (which powers `wait` on ordinary
    // commands), gated so typing echo isn't misread as output.
    let fallback = if !manifests.is_agent(&agent) && recent_activity && !recent_input {
        State::Working
    } else {
        State::Idle
    };
    let matched = manifests.evaluate(&agent, &regions);
    let (state, state_source, rule_priority, rule_region) = match matched {
        Some(rule) => (
            rule.state,
            "manifest_rule",
            Some(rule.priority),
            Some(match rule.region {
                Region::Title => "title",
                Region::Screen => "screen",
            }),
        ),
        None if fallback == State::Working => (fallback, "shell_activity", None, None),
        None => (fallback, "no_positive_state_evidence", None, None),
    };

    Detection {
        state,
        agent,
        identity_source,
        state_source,
        rule_priority,
        rule_region,
    }
}

/// Name the agent running in a pane, in decreasing order of how deliberate the
/// evidence is: the command the pane was spawned with, then the OSC title the
/// agent sets for itself, then — for distinctive names only — its output.
impl Manifests {
    /// Name the agent from the pane's **running processes** -- the only signal
    /// that is ground truth rather than inference. An agent is a program, not a
    /// word on a screen, so this outranks every text heuristic: a pane merely
    /// *printing* "claude" is not running claude.
    ///
    /// Both pattern classes apply, because a command line is as deliberate as
    /// it gets. Matching is per-argv-token against the bare binary name, so
    /// `/opt/homebrew/bin/amp` counts while `--example` never can.
    pub(crate) fn agent_in_processes(&self, running: &[String]) -> Option<String> {
        running
            .iter()
            .find_map(|cmd| self.agent_in_process_command(cmd))
    }

    /// Whether one specific agent is still present anywhere in the pane's
    /// process tree. Lifecycle detection needs the targeted form: an agent may
    /// itself launch another recognised agent as a child, and that must not make
    /// its own process look absent.
    pub(crate) fn process_has_agent(&self, running: &[String], agent: &str) -> bool {
        running
            .iter()
            .any(|cmd| self.agent_in_process_command(cmd).as_deref() == Some(agent))
    }

    fn agent_in_process_command(&self, cmd: &str) -> Option<String> {
        let low = cmd.to_lowercase();
        let mut tokens = low.split_whitespace();
        let first = binary_name(tokens.next()?);
        if let Some(a) = self.match_binary(first) {
            return Some(a);
        }
        // Several agents ship as a script run by an interpreter, so argv[0]
        // is `node` / `python` and the real name is the script slot: the first
        // non-flag argument. Nothing later counts -- `cargo test --example amp`
        // must not resolve to amp.
        if is_interpreter(first) {
            for t in tokens {
                if t.starts_with('-') {
                    continue;
                }
                return self.match_binary(binary_name(t));
            }
        }
        None
    }

    /// The launch flags an agent pane was started with: the argv tokens **after**
    /// the agent's own token in its command line, in original case (docs/62).
    /// Mirrors `agent_in_processes` -- the same per-token binary match and the
    /// same interpreter unwrap (`node .../cli.js --flag`) -- but returns the tail
    /// for the requested agent instead of a name. `None` when no running command
    /// line belongs to that agent, so a pane luvus cannot pin to it captures
    /// nothing rather than guessing.
    pub fn launch_args_for(&self, running: &[String], agent: &str) -> Option<Vec<String>> {
        for cmd in running {
            let tokens: Vec<&str> = cmd.split_whitespace().collect();
            let Some((first, rest)) = tokens.split_first() else {
                continue;
            };
            let first_low = first.to_lowercase();
            if self.match_binary(binary_name(&first_low)).as_deref() == Some(agent) {
                return Some(rest.iter().map(|s| s.to_string()).collect());
            }
            // Interpreter form: the agent token is the first non-flag argument,
            // and nothing before it is a launch flag.
            if is_interpreter(binary_name(&first_low)) {
                for (i, t) in rest.iter().enumerate() {
                    if t.starts_with('-') {
                        continue;
                    }
                    let t_low = t.to_lowercase();
                    if self.match_binary(binary_name(&t_low)).as_deref() == Some(agent) {
                        return Some(rest[(i + 1)..].iter().map(|s| s.to_string()).collect());
                    }
                    break; // the first non-flag arg is the script slot; nothing later counts
                }
            }
        }
        None
    }

    /// The agent whose patterns name exactly this binary.
    fn match_binary(&self, base: &str) -> Option<String> {
        self.agents
            .iter()
            .find(|a| a.all().any(|p| p == base))
            .map(|a| a.name.clone())
    }

    /// Name the agent running in a pane, in decreasing order of how deliberate
    /// the evidence is: the command the pane was spawned with, then the OSC
    /// title the agent sets for itself, then -- for distinctive names only --
    /// its output.
    fn detect_agent(
        &self,
        title: Option<&str>,
        low_bottom: &str,
        base_command: &str,
    ) -> Option<(String, &'static str)> {
        let cmd = base_command.to_lowercase();
        let title = title.map(|t| t.to_lowercase()).unwrap_or_default();

        // Deliberate signals: somebody typed this, or the agent published it.
        for (region, source) in [(&cmd, "launch_command"), (&title, "osc_title")] {
            if let Some(a) = self
                .agents
                .iter()
                .find(|a| a.all().any(|p| contains_agent_word(region, p)))
            {
                return Some((a.name.clone(), source));
            }
        }
        // Incidental signal: pane output. Only names that can't be ordinary words.
        self.agents
            .iter()
            .find(|a| {
                a.distinct
                    .iter()
                    .any(|p| contains_agent_word(low_bottom, p))
            })
            .map(|a| (a.name.clone(), "screen_text"))
    }
}

/// The bare binary name of an argv token: no directory, no `.exe`, and no
/// leading `-` (login shells appear as `-zsh`).
pub(crate) fn binary_name(token: &str) -> &str {
    token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_start_matches('-')
        .trim_end_matches(".exe")
}

/// Runtimes that execute an agent as a script, so the name to look for is the
/// argument rather than argv[0].
pub(crate) fn is_interpreter(base: &str) -> bool {
    matches!(
        base,
        "node"
            | "nodejs"
            | "bun"
            | "deno"
            | "npx"
            | "pnpm"
            | "yarn"
            | "python"
            | "python3"
            | "py"
            | "uv"
            | "uvx"
            | "ruby"
            | "perl"
            | "env"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
    )
}

/// True when `needle` appears in `hay` as a **standalone word**.
///
/// A plain substring test is not good enough here: the haystack is whatever the
/// pane happens to be showing, and short agent names hide inside ordinary words
/// — `amp` alone is a substring of *example*, *sample*, *stamped*, *champion*
/// and *implementation*, so a Claude pane printing "for example" renamed itself
/// to the amp agent. Path separators are excluded from the boundary set too, or
/// listing `~/.kiro` names the pane kiro. `-` and `_` still count as boundaries
/// so `claude-code` and `claude_code` keep matching.
fn contains_agent_word(hay: &str, needle: &str) -> bool {
    let boundary = |c: Option<char>| match c {
        None => true,
        Some(c) => !c.is_alphanumeric() && !matches!(c, '.' | '/' | '\\'),
    };
    hay.match_indices(needle).any(|(i, _)| {
        boundary(hay[..i].chars().next_back()) && boundary(hay[i + needle.len()..].chars().next())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(bottom: &str, activity: bool, input: bool) -> State {
        classify(
            Some("claude"),
            bottom,
            activity,
            input,
            "claude",
            "claude",
            &[],
            &Manifests::builtin(),
        )
        .state
    }

    fn fx_state(bottom: &str, activity: bool, input: bool) -> State {
        classify(
            Some("fx · zai/glm-5.2"),
            bottom,
            activity,
            input,
            "zsh",
            "fx",
            &["/opt/homebrew/bin/fx".to_string()],
            &Manifests::builtin(),
        )
        .state
    }

    #[test]
    fn fx_identity_uses_process_or_title_not_incidental_output() {
        let m = Manifests::builtin();
        assert_eq!(screen_rows("", &["fx".to_string()], &m), 40);
        assert_eq!(screen_rows("claude", &["claude".to_string()], &m), 14);
        let running = classify(
            Some("zsh"),
            "generated file: effects/fx.rs",
            true,
            false,
            "zsh",
            "",
            &["/opt/homebrew/bin/fx".to_string()],
            &m,
        );
        assert_eq!(running.agent, "fx", "the running binary names the agent");

        let titled = classify(
            Some("fx · zai/glm-5.2"),
            "",
            false,
            false,
            "zsh",
            "",
            &[],
            &m,
        );
        assert_eq!(titled.agent, "fx", "fx's OSC title is deliberate evidence");

        let incidental = classify(
            Some("zsh"),
            "generated file: effects/fx.rs",
            true,
            false,
            "zsh",
            "",
            &[],
            &m,
        );
        assert_eq!(incidental.agent, "zsh", "screen prose must not name fx");
    }

    #[test]
    fn fx_native_activity_rows_report_working() {
        assert_eq!(fx_state("• Thinking (5s)", false, false), State::Working);
        assert_eq!(
            fx_state(
                "Provider unavailable · HTTP 503 · retrying request in 4s · attempt 4/10",
                false,
                false,
            ),
            State::Working
        );
        assert_eq!(
            fx_state("running | 1 command started", false, false),
            State::Working
        );
        assert_eq!(
            fx_state("• (12s) (↑169 ↓5.1k)", false, false),
            State::Working
        );
        assert_eq!(
            fx_state("streamed response\n  (↑1.5k ↓20)", true, false),
            State::Working
        );
    }

    #[test]
    fn fx_idle_prompt_ignores_terminal_probe_activity() {
        assert_eq!(
            fx_state("𝒇x\n┃\nauto · zai/glm-5.2", true, false),
            State::Idle,
            "PTY capability traffic alone must not make an idle fx pane Working"
        );
        assert_eq!(
            fx_state(
                "last response (↑10 ↓20)\n┃\nauto · zai/glm-5.2",
                true,
                false
            ),
            State::Idle,
            "a retained token counter above the input rail is still idle"
        );
    }

    #[test]
    fn fx_native_interactions_report_blocked() {
        assert_eq!(
            fx_state(
                "• Thinking (4s)\nPermission needed · Choose one\nEnter Confirm    Esc Cancel",
                true,
                false,
            ),
            State::Blocked,
            "approval must outrank a lingering activity row"
        );
        assert_eq!(
            fx_state(
                "Should we proceed?\n1) Yes\n2) No\nEnter Answer · Esc Cancel",
                false,
                false,
            ),
            State::Blocked,
            "question prompts wait for the user"
        );
        assert_eq!(
            fx_state(
                "Subagent worker needs permission\nEnter Confirm    Esc Cancel",
                false,
                false,
            ),
            State::Blocked
        );
    }

    #[test]
    fn launch_args_extracted_after_the_agent_token() {
        let m = Manifests::builtin();
        // Direct binary at an absolute path; original case is preserved.
        assert_eq!(
            m.launch_args_for(
                &[
                    "/opt/homebrew/bin/claude --model Opus --permission-mode bypassPermissions"
                        .into()
                ],
                "claude",
            ),
            Some(vec![
                "--model".into(),
                "Opus".into(),
                "--permission-mode".into(),
                "bypassPermissions".into(),
            ])
        );
        // Interpreter form: the tail starts after the script slot that names the
        // agent, and the interpreter's own args before it are ignored.
        assert_eq!(
            m.launch_args_for(
                &["python /usr/local/bin/aider --model gpt-4o --yes".into()],
                "aider"
            ),
            Some(vec!["--model".into(), "gpt-4o".into(), "--yes".into()])
        );
        // A plain shell, and a different agent, both yield nothing for claude.
        assert!(m.launch_args_for(&["zsh -l".into()], "claude").is_none());
        assert!(m
            .launch_args_for(&["codex --model o".into()], "claude")
            .is_none());
        // An agent launched with no flags yields an empty tail (Some, not None).
        assert_eq!(
            m.launch_args_for(&["claude".into()], "claude"),
            Some(vec![])
        );
    }

    #[test]
    fn newer_claude_token_counter_reads_working() {
        // Recent Claude Code shows "✢ Inferring… (12s · ↓ 3.2k tokens)" with no
        // "esc to interrupt" and a non-braille spinner, so only the streaming
        // token counter proves it is working.
        assert_eq!(
            state("✢ Inferring… (12s · ↓ 3.2k tokens)", false, false),
            State::Working
        );
        // The done line is past-tense with no counter and must stay idle, even
        // though it starts with the same ✻ glyph.
        assert_eq!(state("✻ Cogitated for 18s", false, false), State::Idle);
        assert_eq!(state("✻ Worked for 47s", false, false), State::Idle);
    }

    #[test]
    fn half_circle_busy_spinner_reads_working() {
        // Some Claude Code builds animate a half-circle busy spinner (◐◑◒◓)
        // instead of braille; a line beginning with one means work.
        assert_eq!(state("◐ Thinking…", false, false), State::Working);
        assert_eq!(state("◓ Doing the thing", false, false), State::Working);
        // A star-led done line is still idle — stars are not spinner glyphs here.
        assert_eq!(state("✻ Cogitated for 18s", false, false), State::Idle);
    }

    #[test]
    fn managed_ota_manifests_load_from_the_managed_subdir() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("luvus-managed-mf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let managed = dir.join("managed");
        fs::create_dir_all(&managed).unwrap();
        // An OTA-style manifest that teaches claude a custom working marker.
        fs::write(
            managed.join("claude.toml"),
            "agent = \"claude\"\n\
             [[rule]]\n\
             state = \"working\"\n\
             priority = 250\n\
             region = \"screen\"\n\
             all = [\"zzz-marker\"]\n",
        )
        .unwrap();
        let m = Manifests::load(&dir);
        let d = classify(
            Some("claude"),
            "zzz-marker showing",
            false,
            false,
            "claude",
            "claude",
            &[],
            &m,
        );
        assert_eq!(
            d.state,
            State::Working,
            "a managed/ manifest rule took effect"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_parses_accepts_valid_and_rejects_garbage() {
        assert!(manifest_parses(
            "agent = \"claude\"\n[[rule]]\nstate = \"working\"\nregion = \"screen\"\nall = [\"x\"]\n"
        ));
        assert!(!manifest_parses("this is not toml ["));
    }

    #[test]
    fn permission_prompt_is_blocked() {
        assert_eq!(
            state("Do you want to proceed? (y/n)", true, false),
            State::Blocked
        );
        assert_eq!(
            state("Run this command? [y/n]", false, false),
            State::Blocked
        );
    }

    #[test]
    fn selection_menu_is_blocked_not_working() {
        let d = state(
            "Choose:\n1. Yes\n  Enter to select · Esc to cancel",
            true,
            false,
        );
        assert_eq!(d, State::Blocked);
    }

    #[test]
    fn spinner_is_working_even_while_typing() {
        assert_eq!(
            state("⠹ Thinking… (esc to interrupt)", true, true),
            State::Working
        );
    }

    // Regression: a line that merely *starts* with the Braille Pattern Blank
    // (U+2800) — an invisible padding cell, not whitespace, that many TUIs use to
    // indent — must not read as a running spinner. It used to pin an idle agent to
    // "working". A real (non-blank) braille frame still counts.
    #[test]
    fn braille_blank_padding_is_not_a_spinner() {
        assert_eq!(
            state("\u{2800}\u{2800}> ready for your next task", true, false),
            State::Idle,
            "a braille-blank-padded idle line is Idle, not Working"
        );
        assert_eq!(
            state("\u{280B} working on it", true, false),
            State::Working,
            "a real (non-blank) braille spinner frame still reads Working"
        );
    }

    #[test]
    fn interrupt_hint_is_working() {
        assert_eq!(
            state("· 1.2k tokens · esc to interrupt", true, false),
            State::Working
        );
    }

    #[test]
    fn typing_at_prompt_is_idle() {
        assert_eq!(state("> write me a function", true, true), State::Idle);
    }

    #[test]
    fn agent_output_without_a_marker_is_idle() {
        // A recognised agent is working only on positive evidence. Bare output
        // (no spinner, no interrupt hint) proves nothing — most visibly the
        // welcome screen a CLI prints on launch.
        assert_eq!(state("streaming plain text", true, false), State::Idle);
        assert_eq!(
            state(
                "✻ Welcome to Claude Code!\n\n  /help for help\n\n> ",
                true,
                false
            ),
            State::Idle,
            "a launching agent is idle, not working"
        );
    }

    #[test]
    fn shell_output_is_working_fallback() {
        // Plain shells keep the activity fallback (drives `wait` on commands).
        let d = classify(
            None,
            "compiling foo v0.1.0",
            true,
            false,
            "zsh",
            "",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Working);
    }

    #[test]
    fn kimi_moon_spinner_is_working() {
        // Kimi animates a moon phase for background agents; a line starting with
        // one reads as working just like a braille spinner.
        let d = classify(
            Some("kimi"),
            "🌖 running task…",
            true,
            false,
            "kimi",
            "kimi",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Working);
    }

    #[test]
    fn kimi_approval_panel_is_blocked() {
        // The approval footer outranks a lingering spinner reading.
        let d = classify(
            Some("kimi"),
            "Run this command?\n rm -rf build\n ↑/↓ select · 1/2 choose · ↵ confirm",
            true,
            false,
            "kimi",
            "kimi",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Blocked);
    }

    #[test]
    fn grok_permission_card_is_blocked() {
        // Grok's permission card shows Always/Never allow together; that pair
        // reads as Blocked even for an edit the generic prompt list wouldn't catch.
        let d = classify(
            Some("grok"),
            "Allow Edit?\n  src/main.rs\n\n  Allow    Always allow: this file    Never allow:",
            true,
            false,
            "grok",
            "grok",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Blocked);

        // The reject affordance on a bash-command card is also Blocked.
        let d = classify(
            Some("grok"),
            "Allow command?\n  cargo test\n\n  Yes    No, reject (type to add feedback)",
            true,
            false,
            "grok",
            "grok",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Blocked);
    }

    #[test]
    fn grok_busy_footer_is_working() {
        // Grok's own interrupt hint uses `Ctrl+c:cancel` punctuation.
        let d = classify(
            Some("grok"),
            "Editing files…\n  Ctrl+c:cancel",
            true,
            false,
            "grok",
            "grok",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Working);
    }

    // An agent's UI does not print its own name on every frame. Switching tabs or
    // clicking into the pane makes it repaint, and that repaint must not
    // re-classify a known agent as a plain shell.
    #[test]
    fn known_agent_stays_idle_when_its_name_is_off_screen() {
        let d = classify(
            None,                    // no OSC title
            "> \n  ? for shortcuts", // repaint: no agent name, no markers
            true,                    // the repaint produced output
            false,                   // and the user did not type
            "zsh",                   // launched through a shell
            "claude",                // but the pane is KNOWN to be claude
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(
            d.state,
            State::Idle,
            "a repaint with no marker must not read as working"
        );
    }

    #[test]
    fn quiet_is_idle() {
        let d = classify(
            None,
            "$ ",
            false,
            false,
            "zsh",
            "",
            &[],
            &Manifests::builtin(),
        );
        assert_eq!(d.state, State::Idle);
        assert_eq!(d.agent, "zsh");
    }

    #[test]
    fn user_manifest_rule_overrides_by_priority() {
        // A user rule with a higher priority flips detection for its agent.
        let toml = r#"
            agent = "claude"
            [[rule]]
            state = "blocked"
            priority = 500
            region = "screen"
            any = ["my custom prompt"]
        "#;
        let mut m = Manifests::builtin();
        m.rules
            .extend(toml::from_str::<ManifestFile>(toml).unwrap().into_rules());
        let d = classify(
            Some("claude"),
            "here is my custom prompt >",
            true,
            false,
            "claude",
            "claude",
            &[],
            &m,
        );
        assert_eq!(d.state, State::Blocked, "user rule applies");
        // The same text for a different agent is unaffected (rule is claude-only).
        let d2 = classify(
            Some("codex"),
            "here is my custom prompt >",
            true,
            false,
            "codex",
            "codex",
            &[],
            &m,
        );
        assert_eq!(d2.state, State::Idle, "user rule is scoped to its agent");
    }

    #[test]
    fn later_equal_priority_rule_overrides_managed_rule() {
        let rules = r#"
            agent = "claude"
            [[rule]]
            state = "working"
            priority = 250
            region = "screen"
            any = ["same marker"]
            [[rule]]
            state = "blocked"
            priority = 250
            region = "screen"
            any = ["same marker"]
        "#;
        let mut manifests = Manifests::builtin();
        manifests
            .rules
            .extend(toml::from_str::<ManifestFile>(rules).unwrap().into_rules());
        let detection = classify(
            Some("claude"),
            "same marker",
            true,
            false,
            "claude",
            "claude",
            &[],
            &manifests,
        );
        assert_eq!(detection.state, State::Blocked);
    }

    #[test]
    fn successful_process_scan_without_agent_is_command_fallback() {
        let detection = classify(
            Some("claude"),
            "claude welcome",
            true,
            false,
            "zsh",
            "claude",
            &["cargo test".to_string()],
            &Manifests::builtin(),
        );
        assert_eq!(detection.agent, "zsh");
        assert_eq!(detection.identity_source, "command_fallback");
    }

    /// A pane is named after the agent *running in it*, never after a word that
    /// happens to be on screen. `amp` is a substring of "example", "sample",
    /// "stamped" and "implementation", so a Claude pane printing ordinary prose
    /// renamed itself to the amp agent; listing `~/.kiro` renamed a shell to
    /// kiro. Both then propagated to the sidebar, the AGENTS list, and API clients.
    #[test]
    fn ordinary_words_do_not_name_an_agent() {
        let m = Manifests::builtin();
        // `known` empty: the pane has no established agent identity yet, which
        // is exactly when a stray word could invent one.
        let named = |title: Option<&str>, screen: &str, base: &str| {
            classify(title, screen, true, false, base, "", &[], &m).agent
        };

        for prose in [
            "for example, run the build",
            "a sample config",
            "cargo test --example foo",
            "stamped output",
            "the implementation examples",
            "champion",
        ] {
            assert_eq!(
                named(Some("zsh"), prose, "zsh"),
                "zsh",
                "ordinary prose must not name an agent: {prose:?}"
            );
        }

        // Path fragments are not agents either.
        assert_eq!(
            named(Some("zsh"), ".kiro/settings/mcp.json\n", "zsh"),
            "zsh",
            "listing ~/.kiro must not turn a shell into the kiro agent"
        );

        // Names that are also ordinary words are believed only where they were
        // placed deliberately: the spawned command or the agent's own title.
        assert_eq!(
            named(Some("zsh"), "error: cursor is out of bounds\n", "zsh"),
            "zsh",
            "a compiler error must not name the pane cursor"
        );
        assert_eq!(
            named(Some("zsh"), "warning: unused variable `droid`\n", "zsh"),
            "zsh"
        );
        assert_eq!(
            named(Some("zsh"), "you have to grok it first\n", "zsh"),
            "zsh"
        );
        // "pi" is an ordinary word (and hides in "api"/"pip"): incidental output
        // must not name it, but the spawn command and its own title do.
        assert_eq!(
            named(Some("zsh"), "pip install requests\n", "zsh"),
            "zsh",
            "the word pi in output must not name the pi agent"
        );
        assert_eq!(named(Some("zsh"), "", "pi"), "pi", "command names it");
        assert_eq!(named(Some("pi"), "", "zsh"), "pi", "title names it");
        assert_eq!(
            named(Some("zsh"), "pi-coding-agent starting\n", "zsh"),
            "pi",
            "the npm binary is distinctive enough to trust from output"
        );
        assert_eq!(named(Some("amp"), "", "zsh"), "amp", "title names it");
        assert_eq!(named(Some("zsh"), "", "droid"), "droid", "command names it");
        assert_eq!(
            named(Some("zsh"), "cursor-agent v1\n", "zsh"),
            "cursor",
            "the CLI binary is distinctive enough to trust from output"
        );

        // Distinctive names still resolve from output — bare and hyphenated.
        assert_eq!(named(Some("zsh"), "claude\n", "zsh"), "claude");
        assert_eq!(named(Some("zsh"), "claude-code v2\n", "zsh"), "claude");
    }

    /// Identity is configurable, not hardcoded: a manifest can tighten a
    /// built-in agent (drop the ambiguous `cursor`, keep `cursor-agent`) and can
    /// teach luvus an agent it has never heard of, so detection follows a new
    /// CLI without waiting for a release.
    #[test]
    fn user_manifest_can_refine_and_add_agent_identity() {
        let apply = |toml: &str, m: &mut Manifests| {
            toml::from_str::<ManifestFile>(toml)
                .unwrap()
                .apply_identity(&mut m.agents);
        };

        // Built-in: `cursor` is ambiguous, so the title still names it.
        let mut m = Manifests::builtin();
        let named = |m: &Manifests, title: Option<&str>, screen: &str, cmd: &str| {
            classify(title, screen, true, false, cmd, "", &[], m).agent
        };
        assert_eq!(named(&m, Some("cursor"), "", "zsh"), "cursor");

        // Tighten it: only the CLI binary counts now.
        apply(
            r#"
            agent = "cursor"
            [identity]
            distinct = ["cursor-agent"]
            replace = true
        "#,
            &mut m,
        );
        assert_eq!(
            named(&m, Some("cursor"), "", "zsh"),
            "zsh",
            "`replace` drops the built-in ambiguous pattern"
        );
        assert_eq!(named(&m, Some("cursor-agent"), "", "zsh"), "cursor");

        // Teach it an agent that does not exist in the built-in registry.
        let mut m2 = Manifests::builtin();
        assert!(!m2.is_agent("nimbus"));
        apply(
            r#"
            agent = "nimbus"
            [identity]
            distinct = ["nimbus-cli"]
            ambiguous = ["nimbus"]
        "#,
            &mut m2,
        );
        assert!(m2.is_agent("nimbus"), "the new agent is recognised");
        assert_eq!(
            named(&m2, Some("zsh"), "nimbus-cli v3 ready\n", "zsh"),
            "nimbus",
            "a distinct pattern is trusted from pane output"
        );
        assert_eq!(
            named(&m2, Some("zsh"), "the nimbus is a cloud\n", "zsh"),
            "zsh",
            "an ambiguous pattern is not trusted from pane output"
        );
    }

    /// Identity comes from the pane's processes, because an agent is a program
    /// and not a word on a screen. This is the signal that survives a pane
    /// printing prose about other agents — the failure that put "amp" and
    /// "kiro" in the sidebar for panes running neither.
    #[test]
    fn running_process_outranks_screen_text() {
        let m = Manifests::builtin();
        let named = |screen: &str, running: &[String]| {
            classify(Some("zsh"), screen, true, false, "zsh", "", running, &m).agent
        };
        let proc = |c: &str| vec![c.to_string()];

        // The screen is talking about other agents; the process is the truth.
        assert_eq!(
            named("see the claude docs\n", &proc("/opt/homebrew/bin/amp")),
            "amp",
            "a real amp process beats the word claude on screen"
        );
        assert_eq!(
            named(
                "installing opencode from npm\n",
                &proc("node /usr/local/bin/claude")
            ),
            "claude"
        );

        // A shell that merely mentions agents is still just a shell.
        assert_eq!(
            named("see the claude docs\n", &proc("-zsh")),
            "zsh",
            "no agent process means no agent, whatever the screen says"
        );
        assert_eq!(
            named("the copilot subscription page\n", &proc("-zsh")),
            "zsh"
        );
        assert_eq!(named("gemini is a constellation\n", &proc("-zsh")), "zsh");

        // Flags never count as the binary, and .exe is stripped.
        assert_eq!(named("", &proc("cargo test --example amp")), "zsh");
        assert_eq!(named("", &proc("C:\\tools\\droid.exe")), "droid");

        // No scan available (Windows / remote / `ps` failed) → text heuristics,
        // which must keep working rather than reporting "no agents at all".
        assert_eq!(named("claude\n", &[]), "claude");
    }
}
