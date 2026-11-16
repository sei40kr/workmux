//! herdr-app backend implementation for the Multiplexer trait.
//!
//! herdr-app (https://herdr.dev) shows spaces, tabs and panes, and herdr-core
//! (`hc`) runs the terminals in them. Spaces map to tmux sessions, tabs to
//! tmux windows, and panes to tmux panes. Pane IDs are hc terminal addresses
//! (`t7`), as in the `$HC_TERMINAL_ID` hc sets in every terminal.
//!
//! The layout is read and changed through `herdr-app ls` and `herdr-app do`,
//! and terminals are read and typed into through `hc`.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::SplitDirection;
use crate::shell::shell_quote;

use super::Multiplexer;
use super::types::*;
use super::util;

/// Run a program and return its trimmed stdout. herdr-app reports errors on
/// stdout and hc on stderr, so a failure carries both.
fn run(cmd: &mut Command, input: Option<&str>) -> Result<String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to run {program}"))?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(input.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("{program} failed: {stdout} {}", stderr.trim()));
    }
    Ok(stdout)
}

fn hc(args: &[&str]) -> Result<String> {
    run(Command::new("hc").args(args), None)
}

/// A shell command that runs `herdr-app do` and discards its output.
fn shell_app_do(command: &str, args: Value) -> String {
    format!(
        "herdr-app do {command} {} >/dev/null 2>&1",
        shell_quote(&args.to_string())
    )
}

/// `herdr-app ls`: spaces, their tabs, and the panes in each tab.
#[derive(Debug, Deserialize)]
struct Layout {
    #[serde(default)]
    spaces: Vec<LayoutSpace>,
}

#[derive(Debug, Deserialize)]
struct LayoutSpace {
    id: String,
    name: Option<String>,
    #[serde(default)]
    tabs: Vec<LayoutTab>,
}

#[derive(Debug, Deserialize)]
struct LayoutTab {
    id: String,
    name: Option<String>,
    #[serde(default)]
    terminals: Vec<LayoutPane>,
}

#[derive(Debug, Deserialize)]
struct LayoutPane {
    /// None for a pane that shows a mod instead of a terminal.
    terminal: Option<String>,
    pane: String,
}

#[derive(Debug, Clone)]
struct Space {
    id: String,
    name: String,
}

/// A terminal pane with the space and tab it is in.
#[derive(Debug, Clone)]
struct Pane {
    terminal: String,
    pane: String,
    space_id: String,
    space: String,
    tab_id: String,
    tab: String,
    /// Position of the tab in its space (1-based).
    tab_index: u32,
}

#[derive(Debug, Default)]
struct Snapshot {
    spaces: Vec<Space>,
    panes: Vec<Pane>,
    /// The space `$HC_TERMINAL_ID` is in.
    current_space_id: Option<String>,
}

impl Snapshot {
    fn parse(output: &str, current_terminal: Option<&str>) -> Result<Self> {
        let layout: Layout =
            serde_json::from_str(output).context("Failed to parse the herdr-app layout")?;
        let mut snapshot = Self::default();
        for space in layout.spaces {
            let space_name = space.name.unwrap_or_default();
            for (index, tab) in space.tabs.into_iter().enumerate() {
                let tab_name = tab.name.unwrap_or_default();
                for entry in tab.terminals {
                    let Some(terminal) = entry.terminal else {
                        continue;
                    };
                    if current_terminal == Some(terminal.as_str()) {
                        snapshot.current_space_id = Some(space.id.clone());
                    }
                    snapshot.panes.push(Pane {
                        terminal,
                        pane: entry.pane,
                        space_id: space.id.clone(),
                        space: space_name.clone(),
                        tab_id: tab.id.clone(),
                        tab: tab_name.clone(),
                        tab_index: index as u32 + 1,
                    });
                }
            }
            snapshot.spaces.push(Space {
                id: space.id,
                name: space_name,
            });
        }
        Ok(snapshot)
    }

    fn pane(&self, terminal: &str) -> Option<&Pane> {
        self.panes.iter().find(|p| p.terminal == terminal)
    }

    fn require_pane(&self, terminal: &str) -> Result<&Pane> {
        self.pane(terminal)
            .ok_or_else(|| anyhow!("Pane {} not found", terminal))
    }

    fn in_current_space(&self, pane: &Pane) -> bool {
        self.current_space_id
            .as_ref()
            .is_none_or(|id| *id == pane.space_id)
    }

    /// The space with this name or ID. Names need not be unique, so the
    /// current space wins, then the first one in the window's order.
    fn space_id(&self, name_or_id: &str) -> Option<String> {
        let matching: Vec<&Space> = self
            .spaces
            .iter()
            .filter(|s| s.name == name_or_id || s.id == name_or_id)
            .collect();
        matching
            .iter()
            .find(|s| self.current_space_id.as_ref() == Some(&s.id))
            .or(matching.first())
            .map(|s| s.id.clone())
    }

    fn require_space_id(&self, name: &str) -> Result<String> {
        self.space_id(name)
            .ok_or_else(|| anyhow!("Space '{}' not found", name))
    }

    /// Panes of the tab with this name, preferring the current space.
    fn window_panes(&self, full_name: &str) -> Vec<&Pane> {
        let named: Vec<&Pane> = self.panes.iter().filter(|p| p.tab == full_name).collect();
        if named
            .iter()
            .any(|p| self.current_space_id.as_ref() == Some(&p.space_id))
        {
            named
                .into_iter()
                .filter(|p| self.in_current_space(p))
                .collect()
        } else {
            named
        }
    }

    fn require_window(&self, full_name: &str) -> Result<&Pane> {
        self.window_panes(full_name)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Window '{}' not found", full_name))
    }

    /// The tab a target names: by name within its parent space when it has
    /// one, otherwise as `window_panes` resolves the name.
    fn target_window(&self, target: &WindowTarget) -> Option<&Pane> {
        match target.parent_session() {
            Some(space) => self
                .panes
                .iter()
                .find(|p| p.tab == target.full_name && p.space == space),
            None => self.window_panes(&target.full_name).into_iter().next(),
        }
    }

    fn require_target_window(&self, target: &WindowTarget) -> Result<&Pane> {
        self.target_window(target)
            .ok_or_else(|| match target.parent_session() {
                Some(space) => anyhow!(
                    "Window '{}' not found in session '{}'",
                    target.full_name,
                    space
                ),
                None => anyhow!("Window '{}' not found", target.full_name),
            })
    }
}

/// A terminal's facts from hc's `terminal.list`.
#[derive(Debug, Deserialize)]
struct TerminalFacts {
    address: String,
    pid: Option<u32>,
    cwd: Option<String>,
    title: Option<String>,
    foreground: Option<Foreground>,
}

#[derive(Debug, Deserialize)]
struct Foreground {
    name: Option<String>,
    cwd: Option<String>,
}

impl TerminalFacts {
    fn cwd_path(&self) -> PathBuf {
        self.foreground
            .as_ref()
            .and_then(|fg| fg.cwd.as_deref())
            .or(self.cwd.as_deref())
            .map(PathBuf::from)
            .unwrap_or_default()
    }

    fn live_pane_info(&self, pane: &Pane) -> (String, LivePaneInfo) {
        let (pane_id, mut info) = util::LivePaneSnapshot {
            pane_id: pane.terminal.clone(),
            pid: self.pid,
            current_command: self.foreground.as_ref().and_then(|fg| fg.name.clone()),
            working_dir: self.cwd_path(),
            title: self.title.clone().unwrap_or_default(),
            session: pane.space.clone(),
            window: pane.tab.clone(),
        }
        .into_pair();
        info.session_id = Some(pane.space_id.clone());
        info.window_id = Some(pane.tab_id.clone());
        info.window_index = Some(pane.tab_index);
        (pane_id, info)
    }
}

/// hc key names that type `text`, or None when a character has no key.
fn text_keys(text: &str) -> Option<Vec<String>> {
    text.chars()
        .map(|c| match c {
            ' ' => Some("space".to_string()),
            '\n' | '\r' => Some("enter".to_string()),
            '\t' => Some("tab".to_string()),
            c if c.is_ascii_alphanumeric() || "-=[]\\;',./`".contains(c) => Some(c.to_string()),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Default, Clone)]
pub struct HerdrAppBackend;

impl HerdrAppBackend {
    pub fn new() -> Self {
        Self
    }

    /// Run a herdr-app command and return its JSON result.
    fn app_do(&self, command: &str, args: Value) -> Result<Value> {
        let output = run(
            Command::new("herdr-app").args(["do", command, &args.to_string()]),
            None,
        )
        .with_context(|| format!("herdr-app command {command} failed"))?;
        serde_json::from_str(&output)
            .with_context(|| format!("Failed to parse the result of herdr-app command {command}"))
    }

    fn snapshot(&self) -> Result<Snapshot> {
        let output = run(Command::new("herdr-app").arg("ls"), None)
            .context("Failed to list herdr-app spaces")?;
        Snapshot::parse(&output, self.current_pane_id().as_deref())
    }

    /// Every terminal's pid, folder and foreground program, in one hc call.
    fn terminal_facts(&self) -> Result<HashMap<String, TerminalFacts>> {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.list", "params": {}});
        let output = run(
            Command::new("hc").arg("connect"),
            Some(&format!("{request}\n")),
        )
        .context("Failed to list hc terminals")?;
        let mut response: Value = output
            .lines()
            .next()
            .map(serde_json::from_str)
            .transpose()?
            .ok_or_else(|| anyhow!("hc returned no terminal list"))?;
        if let Some(error) = response.get("error") {
            return Err(anyhow!("hc failed to list terminals: {error}"));
        }
        let terminals: Vec<TerminalFacts> =
            serde_json::from_value(response["result"]["terminals"].take())
                .context("Failed to parse the hc terminal list")?;
        Ok(terminals
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .collect())
    }

    fn created_terminal(result: &Value) -> Result<String> {
        result["terminal"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("herdr-app did not report the new terminal"))
    }

    /// The trait creates windows and sessions without showing them, but
    /// herdr-app shows every new tab and space, so put the shown tab back.
    fn create_in_background(&self, command: &str, args: Value) -> Result<String> {
        let shown = self
            .app_do_status()
            .ok()
            .and_then(|status| status["tab"].as_str().map(str::to_string));
        let result = self.app_do(command, args)?;
        if let Some(tab) = shown {
            let _ = self.app_do("tab.select", json!({ "id": tab }));
        }
        Self::created_terminal(&result)
    }

    fn app_do_status(&self) -> Result<Value> {
        let output = run(Command::new("herdr-app").arg("status"), None)?;
        Ok(serde_json::from_str(&output)?)
    }

    /// `sh -c` arguments that run `command` (or a login shell) in `cwd`.
    /// A split starts in the folder of the pane it splits, not one given to it.
    fn shell_argv(cwd: &Path, command: Option<&str>) -> Value {
        let command = match command {
            Some(command) => command.to_string(),
            None => {
                let shell = util::default_shell("/bin/sh").unwrap_or_else(|_| "/bin/sh".into());
                format!("exec {} -l", shell_quote(&shell))
            }
        };
        let cd = shell_quote(&cwd.to_string_lossy());
        json!(["sh", "-c", format!("cd {cd} || exit; {command}")])
    }

    fn close_tab(&self, pane: &Pane) -> Result<()> {
        self.app_do("tab.close", json!({ "id": pane.tab_id, "confirmed": true }))
            .map(|_| ())
    }

    fn close_tab_shell_cmd(pane: &Pane) -> String {
        shell_app_do("tab.close", json!({ "id": pane.tab_id, "confirmed": true }))
    }

    fn close_space_shell_cmd(space_id: &str) -> String {
        shell_app_do("space.close", json!({ "id": space_id, "confirmed": true }))
    }

    fn select_tab(&self, pane: &Pane) -> Result<()> {
        self.app_do("tab.select", json!({ "id": pane.tab_id }))
            .map(|_| ())
    }

    fn focus_terminal(&self, terminal: &str) -> Result<()> {
        self.app_do("pane.focus", json!({ "terminal": terminal }))
            .map(|_| ())
    }

    fn rename_tab(&self, pane: &Pane, name: &str) -> Result<()> {
        self.app_do("tab.rename", json!({ "id": pane.tab_id, "name": name }))
            .map(|_| ())
    }

    fn schedule(&self, delay: Duration, command: &str) -> Result<()> {
        util::run_detached_sh_c(&format!("sleep {}; {command}", delay.as_secs_f64()))
    }
}

impl Multiplexer for HerdrAppBackend {
    fn name(&self) -> &'static str {
        "herdr-app"
    }

    fn supports_session_mode(&self) -> bool {
        true
    }

    // === Server/Session ===

    fn is_running(&self) -> Result<bool> {
        // `status` asks the open window and fails without one; it never opens it
        Ok(run(Command::new("herdr-app").arg("status"), None).is_ok())
    }

    fn current_session_id(&self) -> Result<Option<String>> {
        Ok(self.snapshot()?.current_space_id)
    }

    /// Closing a space moves no clients, so `preferred_session` is unused.
    fn shell_close_session_by_id_guard_cmd(
        &self,
        id: &str,
        _preferred_session: Option<&str>,
    ) -> Result<String> {
        Ok(format!("{} || true", Self::close_space_shell_cmd(id)))
    }

    fn current_pane_id(&self) -> Option<String> {
        std::env::var("HC_TERMINAL_ID").ok()
    }

    fn active_pane_id(&self) -> Option<String> {
        let focused = self.app_do_status().ok()?["focused_pane"]
            .as_str()?
            .to_string();
        self.snapshot()
            .ok()?
            .panes
            .into_iter()
            .find(|p| p.pane == focused)
            .map(|p| p.terminal)
    }

    fn get_client_active_pane_path(&self) -> Result<PathBuf> {
        let terminal = self
            .current_pane_id()
            .ok_or_else(|| anyhow!("HC_TERMINAL_ID not set"))?;
        let path = self
            .terminal_facts()?
            .get(&terminal)
            .map(TerminalFacts::cwd_path)
            .unwrap_or_default();
        if path.as_os_str().is_empty() {
            return Err(anyhow!("hc did not report a folder for {}", terminal));
        }
        Ok(path)
    }

    // === Window/Tab Management ===

    fn create_window(&self, params: CreateWindowParams) -> Result<String> {
        let mut args = json!({
            "name": util::prefixed(params.prefix, params.name),
            "folder": params.cwd.to_string_lossy(),
        });
        // herdr-app appends new tabs at the end; params.after_window is ignored.
        // Outside an hc terminal, the tab goes to the shown space.
        if let Some(space) = self.snapshot()?.current_space_id {
            args["space"] = json!(space);
        }
        self.create_in_background("tab.new", args)
            .context("Failed to create herdr-app tab")
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<String> {
        let mut args = json!({
            "name": util::prefixed(params.prefix, params.name),
            "folder": params.cwd.to_string_lossy(),
        });
        if let Some(name) = params.initial_window_name {
            args["tab"] = json!(name);
        }
        self.create_in_background("space.new", args)
            .context("Failed to create herdr-app space")
    }

    fn create_window_in_session(&self, params: CreateWindowInSessionParams) -> Result<String> {
        let space = self.snapshot()?.require_space_id(params.session_name)?;
        let mut args = json!({ "space": space, "folder": params.cwd.to_string_lossy() });
        if let Some(name) = params.name {
            args["name"] = json!(name);
        }
        self.create_in_background("tab.new", args)
            .context("Failed to create herdr-app tab")
    }

    fn switch_to_session(&self, prefix: &str, name: &str) -> Result<()> {
        let space = self
            .snapshot()?
            .require_space_id(&util::prefixed(prefix, name))?;
        self.app_do("space.select", json!({ "id": space }))?;
        Ok(())
    }

    /// Accepts a space name or a stable space ID.
    fn session_exists(&self, full_name: &str) -> Result<bool> {
        Ok(self.snapshot()?.space_id(full_name).is_some())
    }

    fn kill_session(&self, full_name: &str) -> Result<()> {
        if let Some(space) = self.snapshot()?.space_id(full_name) {
            self.app_do("space.close", json!({ "id": space, "confirmed": true }))
                .context("Failed to close herdr-app space")?;
        }
        Ok(())
    }

    fn kill_window(&self, full_name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        if let Some(pane) = snapshot.window_panes(full_name).first() {
            self.close_tab(pane)
                .context("Failed to close herdr-app tab")?;
        }
        Ok(())
    }

    fn rename_window(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        self.rename_tab(snapshot.require_window(old_full_name)?, new_full_name)
    }

    fn rename_window_at_pane(&self, pane_id: &str, new_name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        self.rename_tab(snapshot.require_pane(pane_id)?, new_name)
    }

    fn rename_session(&self, old_full_name: &str, new_full_name: &str) -> Result<()> {
        let space = self.snapshot()?.require_space_id(old_full_name)?;
        self.app_do(
            "space.rename",
            json!({ "id": space, "name": new_full_name }),
        )?;
        Ok(())
    }

    fn schedule_window_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        let snapshot = self.snapshot()?;
        match snapshot.window_panes(full_name).first() {
            Some(pane) => self.schedule(delay, &Self::close_tab_shell_cmd(pane)),
            None => Ok(()),
        }
    }

    fn schedule_session_close(&self, full_name: &str, delay: Duration) -> Result<()> {
        match self.snapshot()?.space_id(full_name) {
            Some(space) => self.schedule(delay, &Self::close_space_shell_cmd(&space)),
            None => Ok(()),
        }
    }

    fn run_deferred_script(&self, script: &str) -> Result<()> {
        util::run_detached_sh_c(script)
    }

    // Window targets name their parent space. The trait defaults look only in
    // the current space, so these look in the parent space instead.

    fn window_target_exists(&self, target: &WindowTarget) -> Result<bool> {
        Ok(self.snapshot()?.target_window(target).is_some())
    }

    fn kill_window_target(&self, target: &WindowTarget) -> Result<()> {
        let snapshot = self.snapshot()?;
        if let Some(pane) = snapshot.target_window(target) {
            self.close_tab(pane)
                .context("Failed to close herdr-app tab")?;
        }
        Ok(())
    }

    fn schedule_window_target_close(&self, target: &WindowTarget, delay: Duration) -> Result<()> {
        let snapshot = self.snapshot()?;
        match snapshot.target_window(target) {
            Some(pane) => self.schedule(delay, &Self::close_tab_shell_cmd(pane)),
            None => Ok(()),
        }
    }

    fn shell_kill_window_target_cmd(&self, target: &WindowTarget) -> Result<String> {
        let snapshot = self.snapshot()?;
        Ok(Self::close_tab_shell_cmd(
            snapshot.require_target_window(target)?,
        ))
    }

    fn select_window_target(&self, target: &WindowTarget) -> Result<()> {
        let snapshot = self.snapshot()?;
        self.select_tab(snapshot.require_target_window(target)?)
    }

    fn shell_select_window_cmd(&self, full_name: &str) -> Result<String> {
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_window(full_name)?;
        Ok(shell_app_do("tab.select", json!({ "id": pane.tab_id })))
    }

    fn shell_kill_window_cmd(&self, full_name: &str) -> Result<String> {
        let snapshot = self.snapshot()?;
        Ok(Self::close_tab_shell_cmd(
            snapshot.require_window(full_name)?,
        ))
    }

    fn shell_switch_session_cmd(&self, full_name: &str) -> Result<String> {
        let space = self.snapshot()?.require_space_id(full_name)?;
        Ok(shell_app_do("space.select", json!({ "id": space })))
    }

    fn shell_kill_session_cmd(&self, full_name: &str) -> Result<String> {
        let space = self.snapshot()?.require_space_id(full_name)?;
        Ok(Self::close_space_shell_cmd(&space))
    }

    fn select_window(&self, prefix: &str, name: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        self.select_tab(snapshot.require_window(&util::prefixed(prefix, name))?)
    }

    fn current_window_name(&self) -> Result<Option<String>> {
        let Some(terminal) = self.current_pane_id() else {
            return Ok(None);
        };
        Ok(self.snapshot()?.pane(&terminal).map(|p| p.tab.clone()))
    }

    fn get_all_window_names(&self) -> Result<HashSet<String>> {
        let snapshot = self.snapshot()?;
        Ok(snapshot
            .panes
            .iter()
            .filter(|p| snapshot.in_current_space(p))
            .map(|p| p.tab.clone())
            .collect())
    }

    fn get_window_names_in_session(&self, session_name: &str) -> Result<HashSet<String>> {
        Ok(self
            .snapshot()?
            .panes
            .into_iter()
            .filter(|p| p.space == session_name)
            .map(|p| p.tab)
            .collect())
    }

    fn get_all_windows_with_sessions(&self) -> Result<HashSet<(String, String)>> {
        Ok(self
            .snapshot()?
            .panes
            .into_iter()
            .map(|p| (p.tab, p.space))
            .collect())
    }

    fn get_all_session_names(&self) -> Result<HashSet<String>> {
        Ok(self
            .snapshot()?
            .spaces
            .into_iter()
            .map(|s| s.name)
            .collect())
    }

    fn get_all_window_names_all_sessions(&self) -> Result<HashSet<String>> {
        Ok(self.snapshot()?.panes.into_iter().map(|p| p.tab).collect())
    }

    fn wait_until_session_closed(&self, full_session_name: &str) -> Result<()> {
        while self.session_exists(full_session_name)? {
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    // === Pane Management ===

    fn select_pane(&self, pane_id: &str) -> Result<()> {
        self.focus_terminal(pane_id)
    }

    fn zoom_pane(&self, pane_id: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
        // pane.zoom toggles, so a zoomed pane is unzoomed
        self.app_do("pane.zoom", json!({ "pane": pane.pane }))?;
        Ok(())
    }

    fn switch_to_pane(&self, pane_id: &str, _window_hint: Option<&str>) -> Result<()> {
        // Focusing a terminal shows its space and tab too
        self.focus_terminal(pane_id)
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        let snapshot = self.snapshot()?;
        let pane = snapshot.require_pane(pane_id)?;
        self.app_do("pane.close", json!({ "pane": pane.pane }))
            .context("Failed to close herdr-app pane")?;
        Ok(())
    }

    fn respawn_pane(&self, pane_id: &str, cwd: &Path, cmd: Option<&str>) -> Result<String> {
        // herdr-app can't restart a pane in place: split beside it, then close
        // it. The new pane takes the old one's whole space once it is gone.
        let new_pane_id =
            self.split_pane(pane_id, &SplitDirection::Horizontal, cwd, None, None, cmd)?;
        self.kill_pane(pane_id)?;
        Ok(new_pane_id)
    }

    fn capture_pane(&self, pane_id: &str, lines: u16) -> Option<String> {
        let output = hc(&["read", "--lines", &lines.to_string(), "--", pane_id]).ok()?;
        Some(util::tail_lines(&output, lines))
    }

    // === Text I/O ===

    fn send_text_fragment(&self, pane_id: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        // shortcut: hc can only press named keys without submitting, so text
        // with shifted symbols or non-ASCII fails; use hc raw input once it has one.
        let keys = text_keys(text).ok_or_else(|| {
            anyhow!("herdr-app can only type letters, digits and unshifted symbols without pressing Enter: {text:?}")
        })?;
        let mut args = vec!["keys", "--", pane_id];
        args.extend(keys.iter().map(String::as_str));
        hc(&args).context("Failed to send text to pane")?;
        Ok(())
    }

    /// `hc send` pastes the text and presses Enter in one call, so any text works.
    fn send_keys(&self, pane_id: &str, command: &str) -> Result<()> {
        // --force: type even into a program waiting on a dialog
        hc(&["send", "--force", "--", pane_id, command]).context("Failed to send keys to pane")?;
        Ok(())
    }

    fn send_enter(&self, pane_id: &str) -> Result<()> {
        self.send_key(pane_id, "Enter")
    }

    fn send_key(&self, pane_id: &str, key: &str) -> Result<()> {
        // hc names keys the way Rex does
        let key = super::rex::RexBackend::rex_key(key);
        hc(&["keys", "--", pane_id, &key]).context("Failed to send key to pane")?;
        Ok(())
    }

    fn paste_text(&self, pane_id: &str, content: &str) -> Result<()> {
        self.send_text_fragment(pane_id, content)
    }

    fn paste_and_submit(&self, pane_id: &str, content: &str) -> Result<()> {
        self.send_keys(pane_id, content)
    }

    fn paste_multiline(&self, pane_id: &str, content: &str) -> Result<()> {
        self.send_keys(pane_id, content)
    }

    // === Status ===

    fn set_status(&self, _pane_id: &str, _icon: &str, _auto_clear_on_focus: bool) -> Result<()> {
        // Agent state lives in the StateStore. herdr-app shows agent status
        // itself from what hc observes, so no UI update is needed.
        Ok(())
    }

    fn clear_status(&self, _pane_id: &str) -> Result<()> {
        Ok(())
    }

    fn ensure_status_format(&self, _pane_id: &str) -> Result<()> {
        Ok(())
    }

    // === Multi-Session Support ===

    fn current_session(&self) -> Option<String> {
        let snapshot = self.snapshot().ok()?;
        let current = snapshot.current_space_id.as_ref()?;
        snapshot
            .spaces
            .iter()
            .find(|s| &s.id == current)
            .map(|s| s.name.clone())
    }

    // === State Reconciliation ===

    fn instance_id(&self) -> String {
        self.resolve_instance_id()
            .unwrap_or_else(|_| "default".to_string())
    }

    fn resolve_instance_id(&self) -> Result<String> {
        // HC_TERMINAL_SOCKET names the herdr-core socket, which every
        // space and tab on it shares, matching tmux server scope.
        std::env::var("HC_TERMINAL_SOCKET")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                anyhow!("HC_TERMINAL_SOCKET is required to resolve the herdr-app instance")
            })
    }

    fn get_live_pane_info(&self, pane_id: &str) -> Result<Option<LivePaneInfo>> {
        let snapshot = self.snapshot()?;
        let Some(pane) = snapshot.pane(pane_id) else {
            return Ok(None);
        };
        Ok(self
            .terminal_facts()?
            .get(pane_id)
            .map(|facts| facts.live_pane_info(pane).1))
    }

    fn get_all_live_pane_info(&self) -> Result<HashMap<String, LivePaneInfo>> {
        let snapshot = self.snapshot()?;
        let facts = self.terminal_facts()?;
        Ok(snapshot
            .panes
            .iter()
            .filter_map(|pane| facts.get(&pane.terminal).map(|f| f.live_pane_info(pane)))
            .collect())
    }

    fn split_pane(
        &self,
        target_pane_id: &str,
        direction: &SplitDirection,
        cwd: &Path,
        _size: Option<u16>,
        _percentage: Option<u8>,
        command: Option<&str>,
    ) -> Result<String> {
        let direction = match direction {
            SplitDirection::Horizontal => "right",
            SplitDirection::Vertical => "down",
            SplitDirection::Stacked => {
                return Err(anyhow!(
                    "split: stacked is only supported by the Zellij backend"
                ));
            }
        };
        let snapshot = self.snapshot()?;
        let target = snapshot.require_pane(target_pane_id)?;
        // herdr-app splits in half; it takes no size
        let result = self
            .app_do(
                "pane.split",
                json!({
                    "direction": direction,
                    "pane": target.pane,
                    "argv": Self::shell_argv(cwd, command),
                }),
            )
            .context("Failed to split herdr-app pane")?;
        Self::created_terminal(&result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: &str = r#"{"spaces":[
        {"id":"sp1","name":"work","tabs":[
            {"id":"tb1","name":"wm-feature","terminals":[
                {"terminal":"t1","pane":"p1"},{"terminal":"t2","pane":"p2"}]},
            {"id":"tb2","name":"2","terminals":[{"terminal":null,"pane":"p3"}]}]},
        {"id":"sp2","name":"work","tabs":[
            {"id":"tb3","name":"wm-feature","terminals":[{"terminal":"t3","pane":"p4"}]}]},
        {"id":"sp3","name":null,"tabs":[]}]}"#;

    #[test]
    fn parses_the_layout() {
        let snapshot = Snapshot::parse(LAYOUT, Some("t3")).unwrap();
        // Mod panes have no terminal and are skipped
        assert_eq!(snapshot.panes.len(), 3);
        assert_eq!(snapshot.current_space_id.as_deref(), Some("sp2"));
        let pane = snapshot.pane("t2").unwrap();
        assert_eq!(
            (pane.pane.as_str(), pane.tab_id.as_str(), pane.tab_index),
            ("p2", "tb1", 1)
        );
        // A space without tabs is still known
        assert_eq!(snapshot.space_id("sp3").as_deref(), Some("sp3"));
    }

    #[test]
    fn duplicate_names_prefer_the_current_space() {
        let inside = Snapshot::parse(LAYOUT, Some("t3")).unwrap();
        assert_eq!(inside.space_id("work").as_deref(), Some("sp2"));
        assert_eq!(inside.require_window("wm-feature").unwrap().terminal, "t3");

        let outside = Snapshot::parse(LAYOUT, None).unwrap();
        assert_eq!(outside.space_id("work").as_deref(), Some("sp1"));
        assert_eq!(outside.window_panes("wm-feature").len(), 3);
    }

    #[test]
    fn target_window_looks_in_the_parent_space() {
        let snapshot = Snapshot::parse(LAYOUT, Some("t1")).unwrap();
        let target = WindowTarget::new("wm-feature".into(), Some("work".into()));
        assert!(snapshot.target_window(&target).is_some());
        let missing = WindowTarget::new("wm-feature".into(), Some("other".into()));
        assert!(snapshot.target_window(&missing).is_none());
    }

    #[test]
    fn shell_argv_starts_in_the_folder() {
        assert_eq!(
            HerdrAppBackend::shell_argv(Path::new("/tmp/my dir"), Some("echo 'hi'")),
            json!(["sh", "-c", "cd '/tmp/my dir' || exit; echo 'hi'"])
        );
        let login = HerdrAppBackend::shell_argv(Path::new("/tmp"), None);
        assert!(login[2].as_str().unwrap().ends_with(" -l"));
    }

    #[test]
    fn text_keys_names_the_keys_hc_can_press() {
        assert_eq!(
            text_keys("ls -a\n").unwrap(),
            vec!["l", "s", "space", "-", "a", "enter"]
        );
        assert!(text_keys("echo $HOME").is_none());
        assert!(text_keys("ä").is_none());
    }

    #[test]
    fn reads_terminal_facts() {
        let facts: TerminalFacts = serde_json::from_value(json!({
            "address": "t1", "pid": 42, "cwd": "/a", "title": "x",
            "foreground": {"name": "claude", "cwd": "/b", "pid": 43}
        }))
        .unwrap();
        let snapshot = Snapshot::parse(LAYOUT, None).unwrap();
        let (id, info) = facts.live_pane_info(snapshot.pane("t1").unwrap());
        assert_eq!(id, "t1");
        assert_eq!(info.pid, Some(42));
        assert_eq!(info.current_command.as_deref(), Some("claude"));
        assert_eq!(info.working_dir, PathBuf::from("/b"));
        assert_eq!(info.window_id.as_deref(), Some("tb1"));
    }
}

#[cfg(test)]
mod live {
    use super::*;
    use std::thread::sleep;

    fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..60 {
            if ok() {
                return;
            }
            sleep(Duration::from_millis(250));
        }
        panic!("timed out waiting for {what}");
    }

    fn sh(script: &str) {
        assert!(
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .status()
                .unwrap()
                .success()
        );
    }

    fn shows(mux: &HerdrAppBackend, pane: &str) -> bool {
        mux.active_pane_id().as_deref() == Some(pane)
    }

    fn output_has(mux: &HerdrAppBackend, pane: &str, text: &str) -> bool {
        mux.capture_pane(pane, 200)
            .is_some_and(|o| o.contains(text))
    }

    fn window(mux: &HerdrAppBackend, name: &str, cwd: &Path) -> String {
        mux.create_window(CreateWindowParams {
            prefix: "wmt-",
            name,
            cwd,
            after_window: None,
        })
        .unwrap()
    }

    fn session(mux: &HerdrAppBackend, name: &str, cwd: &Path) -> String {
        mux.create_session(CreateSessionParams {
            prefix: "wmt-",
            name,
            cwd,
            initial_window_name: Some("first"),
        })
        .unwrap()
    }

    #[test]
    #[ignore = "drives the running herdr-app window"]
    fn every_method_against_the_running_app() {
        let mux = HerdrAppBackend::new();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let cwd = cwd.as_path();
        let me = mux.current_pane_id().expect("run inside an hc terminal");
        mux.select_pane(&me).unwrap();

        // Server and current context
        assert_eq!(mux.name(), "herdr-app");
        assert!(mux.is_running().unwrap());
        assert!(mux.supports_session_mode());
        let space = mux.current_session().unwrap();
        let space_id = mux.current_session_id().unwrap().unwrap();
        assert!(mux.session_exists(&space).unwrap());
        assert!(mux.session_exists(&space_id).unwrap());
        assert!(mux.get_all_session_names().unwrap().contains(&space));
        assert!(mux.current_window_name().unwrap().is_some());
        assert!(mux.get_client_active_pane_path().unwrap().is_dir());
        assert_eq!(mux.resolve_instance_id().unwrap(), mux.instance_id());
        assert!(shows(&mux, &me));
        mux.ensure_status_format(&me).unwrap();
        mux.set_status(&me, "x", true).unwrap();
        mux.clear_status(&me).unwrap();

        // Windows are created in the background
        let p1 = window(&mux, "one", cwd);
        assert!(shows(&mux, &me));
        assert!(mux.window_exists("wmt-", "one").unwrap());
        assert!(mux.window_exists_by_full_name("wmt-one").unwrap());
        assert!(mux.get_all_window_names().unwrap().contains("wmt-one"));
        assert!(
            mux.get_window_names_in_session(&space)
                .unwrap()
                .contains("wmt-one")
        );
        assert!(
            mux.get_all_windows_with_sessions()
                .unwrap()
                .contains(&("wmt-one".to_string(), space.clone()))
        );
        assert!(
            mux.get_all_window_names_all_sessions()
                .unwrap()
                .contains("wmt-one")
        );
        assert_eq!(
            mux.filter_active_windows(&["wmt-one".into(), "wmt-none".into()])
                .unwrap(),
            vec!["wmt-one"]
        );

        // Focus
        mux.select_window("wmt-", "one").unwrap();
        assert!(shows(&mux, &p1));
        mux.select_pane(&me).unwrap();
        assert!(shows(&mux, &me));

        // Splits and live pane info
        let p2 = mux
            .split_pane(&p1, &SplitDirection::Vertical, cwd, None, Some(30), None)
            .unwrap();
        let p3 = mux
            .split_pane(
                &p1,
                &SplitDirection::Horizontal,
                cwd,
                None,
                None,
                Some("echo split-cmd; exec sh"),
            )
            .unwrap();
        eventually("the split command", || output_has(&mux, &p3, "split-cmd"));
        assert!(
            mux.split_pane(&p1, &SplitDirection::Stacked, cwd, None, None, None)
                .is_err()
        );
        // hc reports the program a moment after the pane appears
        eventually("live pane info", || {
            mux.get_live_pane_info(&p2)
                .unwrap()
                .is_some_and(|i| i.pid.is_some() && i.current_command.is_some())
        });
        let info = mux.get_live_pane_info(&p2).unwrap().unwrap();
        assert_eq!(info.working_dir, cwd);
        assert_eq!(info.window.as_deref(), Some("wmt-one"));
        assert_eq!(info.session.as_deref(), Some(space.as_str()));
        let all = mux.get_all_live_pane_info().unwrap();
        assert!([&me, &p1, &p2, &p3].iter().all(|p| all.contains_key(*p)));
        assert!(mux.get_live_pane_info("t-missing").unwrap().is_none());

        // Text I/O (p2 runs a login shell)
        eventually("the shell prompt", || {
            mux.capture_pane(&p2, 5)
                .is_some_and(|o| !o.trim().is_empty())
        });
        mux.send_keys(&p2, "echo 'sent $ keys | ok'").unwrap();
        eventually("send_keys", || output_has(&mux, &p2, "sent $ keys | ok"));
        mux.send_text_fragment(&p2, "echo frag-ment").unwrap();
        mux.send_enter(&p2).unwrap();
        eventually("send_text_fragment", || {
            output_has(&mux, &p2, "\nfrag-ment")
        });
        assert!(mux.send_text_fragment(&p2, "echo $HOME").is_err());
        mux.paste_text(&p2, "echo pasted").unwrap();
        mux.send_key(&p2, "Enter").unwrap();
        eventually("paste_text", || output_has(&mux, &p2, "\npasted"));
        mux.paste_and_submit(&p2, "echo 'paste+submit'").unwrap();
        eventually("paste_and_submit", || {
            output_has(&mux, &p2, "\npaste+submit")
        });
        mux.paste_multiline(&p2, "echo 'multi!'").unwrap();
        eventually("paste_multiline", || output_has(&mux, &p2, "\nmulti!"));
        mux.send_keys_to_agent(&p2, "echo to-agent", None).unwrap();
        eventually("send_keys_to_agent", || output_has(&mux, &p2, "\nto-agent"));
        mux.send_prompt_to_agent(&p2, "echo to-prompt", None)
            .unwrap();
        eventually("send_prompt_to_agent", || {
            output_has(&mux, &p2, "\nto-prompt")
        });
        mux.send_key(&p2, "C-c").unwrap();
        assert!(mux.capture_pane_at(&p2, &info, 3).unwrap().lines().count() <= 3);

        // Renames
        mux.rename_window("wmt-one", "wmt-two").unwrap();
        assert!(mux.window_exists_by_full_name("wmt-two").unwrap());
        mux.rename_window_at_pane(&p1, "wmt-one").unwrap();
        assert!(mux.window_exists_by_full_name("wmt-one").unwrap());

        // Pane management
        mux.zoom_pane(&p2).unwrap();
        mux.zoom_pane(&p2).unwrap();
        mux.switch_to_pane(&p3, None).unwrap();
        assert!(shows(&mux, &p3));
        mux.select_pane(&me).unwrap();
        let p4 = mux
            .respawn_pane(&p3, cwd, Some("echo respawned; exec sh"))
            .unwrap();
        eventually("the respawned command", || {
            output_has(&mux, &p4, "respawned")
        });
        assert!(mux.get_live_pane_info(&p3).unwrap().is_none());
        mux.kill_pane(&p4).unwrap();
        assert!(mux.get_live_pane_info(&p4).unwrap().is_none());

        // Deferred shell commands
        sh(&mux.shell_select_window_cmd("wmt-one").unwrap());
        assert!(mux.active_pane_id().is_some_and(|p| p == p1 || p == p2));
        mux.select_pane(&me).unwrap();
        let marker = cwd.join("deferred");
        mux.run_deferred_script(&format!("touch {}", shell_quote(&marker.to_string_lossy())))
            .unwrap();
        eventually("run_deferred_script", || marker.exists());

        // Window targets
        let one = WindowTarget::new("wmt-one".into(), Some(space.clone()));
        let elsewhere = WindowTarget::new("wmt-one".into(), Some("no-such-space".into()));
        assert!(mux.window_target_exists(&one).unwrap());
        assert!(!mux.window_target_exists(&elsewhere).unwrap());
        mux.select_window_target(&one).unwrap();
        assert!(mux.active_pane_id().is_some_and(|p| p == p1 || p == p2));
        mux.select_pane(&me).unwrap();
        sh(&mux.shell_kill_window_target_cmd(&one).unwrap());
        assert!(!mux.window_target_exists(&one).unwrap());

        // Closing windows
        window(&mux, "a", cwd);
        mux.kill_window("wmt-a").unwrap();
        assert!(!mux.window_exists_by_full_name("wmt-a").unwrap());
        window(&mux, "b", cwd);
        sh(&mux.shell_kill_window_cmd("wmt-b").unwrap());
        assert!(!mux.window_exists_by_full_name("wmt-b").unwrap());
        window(&mux, "c", cwd);
        mux.schedule_window_close("wmt-c", Duration::from_millis(200))
            .unwrap();
        mux.wait_until_windows_closed(&["wmt-c".into()]).unwrap();
        window(&mux, "d", cwd);
        let d = WindowTarget::new("wmt-d".into(), Some(space.clone()));
        mux.schedule_window_target_close(&d, Duration::from_millis(200))
            .unwrap();
        eventually("schedule_window_target_close", || {
            !mux.window_target_exists(&d).unwrap()
        });
        window(&mux, "e", cwd);
        let e = WindowTarget::new("wmt-e".into(), None);
        mux.kill_window_target(&e).unwrap();
        assert!(!mux.window_target_exists(&e).unwrap());
        assert!(shows(&mux, &me));

        // Sessions are created in the background too
        let s1 = session(&mux, "s", cwd);
        assert!(shows(&mux, &me));
        assert!(mux.session_exists("wmt-s").unwrap());
        assert!(mux.get_all_session_names().unwrap().contains("wmt-s"));
        assert!(
            mux.get_window_names_in_session("wmt-s")
                .unwrap()
                .contains("first")
        );
        let s2 = mux
            .create_window_in_session(CreateWindowInSessionParams {
                session_name: "wmt-s",
                name: Some("second"),
                cwd,
            })
            .unwrap();
        assert!(
            mux.get_window_names_in_session("wmt-s")
                .unwrap()
                .contains("second")
        );
        assert_eq!(
            mux.get_live_pane_info(&s2).unwrap().unwrap().working_dir,
            cwd
        );
        mux.switch_to_session("wmt-", "s").unwrap();
        assert!(mux.active_pane_id().is_some_and(|p| p == s1 || p == s2));
        mux.select_pane(&me).unwrap();
        sh(&mux.shell_switch_session_cmd("wmt-s").unwrap());
        assert!(mux.active_pane_id().is_some_and(|p| p == s1 || p == s2));
        mux.select_pane(&me).unwrap();
        mux.rename_session("wmt-s", "wmt-s2").unwrap();
        assert!(mux.session_exists("wmt-s2").unwrap() && !mux.session_exists("wmt-s").unwrap());
        mux.kill_session("wmt-s2").unwrap();
        assert!(!mux.session_exists("wmt-s2").unwrap());

        session(&mux, "k", cwd);
        sh(&mux.shell_kill_session_cmd("wmt-k").unwrap());
        assert!(!mux.session_exists("wmt-k").unwrap());
        session(&mux, "l", cwd);
        mux.schedule_session_close("wmt-l", Duration::from_millis(200))
            .unwrap();
        mux.wait_until_session_closed("wmt-l").unwrap();
        session(&mux, "m", cwd);
        let m = mux.snapshot().unwrap().space_id("wmt-m").unwrap();
        sh(&mux.shell_close_session_by_id_guard_cmd(&m, None).unwrap());
        assert!(!mux.session_exists(&m).unwrap());
        // The guard tolerates a space that is already gone
        sh(&mux.shell_close_session_by_id_guard_cmd(&m, None).unwrap());
        assert!(shows(&mux, &me));
    }
}
