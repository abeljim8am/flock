//! Agent state detection via terminal tail pattern matching.
//!
//! Each pane's live bottom-of-buffer text is read periodically and matched
//! against known agent output patterns to determine state.

/// The detected state of a terminal pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Agent finished, prompt visible, nothing happening.
    Idle,
    /// Agent is actively working/processing.
    Working,
    /// Agent needs human input and is blocked on a response.
    Blocked,
    /// Plain shell or unrecognized program.
    Unknown,
}

/// Screen-derived agent state plus confidence metadata used for source arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDetection {
    pub state: AgentState,
    /// True when the current screen visibly shows live UI chrome that needs
    /// human input. This is stronger than arbitrary prompt-like text in the
    /// scrollback and may override a non-blocked integration state.
    pub visible_blocker: bool,
    /// True when the current screen visibly shows the agent's idle input UI.
    /// This lets Herdr recover from integrations that miss an interrupt/stop
    /// event without treating an empty or ambiguous screen as idle authority.
    pub visible_idle: bool,
    /// True when the current screen visibly shows live working chrome. This is
    /// narrower than a fallback `Working` heuristic and may guard against stale
    /// hook idle reports.
    pub visible_working: bool,
}

/// Which agent we detected running in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Pi,
    Claude,
    Codex,
    Gemini,
    Cursor,
    Antigravity,
    Cline,
    OpenCode,
    GithubCopilot,
    Kimi,
    Kiro,
    Droid,
    Amp,
    Grok,
    Hermes,
    Qodercli,
}

pub fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Pi => "pi",
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Gemini => "gemini",
        Agent::Cursor => "cursor",
        Agent::Antigravity => "agy",
        Agent::Cline => "cline",
        Agent::OpenCode => "opencode",
        Agent::GithubCopilot => "copilot",
        Agent::Kimi => "kimi",
        Agent::Kiro => "kiro",
        Agent::Droid => "droid",
        Agent::Amp => "amp",
        Agent::Grok => "grok",
        Agent::Hermes => "hermes",
        Agent::Qodercli => "qodercli",
    }
}

pub fn parse_agent_label(agent: &str) -> Option<Agent> {
    let name = agent.trim().to_lowercase();
    match name.as_str() {
        "pi" => Some(Agent::Pi),
        "claude" | "claude-code" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        "gemini" => Some(Agent::Gemini),
        "cursor" | "cursor-agent" => Some(Agent::Cursor),
        "agy" | "antigravity" | "antigravity-cli" => Some(Agent::Antigravity),
        "cline" => Some(Agent::Cline),
        "opencode" | "open-code" => Some(Agent::OpenCode),
        "copilot" | "github-copilot" | "ghcs" => Some(Agent::GithubCopilot),
        "kimi" | "kimi-code" | "kimi code" => Some(Agent::Kimi),
        "kiro" | "kiro-cli" => Some(Agent::Kiro),
        "droid" => Some(Agent::Droid),
        "amp" | "amp-local" => Some(Agent::Amp),
        "grok" | "grok-build" => Some(Agent::Grok),
        "hermes" | "hermes-agent" => Some(Agent::Hermes),
        "qodercli" | "qoderclicn" | "qoder" | "qodercn" => Some(Agent::Qodercli),
        _ => None,
    }
}

/// Identify which agent is running from the process name.
/// Returns `None` for plain shells or unrecognized programs.
pub fn identify_agent(process_name: &str) -> Option<Agent> {
    let name = process_name.to_lowercase();
    // Match against known binary names
    match name.as_str() {
        "pi" => Some(Agent::Pi),
        "claude" | "claude-code" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        "gemini" => Some(Agent::Gemini),
        "cursor" | "cursor-agent" => Some(Agent::Cursor),
        "agy" | "antigravity" | "antigravity-cli" => Some(Agent::Antigravity),
        "cline" => Some(Agent::Cline),
        "opencode" | "open-code" => Some(Agent::OpenCode),
        "copilot" | "github-copilot" | "ghcs" => Some(Agent::GithubCopilot),
        "kimi" | "kimi-code" | "kimi code" => Some(Agent::Kimi),
        "kiro" | "kiro-cli" => Some(Agent::Kiro),
        "droid" => Some(Agent::Droid),
        "amp" | "amp-local" => Some(Agent::Amp),
        "grok" | "grok-build" => Some(Agent::Grok),
        "hermes" | "hermes-agent" => Some(Agent::Hermes),
        "qodercli" | "qoderclicn" | "qoder" | "qodercn" => Some(Agent::Qodercli),
        _ => None,
    }
}

/// Detect the state of an agent from the live terminal tail snapshot.
/// If `agent` is `None`, returns `Unknown`.
#[cfg(test)]
pub fn detect_state(agent: Option<Agent>, screen_content: &str) -> AgentState {
    detect_agent(agent, screen_content).state
}

/// Detect state and whether a visible blocker is present on the current screen.
pub fn detect_agent(agent: Option<Agent>, screen_content: &str) -> AgentDetection {
    let Some(agent) = agent else {
        return AgentDetection {
            state: AgentState::Unknown,
            visible_blocker: false,
            visible_idle: false,
            visible_working: false,
        };
    };
    let state = match agent {
        Agent::Pi => detect_pi(screen_content),
        Agent::Claude => detect_claude(screen_content),
        Agent::Codex => detect_codex(screen_content),
        Agent::Gemini => detect_gemini(screen_content),
        Agent::Cursor => detect_cursor(screen_content),
        Agent::Antigravity => detect_antigravity(screen_content),
        Agent::Cline => detect_cline(screen_content),
        Agent::OpenCode => detect_opencode(screen_content),
        Agent::GithubCopilot => detect_github_copilot(screen_content),
        Agent::Kimi => detect_kimi(screen_content),
        Agent::Kiro => detect_kiro(screen_content),
        Agent::Droid => detect_droid(screen_content),
        Agent::Amp => detect_amp(screen_content),
        Agent::Grok => detect_grok(screen_content),
        Agent::Hermes => detect_hermes(screen_content),
        Agent::Qodercli => detect_qodercli(screen_content),
    };
    AgentDetection {
        state,
        visible_blocker: has_visible_blocker(agent, screen_content, state),
        visible_idle: has_visible_idle(agent, screen_content, state),
        visible_working: has_visible_working(agent, screen_content, state),
    }
}

// ---------------------------------------------------------------------------
// Per-agent detectors
// ---------------------------------------------------------------------------

fn detect_pi(content: &str) -> AgentState {
    // pi shows "Working..." when the agent is processing
    if content.contains("Working...") {
        return AgentState::Working;
    }
    AgentState::Idle
}

/// Claude Code detection. The most complex — it has a structured prompt box UI.
///
/// Screen layout:
/// ```text
///   (agent output / tool results)
///   ───────────────────────── (top border)
///   ❯ _                      (prompt line)
///   ───────────────────────── (bottom border)
/// ```
fn detect_claude(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Search prompt is always idle
    if content.contains("⌕ Search…") {
        return AgentState::Idle;
    }

    // ctrl+r toggle — don't change state
    // (we return Idle as a safe default since we don't have previous state here)
    if lower.contains("ctrl+r to toggle") {
        return AgentState::Idle;
    }

    // --- Blocked detection (full content including prompt box) ---

    if has_claude_blocked_prompt(content, &lower) {
        return AgentState::Blocked;
    }

    // --- Working detection (content above the prompt box) ---

    if has_claude_working_chrome(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_codex(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Blocked patterns
    if lower.contains("press enter to confirm or esc to cancel")
        || lower.contains("enter to submit answer")
        || lower.contains("enter to submit all")
        || lower.contains("allow command?")
        || lower.contains("[y/n]")
        || lower.contains("yes (y)")
    {
        return AgentState::Blocked;
    }
    if has_confirmation_prompt(&lower) {
        return AgentState::Blocked;
    }

    // Working
    if has_interrupt_pattern(&lower) || has_codex_working_header(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_gemini(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Blocked — explicit confirmation
    if lower.contains("waiting for user confirmation") {
        return AgentState::Blocked;
    }

    // Blocked — box-drawing confirmation prompts
    if content.contains("│ Apply this change")
        || content.contains("│ Allow execution")
        || content.contains("│ Do you want to proceed")
    {
        return AgentState::Blocked;
    }
    if has_confirmation_prompt(&lower) {
        return AgentState::Blocked;
    }

    // Working
    if lower.contains("esc to cancel") {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_cursor(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Blocked
    if has_cursor_blocked_prompt(content, &lower) {
        return AgentState::Blocked;
    }

    // Working
    if lower.contains("ctrl+c to stop") {
        return AgentState::Working;
    }
    if has_cursor_spinner(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_antigravity(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    let has_permission_request = lower.contains("requesting permission for:");
    let has_permission_question = lower.contains("do you want to proceed?");
    let has_permission_controls = lower.contains("tab amend") && lower.contains("edit command");
    if has_permission_request && (has_permission_question || has_permission_controls) {
        return AgentState::Blocked;
    }

    if has_antigravity_spinner(content) || has_antigravity_background_tasks(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_cline(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Blocked
    if lower.contains("let cline use this tool") {
        return AgentState::Blocked;
    }
    // [act mode] or [plan mode] followed by "yes"
    if (lower.contains("[act mode]") || lower.contains("[plan mode]")) && lower.contains("yes") {
        return AgentState::Blocked;
    }

    // Idle
    if lower.contains("cline is ready for your message") {
        return AgentState::Idle;
    }

    // Cline defaults to working (unlike most agents that default to idle)
    AgentState::Working
}

fn detect_opencode(content: &str) -> AgentState {
    // Blocked
    if content.contains("△ Permission required") || has_opencode_question_prompt(content) {
        return AgentState::Blocked;
    }

    // Working
    if has_interrupt_pattern(&content.to_lowercase()) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_github_copilot(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Blocked
    if lower.contains("esc to cancel")
        && (lower.contains("enter to select")
            || lower.contains("enter to confirm")
            || lower.contains("enter to submit"))
    {
        return AgentState::Blocked;
    }

    // Working
    if lower.contains("esc to cancel")
        || lower.contains("esc cancel")
        || lower.contains("esc again to cancel")
    {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn detect_kimi(content: &str) -> AgentState {
    if has_kimi_blocked_prompt(content) {
        return AgentState::Blocked;
    }

    if has_kimi_working_status(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn has_kimi_blocked_prompt(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("requesting approval")
        && (lower.contains("approve once") || lower.contains("approve for this session"))
        && lower.contains("reject")
        && (lower.contains("1/2/3/4 choose") || lower.contains("↵ confirm"))
}

fn has_kimi_working_status(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim();
        if matches!(
            trimmed,
            "🌕" | "🌖" | "🌗" | "🌘" | "🌑" | "🌒" | "🌓" | "🌔"
        ) {
            return true;
        }

        let mut chars = trimmed.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !('\u{2800}'..='\u{28FF}').contains(&first) {
            return false;
        }

        let rest = chars
            .as_str()
            .trim_start_matches(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
            .trim_start()
            .to_lowercase();
        rest.starts_with("thinking...") || rest.starts_with("using ")
    })
}

/// Kiro CLI detection.
///
/// Kiro exposes reliable working and idle terminal markers. Tool approval
/// prompts render with a stable "requires approval" line and an action menu.
fn detect_kiro(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    let has_approval_request = lower.contains("requires approval");
    let has_approval_actions = lower.contains("yes, single permission")
        || lower.contains("trust, always allow")
        || lower.contains("no (tab to edit)")
        || lower.contains("esc to close");
    if has_approval_request && has_approval_actions {
        return AgentState::Blocked;
    }

    if lower.contains("kiro is working")
        || (lower.contains("esc to cancel") && has_kiro_tool_spinner(content))
    {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Droid detection.
///
/// Working: braille spinner line (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏) + "Thinking..." + "(Press ESC to stop)"
/// Blocked: EXECUTE prompt with selection box ("Yes, allow" / "No, cancel") +
///          "Use ↑↓ to navigate, Enter to select"
/// Idle: prompt box visible, no spinner, no selection prompt
fn detect_droid(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Blocked: EXECUTE approval prompt with selection UI chrome
    // Primary (AND): structural keyword + chrome text = certain
    let has_execute = content.contains("EXECUTE");
    let has_selection_chrome = lower.contains("enter to select")
        || lower.contains("↑↓ to navigate")
        || lower.contains("esc to cancel");
    let has_selection_options = lower.contains("> yes, allow") || lower.contains("> no, cancel");

    if has_execute && (has_selection_chrome || has_selection_options) {
        return AgentState::Blocked;
    }
    // Secondary: selection chrome + options together (no EXECUTE needed)
    if has_selection_chrome && has_selection_options {
        return AgentState::Blocked;
    }

    // Working: braille spinner character at start of a line + "Thinking..."
    // The braille chars (⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏) are very specific — won't appear in normal content
    if has_braille_spinner(content) && lower.contains("esc to stop") {
        return AgentState::Working;
    }
    // Fallback: "ESC to stop" alone is still a strong signal (it's UI chrome)
    if lower.contains("esc to stop") {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Amp (Sourcegraph) detection.
///
/// Blocked approval prompts use a shared footer with options like
/// "Approve", "Allow All for This Session", "Allow All for Every Session",
/// "Allow File for Every Session", and "Deny with feedback". The header varies
/// by approval type, for example "Invoke tool ...?", "Run this command?",
/// "Allow editing file:", or "Allow creating file:".
///
/// Working layout:
/// ```text
///   ✓ Search Map the core runtime architecture...
///   ⋯ Oracle ▼
///   ≈ Running tools...         Esc to cancel
/// ```
fn detect_amp(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    let has_waiting_for_approval = lower.contains("waiting for approval");
    let has_approval_header = lower.contains("invoke tool")
        || lower.contains("run this command?")
        || lower.contains("allow editing file:")
        || lower.contains("allow creating file:")
        || lower.contains("confirm tool call");
    let has_approval_actions = lower.contains("approve")
        && (lower.contains("allow all for this session")
            || lower.contains("allow all for every session")
            || lower.contains("allow file for every session")
            || lower.contains("deny with feedback"));

    if has_approval_actions && (has_waiting_for_approval || has_approval_header) {
        return AgentState::Blocked;
    }

    if lower.contains("esc to cancel") {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Grok Build detection.
///
/// Blocked permission prompts display a whitelist scope selector with choices
/// like "Yes, proceed" and "No, reject". Working turns show a braille spinner
/// status line such as "⠋ Waiting… 1.8s" plus live controls like
/// "Ctrl+c:cancel" and "Ctrl+Enter:interject".
fn detect_grok(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    if lower.contains("use ← → to choose permission whitelist scope")
        || lower.contains("yes, proceed")
        || lower.contains("no, reject")
        || lower.contains("ctrl+o:yolo")
        || lower.contains(":scope")
    {
        return AgentState::Blocked;
    }

    if has_braille_spinner(content)
        && (lower.contains("waiting")
            || lower.contains("run ")
            || lower.contains("read ")
            || lower.contains("search ")
            || lower.contains("list "))
    {
        return AgentState::Working;
    }

    if lower.contains("ctrl+c:cancel") && lower.contains("ctrl+enter:interject") {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Hermes Agent detection.
///
/// Hermes shows a bottom status bar while turns are active and modal approval
/// dialogs for dangerous terminal commands. Prefer the modal controls for
/// blocked detection, then the live interrupt/status controls for working.
fn detect_hermes(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    let has_approval_options = lower.contains("allow once")
        && lower.contains("allow for this session")
        && lower.contains("deny");
    let has_approval_controls = lower.contains("enter to confirm")
        || lower.contains("↑/↓ to select")
        || lower.contains("show full command");
    if (lower.contains("dangerous command") || has_approval_options) && has_approval_controls {
        return AgentState::Blocked;
    }

    if lower.contains("msg=interrupt") || lower.contains("ctrl+c cancel") {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Check for braille spinner characters at the start of a line.
/// These are the Unicode braille pattern dots used by CLI spinners.
fn has_braille_spinner(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(c) = trimmed.chars().next() {
            if ('\u{2800}'..='\u{28FF}').contains(&c) {
                return true;
            }
        }
    }
    false
}

fn has_kiro_tool_spinner(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !matches!(first, '◔' | '◑' | '◕' | '●') {
            return false;
        }
        let rest = chars.as_str().trim_start();
        rest.chars().next().is_some_and(char::is_alphabetic)
    })
}

/// Qodercli detection.
///
/// Qodercli is a Node.js coding-agent CLI. It surfaces a confirmation prompt
/// while awaiting tool approval and a braille spinner while working.
fn detect_qodercli(content: &str) -> AgentState {
    let lower = content.to_lowercase();

    // Idle short-circuit: double-press confirmation hints render *over* the
    // input prompt while the user briefly holds Ctrl+C / Esc. The pane is
    // effectively idle there — without this, a stale spinner row above could
    // still flip it to Working.
    if has_qodercli_idle_override(&lower) {
        return AgentState::Idle;
    }

    if has_qodercli_blocked_prompt(&lower) {
        return AgentState::Blocked;
    }

    // Working: explicit "(esc to cancel, …)" hint or an active spinner row.
    if has_qodercli_working_hint(&lower) || has_qodercli_spinner_row(content) {
        return AgentState::Working;
    }

    AgentState::Idle
}

/// Idle override hints. Mirrors the `⌕ Search…` / `ctrl+r to toggle` shortcut
/// in [`detect_claude`]: when these UI bits are visible the pane is sitting at
/// a static prompt and should not be classified as Working or Blocked.
///
/// Covers qodercli's "press again" exit/rewind banners.
fn has_qodercli_idle_override(lower_content: &str) -> bool {
    lower_content.contains("press ctrl+c again to exit")
        || lower_content.contains("press ctrl+d again to exit")
        || lower_content.contains("press esc again to rewind")
}

/// Working hints qodercli prints alongside the spinner while the model is
/// responding. The "(esc to cancel, …)" suffix is unique to qodercli's loading
/// indicator and survives even when the spinner glyph is masked (e.g. by
/// a hook icon).
fn has_qodercli_working_hint(lower_content: &str) -> bool {
    lower_content.contains("(esc to cancel,")
}

/// Strict spinner-row detection for qodercli.
///
/// Matches a line whose first non-whitespace glyph is a braille pattern
/// (U+2800–U+28FF, the cli-spinners "dots" set qodercli renders), followed by
/// a space and at least one alphabetic character on the same line. This avoids
/// flagging the pane as Working when the scrollback merely contains a stale
/// braille glyph from an earlier frame.
fn has_qodercli_spinner_row(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        let Some(first) = chars.next() else {
            continue;
        };
        if !('\u{2800}'..='\u{28FF}').contains(&first) {
            continue;
        }
        let rest: String = chars.collect();
        if rest.starts_with(' ') && rest.chars().any(|c| c.is_alphabetic()) {
            return true;
        }
    }
    false
}

/// Blocked patterns specific to qodercli.
///
/// Mirrors the helper structure used by [`has_claude_blocked_prompt`] so the
/// pattern surface stays a single, easy-to-extend list.
///
/// Covered states:
/// * Tool-call confirmation banners ("Waiting for user confirmation",
///   "Awaiting approval").
/// * The "Permission Required / Allow once or always?" approval dialog.
/// * The `ask-user` tool's interactive prompt. "Asking User" is the dialog's
///   stable BaseTabDialog title and covers every form (single-select,
///   multi-select, free-form input, review tab). The "Enter your response"
///   placeholder and "Review your answers:" review heading are kept as
///   defensive fallbacks in case the title row scrolls off-screen.
/// * The interactive shell waiting hint emitted by qodercli when an agent
///   spawns a shell that is now parked for user keystrokes.
fn has_qodercli_blocked_prompt(lower_content: &str) -> bool {
    lower_content.contains("waiting for user confirmation")
        || lower_content.contains("awaiting approval")
        || lower_content.contains("permission required")
        || lower_content.contains("allow once or always?")
        || lower_content.contains("asking user")
        || lower_content.contains("enter your response")
        || lower_content.contains("review your answers:")
        || lower_content.contains("shell awaiting input")
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Check for "do you want"/"would you like" followed by "yes" or "❯"
fn has_confirmation_prompt(lower_content: &str) -> bool {
    if let Some(pos) = lower_content
        .find("do you want")
        .or_else(|| lower_content.find("would you like"))
    {
        let after = &lower_content[pos..];
        return after.contains("yes") || after.contains('❯');
    }
    false
}

/// Claude uses the same generic Select and Dialog widgets for both
/// permission flows and ordinary slash/settings menus. Match only the
/// permission and interview prompts that actually need user input.
fn has_claude_blocked_prompt(content: &str, lower_content: &str) -> bool {
    has_confirmation_prompt(lower_content)
        || lower_content.contains("do you want to proceed?")
        || lower_content.contains("would you like to proceed?")
        || lower_content.contains("waiting for permission")
        || lower_content.contains("do you want to allow this connection?")
        || lower_content.contains("tab to amend")
        || lower_content.contains("ctrl+e to explain")
        || lower_content.contains("chat about this")
        || lower_content.contains("review your answers")
        || lower_content.contains("skip interview and plan immediately")
        || (has_selection_prompt(content) && has_claude_yes_no_choice(content))
}

fn has_claude_yes_no_choice(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line
            .trim()
            .trim_start_matches('❯')
            .trim_start()
            .to_lowercase();
        trimmed == "yes"
            || trimmed == "no"
            || trimmed.starts_with("1. yes")
            || trimmed.starts_with("2. no")
            || trimmed.starts_with("yes, and ")
            || trimmed.starts_with("no, and tell claude")
    })
}

/// Check for "❯" followed by numbered options like "1."
fn has_selection_prompt(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('❯') {
            // Check if there's a digit followed by a dot nearby
            if trimmed.chars().any(|c| c.is_ascii_digit()) && trimmed.contains('.') {
                return true;
            }
        }
    }
    false
}

/// Check for "esc" + "interrupt" pattern
fn has_interrupt_pattern(lower_content: &str) -> bool {
    lower_content.contains("esc to interrupt")
        || lower_content.contains("ctrl+c to interrupt")
        || (lower_content.contains("esc") && lower_content.contains("interrupt"))
}

/// Claude Code spinner characters + activity label.
/// The verb changes frequently ("Processing…", "Pouncing…", etc.), so rely
/// on the spinner glyph + trailing ellipsis rather than specific wording.
/// Include Claude's narrow-pane middle-dot frame too.
fn has_spinner_activity(content: &str) -> bool {
    const SPINNER_CHARS: &str = "·✱✲✳✴✵✶✷✸✹✺✻✼✽✾✿❀❁❂❃❇❈❉❊❋✢✣✤✥✦✧✨⊛⊕⊙◉◎◍⁂⁕※⍟☼★☆";
    for line in content.lines() {
        let trimmed = line.trim();
        let mut chars = trimmed.chars();
        if let Some(first) = chars.next() {
            if SPINNER_CHARS.contains(first) {
                let rest: String = chars.collect();
                if rest.starts_with(' ')
                    && rest.contains('\u{2026}')
                    && rest.chars().any(|c| c.is_alphanumeric())
                {
                    return true;
                }
            }
        }
    }
    false
}

fn has_visible_blocker(agent: Agent, content: &str, state: AgentState) -> bool {
    if state != AgentState::Blocked {
        return false;
    }

    match agent {
        // Strong visible blockers are opt-in because this flag can override
        // hook authority. Plain blocked heuristics remain valid fallback state,
        // but they must not become hook overrides unless the current UI chrome
        // is known to be structural and live.
        Agent::Claude => has_claude_visible_blocker(content),
        Agent::Codex => has_codex_visible_blocker(content),
        _ => false,
    }
}

fn has_claude_visible_blocker(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("do you want to proceed?")
        && has_claude_yes_no_choice(content)
        && (lower.contains("bash command")
            || lower.contains("bash(")
            || lower.contains("contains expansion")
            || lower.contains("tab to amend")
            || lower.contains("ctrl+e to explain"))
}

fn has_codex_visible_blocker(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("press enter to confirm or esc to cancel")
        || lower.contains("enter to submit answer")
        || lower.contains("enter to submit all")
        || lower.contains("allow command?")
}

fn has_visible_idle(agent: Agent, content: &str, state: AgentState) -> bool {
    if state != AgentState::Idle {
        return false;
    }

    match agent {
        Agent::Claude => has_claude_prompt_box(content),
        Agent::Codex => has_codex_prompt(content),
        _ => false,
    }
}

fn has_visible_working(agent: Agent, content: &str, state: AgentState) -> bool {
    if state != AgentState::Working {
        return false;
    }

    match agent {
        Agent::Claude => has_claude_working_chrome(content),
        Agent::Codex => has_codex_visible_working(content),
        _ => false,
    }
}

fn has_codex_visible_working(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let Some(working_index) = lines.iter().rposition(|line| codex_live_working_line(line)) else {
        return false;
    };

    lines[working_index + 1..].iter().all(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('•')
            && !trimmed.starts_with('■')
            && !trimmed.starts_with('✗')
            && !trimmed.starts_with('✓')
    })
}

fn has_codex_working_header(content: &str) -> bool {
    content.lines().any(codex_working_status_line)
}

fn codex_live_working_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let lower = trimmed.to_lowercase();
    codex_working_status_line(line)
        && (trimmed.contains("Waiting for background terminal")
            || lower.contains("esc to interrupt")
            || lower.contains("esc…"))
}

fn codex_working_status_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('•')
        && (trimmed.contains("Working (") || trimmed.contains("Waiting for background terminal ("))
}

fn has_codex_prompt(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed == "›" || trimmed.starts_with("› ")
    })
}

fn has_cursor_blocked_prompt(content: &str, lower: &str) -> bool {
    if lower.contains("waiting for approval") || lower.contains("run this command?") {
        return true;
    }

    if lower.contains("(y) (enter)")
        || lower.contains("keep (n)")
        || lower.contains("skip (esc or n)")
    {
        return true;
    }

    content.lines().any(|line| {
        let line = line.trim().to_lowercase();
        let has_yes_action = line.contains("(y)");
        has_yes_action
            && (line.contains("allow")
                || line.contains("run (once)")
                || line.contains("→ run")
                || line.starts_with("run "))
    })
}

/// Cursor status line: spinner glyphs followed by a live action label.
fn has_cursor_spinner(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        let rest = chars.as_str().trim_start();

        if matches!(first, '⬡' | '⬢') {
            return cursor_status_word_is_active(rest);
        }

        if ('\u{2800}'..='\u{28FF}').contains(&first) {
            let rest = rest.trim_start_matches(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
            return cursor_status_word_is_active(rest.trim_start());
        }

        false
    })
}

fn cursor_status_word_is_active(rest: &str) -> bool {
    let Some(word) = rest.split_whitespace().next() else {
        return false;
    };
    word.trim_end_matches(|c: char| !c.is_alphabetic())
        .to_ascii_lowercase()
        .ends_with("ing")
}

fn has_antigravity_spinner(content: &str) -> bool {
    content.lines().any(|line| {
        let trimmed = line.trim_start();
        let mut chars = trimmed.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !('\u{2800}'..='\u{28FF}').contains(&first) {
            return false;
        }

        let rest = chars
            .as_str()
            .trim_start_matches(|c| ('\u{2800}'..='\u{28FF}').contains(&c))
            .trim_start();
        cursor_status_word_is_active(rest)
    })
}

fn has_antigravity_background_tasks(content: &str) -> bool {
    let bottom_lines: Vec<&str> = content
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(5)
        .collect();

    bottom_lines.into_iter().any(|line| {
        let line = line.trim().to_lowercase();
        line.contains("/tasks") && antigravity_task_count(&line).is_some_and(|count| count > 0)
    })
}

fn antigravity_task_count(line: &str) -> Option<u32> {
    for marker in [" task(s)", " tasks", " task"] {
        let Some((before, _)) = line.split_once(marker) else {
            continue;
        };
        let raw_count = before.split_whitespace().last()?.trim_matches(|c| c == '·');
        if let Ok(count) = raw_count.parse() {
            return Some(count);
        }
    }
    None
}

fn has_opencode_question_prompt(content: &str) -> bool {
    let lower = content.to_lowercase();
    let has_enter_action = lower.contains("enter confirm")
        || lower.contains("enter submit")
        || lower.contains("enter toggle");
    let has_question_nav = content.contains("↑↓ select") || content.contains("⇆ tab");

    lower.contains("esc dismiss") && has_enter_action && has_question_nav
}

fn has_claude_working_chrome(content: &str) -> bool {
    let above = content_above_prompt_box(content);
    let above_lower = above.to_lowercase();
    above_lower.contains("esc to interrupt")
        || above_lower.contains("ctrl+c to interrupt")
        || has_spinner_activity(above)
}

/// Extract content above Claude's prompt box.
/// The prompt box is two ─── border lines with ❯ between them.
fn content_above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();

    if let Some(i) = claude_prompt_box_top_border_index(&lines) {
        let byte_offset: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        return &content[..byte_offset.min(content.len())];
    }

    // No prompt box found, return all content
    content
}

fn has_claude_prompt_box(content: &str) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top_border_index) = claude_prompt_box_top_border_index(&lines) else {
        return false;
    };

    lines[top_border_index + 1..]
        .iter()
        .take_while(|line| !is_horizontal_rule(line))
        .any(|line| line.trim_start().starts_with('❯'))
}

fn claude_prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;

    for i in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[i]) {
            border_count += 1;
            if border_count == 2 {
                return Some(i);
            }
        }
    }

    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '─')
}

// ---------------------------------------------------------------------------
// Agent identification from a pane's running command
// ---------------------------------------------------------------------------
//
// herdr identifies agents by inspecting the foreground process group of each
// PTY it owns (`crate::platform`). A Zellij plugin can't read `/proc`, but the
// server hands it the running-command argv directly via the `CommandChanged`
// event and `get_pane_running_command()`. So we keep herdr's argv-parsing
// logic (runtime-wrapper unwrapping, path-token basename mapping) and drop the
// platform process-walking that fed it.

/// Identify which agent is running in a pane from its command argv, as
/// delivered by Zellij's `CommandChanged` event / `get_pane_running_command()`.
///
/// `command[0]` is the program; the rest are its arguments. Returns `None` for
/// plain shells or unrecognized programs.
pub fn identify_agent_from_command(command: &[String]) -> Option<Agent> {
    let argv0 = command.first()?;
    let effective = path_basename(argv0);
    let lower_effective = effective.to_lowercase();

    // A generic runtime/shell may be wrapping an agent script, e.g.
    // `node …/claude.js` or `sh -c 'pi …'`. Unwrap it first.
    if is_generic_runtime_or_shell(&lower_effective) {
        if let Some(name) = wrapped_agent_name_from_runtime_argv(&lower_effective, Some(command)) {
            return parse_agent_label(&name);
        }
    }

    // Direct binary match on the basename (`claude`, `/opt/homebrew/bin/codex`).
    if let Some(agent) = identify_agent(effective) {
        return Some(agent);
    }

    // Fallback: argv0 may still be a path token we can map by basename.
    agent_name_from_path_token(argv0).and_then(|name| parse_agent_label(&name))
}

fn wrapped_agent_name_from_runtime_argv(runtime: &str, argv: Option<&[String]>) -> Option<String> {
    let argv = argv?;
    let runtime = path_basename(runtime).to_lowercase();

    match runtime.as_str() {
        "node" | "bun" => script_arg_agent_name(argv, &["-e", "--eval", "-p", "--print"], &[]),
        "python" | "python3" => script_arg_agent_name(argv, &["-c"], &["-m"]),
        "sh" | "bash" | "zsh" | "fish" => script_arg_agent_name(argv, &["-c"], &[]),
        "tmux" => None,
        _ => None,
    }
}

fn script_arg_agent_name(
    argv: &[String],
    eval_flags: &[&str],
    module_flags: &[&str],
) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args
                .next()
                .and_then(|token| agent_name_from_path_token(token));
        }

        if flag_matches(arg, eval_flags) || flag_matches(arg, module_flags) {
            return None;
        }

        if arg.starts_with('-') {
            if option_takes_value(arg) {
                let _ = args.next();
            }
            continue;
        }

        return agent_name_from_path_token(arg);
    }

    None
}

fn flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags
        .iter()
        .any(|flag| arg == *flag || short_flag_payload(arg, flag) || long_flag_value(arg, flag))
}

fn short_flag_payload(arg: &str, flag: &str) -> bool {
    flag.starts_with('-')
        && !flag.starts_with("--")
        && arg.starts_with(flag)
        && arg.len() > flag.len()
}

fn long_flag_value(arg: &str, flag: &str) -> bool {
    flag.starts_with("--")
        && arg
            .strip_prefix(flag)
            .is_some_and(|rest| rest.starts_with('='))
}

fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-r" | "--require"
            | "--loader"
            | "--import"
            | "--experimental-loader"
            | "--inspect-port"
            | "-W"
            | "-X"
            | "-S"
            | "-L"
            | "-o"
    )
}

fn agent_name_from_path_token(token: &str) -> Option<String> {
    let trimmed = token.trim_matches(|c| matches!(c, '"' | '\''));
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }

    // herdr also canonicalizes the path here to resolve symlinked launchers
    // (e.g. nix store wrappers). A wasm plugin has no filesystem access, so we
    // map by basename only — the common case the server already resolves.
    agent_name_from_basename(path_basename(trimmed))
}

fn agent_name_from_basename(basename: &str) -> Option<String> {
    let agent = parse_agent_label(basename)?;
    Some(agent_label(agent).to_string())
}

fn path_basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn is_generic_runtime_or_shell(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash" | "zsh" | "fish" | "tmux" | "node" | "bun" | "python" | "python3"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Agent identification ----

    #[test]
    fn identify_known_agents() {
        assert_eq!(identify_agent("pi"), Some(Agent::Pi));
        assert_eq!(identify_agent("claude"), Some(Agent::Claude));
        assert_eq!(identify_agent("claude-code"), Some(Agent::Claude));
        assert_eq!(identify_agent("codex"), Some(Agent::Codex));
        assert_eq!(identify_agent("gemini"), Some(Agent::Gemini));
        assert_eq!(identify_agent("cursor"), Some(Agent::Cursor));
        assert_eq!(identify_agent("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(identify_agent("agy"), Some(Agent::Antigravity));
        assert_eq!(identify_agent("antigravity-cli"), Some(Agent::Antigravity));
        assert_eq!(identify_agent("cline"), Some(Agent::Cline));
        assert_eq!(identify_agent("opencode"), Some(Agent::OpenCode));
        assert_eq!(identify_agent("kimi"), Some(Agent::Kimi));
        assert_eq!(identify_agent("Kimi Code"), Some(Agent::Kimi));
        assert_eq!(identify_agent("kiro"), Some(Agent::Kiro));
        assert_eq!(identify_agent("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(identify_agent("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(identify_agent("ghcs"), Some(Agent::GithubCopilot));
        assert_eq!(identify_agent("grok"), Some(Agent::Grok));
        assert_eq!(identify_agent("grok-build"), Some(Agent::Grok));
        assert_eq!(identify_agent("hermes"), Some(Agent::Hermes));
        assert_eq!(identify_agent("hermes-agent"), Some(Agent::Hermes));
    }

    #[test]
    fn parse_known_agent_labels() {
        assert_eq!(parse_agent_label("pi"), Some(Agent::Pi));
        assert_eq!(parse_agent_label("claude"), Some(Agent::Claude));
        assert_eq!(parse_agent_label("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(parse_agent_label("agy"), Some(Agent::Antigravity));
        assert_eq!(parse_agent_label("antigravity"), Some(Agent::Antigravity));
        assert_eq!(parse_agent_label("copilot"), Some(Agent::GithubCopilot));
        assert_eq!(parse_agent_label("kimi-code"), Some(Agent::Kimi));
        assert_eq!(
            parse_agent_label("github-copilot"),
            Some(Agent::GithubCopilot)
        );
        assert_eq!(parse_agent_label("amp-local"), Some(Agent::Amp));
        assert_eq!(parse_agent_label("kiro-cli"), Some(Agent::Kiro));
        assert_eq!(parse_agent_label("grok-build"), Some(Agent::Grok));
        assert_eq!(parse_agent_label("hermes-agent"), Some(Agent::Hermes));
    }

    #[test]
    fn agent_labels_use_display_names() {
        assert_eq!(agent_label(Agent::Pi), "pi");
        assert_eq!(agent_label(Agent::GithubCopilot), "copilot");
        assert_eq!(agent_label(Agent::OpenCode), "opencode");
        assert_eq!(agent_label(Agent::Antigravity), "agy");
        assert_eq!(agent_label(Agent::Kiro), "kiro");
        assert_eq!(agent_label(Agent::Grok), "grok");
        assert_eq!(agent_label(Agent::Hermes), "hermes");
    }

    #[test]
    fn identify_unknown_processes() {
        assert_eq!(identify_agent("bash"), None);
        assert_eq!(identify_agent("zsh"), None);
        assert_eq!(identify_agent("vim"), None);
        assert_eq!(identify_agent("node"), None);
    }

    #[test]
    fn identify_case_insensitive() {
        assert_eq!(identify_agent("Pi"), Some(Agent::Pi));
        assert_eq!(identify_agent("CLAUDE"), Some(Agent::Claude));
        assert_eq!(identify_agent("Codex"), Some(Agent::Codex));
    }

    // ---- Agent identification from command argv ----

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn command_identifies_direct_binary() {
        assert_eq!(
            identify_agent_from_command(&cmd(&["claude"])),
            Some(Agent::Claude)
        );
        assert_eq!(
            identify_agent_from_command(&cmd(&["/opt/homebrew/bin/codex", "--model", "gpt-5"])),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn command_identifies_node_wrapped_codex() {
        assert_eq!(
            identify_agent_from_command(&cmd(&["node", "/path/to/bin/codex"])),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn command_identifies_shell_wrapped_pi() {
        assert_eq!(
            identify_agent_from_command(&cmd(&["/bin/sh", "/tmp/test-bin/pi"])),
            Some(Agent::Pi)
        );
    }

    #[test]
    fn command_resolves_nix_wrapped_codex_path_argv0() {
        assert_eq!(
            identify_agent_from_command(&cmd(&[
                "/etc/profiles/per-user/user/bin/codex",
                "--model",
                "gpt-5"
            ])),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn command_ignores_python_c_argument_named_codex() {
        assert_eq!(
            identify_agent_from_command(&cmd(&[
                "python3",
                "-c",
                "import time; time.sleep(60)",
                "/tmp/codex"
            ])),
            None
        );
    }

    #[test]
    fn command_ignores_node_eval_argument_named_codex() {
        assert_eq!(
            identify_agent_from_command(&cmd(&[
                "node",
                "-e",
                "setTimeout(() => {}, 60000)",
                "/tmp/codex"
            ])),
            None
        );
    }

    #[test]
    fn command_ignores_shell_c_argument_named_codex() {
        assert_eq!(
            identify_agent_from_command(&cmd(&["bash", "-c", "sleep 60", "/tmp/codex"])),
            None
        );
    }

    #[test]
    fn command_detects_python_script_named_codex() {
        assert_eq!(
            identify_agent_from_command(&cmd(&["python3", "/tmp/codex", "--model", "gpt-5"])),
            Some(Agent::Codex)
        );
    }

    #[test]
    fn command_ignores_unrecognized_program() {
        assert_eq!(identify_agent_from_command(&cmd(&["bash"])), None);
        assert_eq!(identify_agent_from_command(&cmd(&["vim", "file.rs"])), None);
        assert_eq!(identify_agent_from_command(&cmd(&[])), None);
    }

    #[test]
    fn wrapped_agent_name_from_runtime_argv_ignores_plain_shell_flags() {
        assert_eq!(
            wrapped_agent_name_from_runtime_argv("bash", Some(&["bash".into(), "-lc".into()])),
            None
        );
    }

    // ---- Workspace state rollup ----

    // ---- No agent → Unknown ----

    #[test]
    fn no_agent_returns_unknown() {
        assert_eq!(detect_state(None, "anything"), AgentState::Unknown);
    }

    // ---- Pi ----

    #[test]
    fn pi_working_when_working() {
        assert_eq!(detect_pi("some output\nWorking..."), AgentState::Working);
    }

    #[test]
    fn pi_working_working_in_middle() {
        assert_eq!(detect_pi("line1\nWorking...\nline3"), AgentState::Working);
    }

    #[test]
    fn pi_idle_at_prompt() {
        assert_eq!(detect_pi("❯ "), AgentState::Idle);
    }

    #[test]
    fn pi_idle_no_working_text() {
        assert_eq!(detect_pi("some output\n\n> ready"), AgentState::Idle);
    }

    // ---- Claude Code ----

    #[test]
    fn claude_working_esc_to_interrupt() {
        let screen = "Reading file src/main.rs\nesc to interrupt\n─────────\n❯ \n─────────";
        assert_eq!(detect_claude(screen), AgentState::Working);
    }

    #[test]
    fn claude_working_ctrl_c_to_interrupt() {
        let screen = "Editing code\nctrl+c to interrupt\n─────────\n❯ \n─────────";
        assert_eq!(detect_claude(screen), AgentState::Working);
    }

    #[test]
    fn claude_working_spinner() {
        let screen = "✽ Tempering…\n─────────\n❯ \n─────────";
        assert_eq!(detect_claude(screen), AgentState::Working);
    }

    #[test]
    fn claude_working_middle_dot_spinner() {
        let screen = "· Thinking…\n─────────\n❯ \n─────────";
        assert_eq!(detect_claude(screen), AgentState::Working);
    }

    #[test]
    fn claude_working_spinner_with_detail() {
        let screen = "✳ Simplifying recompute_tangents…\n─────────\n❯ \n─────────";
        assert_eq!(detect_claude(screen), AgentState::Working);
    }

    #[test]
    fn claude_waiting_do_you_want() {
        let screen = "Do you want to run this command?\n\nYes  No";
        assert_eq!(detect_claude(screen), AgentState::Blocked);
    }

    #[test]
    fn claude_waiting_would_you_like() {
        let screen = "Would you like to apply these changes?\n\n❯ Yes";
        assert_eq!(detect_claude(screen), AgentState::Blocked);
    }

    #[test]
    fn claude_waiting_selection_prompt() {
        let screen = "Do you want to proceed?\n❯ 1. Yes\n  2. No\n\nEsc to cancel · Tab to amend";
        assert_eq!(detect_claude(screen), AgentState::Blocked);
    }

    #[test]
    fn claude_waiting_esc_to_cancel() {
        let screen = "Allow bash: rm -rf /tmp/test?\n\nDo you want to proceed?\n\nesc to cancel";
        assert_eq!(detect_claude(screen), AgentState::Blocked);
    }

    #[test]
    fn claude_bash_permission_modal_is_visible_blocker() {
        let screen = "● Bash(mkdir -p /tmp/herdr-claude-detector-test && for i in 1 2 3; do dd if=/dev/urandom)\n  ⎿  Waiting…\n\n────────────────────────\n Bash command\n\n   mkdir -p /tmp/herdr-claude-detector-test && ls -la /tmp/herdr-claude-detector-test\n   Create random files in temporary detector directory\n\n Contains expansion\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel · Tab to amend · ctrl+e to explain";
        let detection = detect_agent(Some(Agent::Claude), screen);

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn claude_cropped_bash_permission_modal_is_visible_blocker() {
        let screen = "● Bash(mkdir -p /tmp/herdr-claude-detector-test && ls -la /tmp/herdr-claude-detector-test)\n  ⎿  Waiting…\n\nDo you want to proceed?\n❯ 1. Yes\n  2. No";
        let detection = detect_agent(Some(Agent::Claude), screen);

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    #[test]
    fn claude_waiting_ask_user_question_menu() {
        let screen =
            "Which approach should I take?\n❯ 1. Minimal change\n  2. Bigger refactor\n3. Chat about this\n\nEnter to select · Tab/Arrow keys to navigate · Esc to cancel";
        assert_eq!(detect_claude(screen), AgentState::Blocked);
    }

    #[test]
    fn claude_idle_hooks_menu() {
        let screen = "Hooks\n0 hooks configured\nℹ This menu is read-only. To add or modify hooks, edit settings.json directly or ask Claude. Learn more\n\n❯ 1. PreToolUse\n  2. PostToolUse\n  3. PostToolUseFailure\n\nEnter to confirm · Esc to cancel";
        assert_eq!(detect_claude(screen), AgentState::Idle);
    }

    #[test]
    fn claude_idle_theme_menu() {
        let screen = "Theme\nChoose the text style that looks best with your terminal\n\n❯ 1. Dark mode ✔\n  2. Light mode\n  3. Dark mode (colorblind-friendly)\n\nSyntax theme: Monokai Extended (ctrl+t to disable)\n\nEnter to select · Esc to cancel";
        assert_eq!(detect_claude(screen), AgentState::Idle);
    }

    #[test]
    fn claude_idle_prompt_box() {
        let screen = "Task complete.\n─────────────\n❯ \n─────────────";
        assert_eq!(detect_claude(screen), AgentState::Idle);
    }

    #[test]
    fn claude_prompt_box_is_visible_idle() {
        let screen = "Interrupted.\n─────────────\n❯ \n─────────────";
        let detection = detect_agent(Some(Agent::Claude), screen);

        assert_eq!(detection.state, AgentState::Idle);
        assert!(detection.visible_idle);
    }

    #[test]
    fn claude_separators_without_prompt_are_not_visible_idle() {
        let screen = "Task complete.\n─────────────\nplain text\n─────────────";
        let detection = detect_agent(Some(Agent::Claude), screen);

        assert_eq!(detection.state, AgentState::Idle);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn claude_spinner_above_prompt_box_is_working() {
        let screen = "✢ Imagining… (3s · thinking with high effort)\n  ⎿  Tip: Run /terminal-setup\n\n─────────────\n❯ \n─────────────\n~/project";
        let detection = detect_agent(Some(Agent::Claude), screen);

        assert_eq!(detection.state, AgentState::Working);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn claude_idle_search() {
        let screen = "⌕ Search…\nsome content";
        assert_eq!(detect_claude(screen), AgentState::Idle);
    }

    #[test]
    fn claude_working_not_confused_by_old_prompt() {
        // The "esc to interrupt" is ABOVE the prompt box — should be working
        let screen = "✽ Writing…\nesc to interrupt\n──────\n❯ \n──────";
        assert_eq!(detect_claude(screen), AgentState::Working);
    }

    // ---- Codex ----

    #[test]
    fn codex_waiting_confirm() {
        assert_eq!(
            detect_codex("press enter to confirm or esc to cancel"),
            AgentState::Blocked
        );
    }

    #[test]
    fn codex_waiting_allow_command() {
        assert_eq!(detect_codex("allow command?\n[y/n]"), AgentState::Blocked);
    }

    #[test]
    fn codex_waiting_submit_answer() {
        assert_eq!(
            detect_codex("Question about approach\n| enter to submit answer"),
            AgentState::Blocked
        );
    }

    #[test]
    fn codex_question_ui_is_visible_blocker() {
        let screen = "Question 1/1 (1 unanswered)\nWhat kind of code improvement do you want?\n› 1. Reduce complexity\n  2. Improve reliability\n\ntab to add notes | enter to submit answer | esc to interrupt";
        let detection = detect_agent(Some(Agent::Codex), screen);

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    #[test]
    fn codex_bare_yes_no_hint_is_not_visible_blocker() {
        let detection = detect_agent(Some(Agent::Codex), "The docs mention [y/n] prompts.");

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(!detection.visible_blocker);
    }

    #[test]
    fn codex_generic_confirmation_prompt_is_not_visible_blocker() {
        let detection = detect_agent(
            Some(Agent::Codex),
            "Earlier output asked: do you want to continue? The answer was yes.",
        );

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(!detection.visible_blocker);
    }

    #[test]
    fn non_codex_blocked_heuristics_are_not_strong_visible_blockers_by_default() {
        let detection = detect_agent(Some(Agent::Gemini), "Do you want to proceed?\n\nYes  No");

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(!detection.visible_blocker);
    }

    #[test]
    fn codex_waiting_submit_answer_wrapped_footer() {
        assert_eq!(
            detect_codex(
                "Question 1/2\nChoose an option.\nenter to submit answer\nesc to interrupt"
            ),
            AgentState::Blocked
        );
    }

    #[test]
    fn codex_waiting_submit_all_multi_question_footer() {
        let screen = "Question 2/2 (1 unanswered)\nAt a high level, what is issue 249 supposed to fix?\n› 1. State arbitration (Recommended)\n  2. UI behavior\n  3. Test reliability\n  4. None of the above\n\ntab to add notes | enter to submit all | ←/→ to navigate questions | esc to interrupt";
        let detection = detect_agent(Some(Agent::Codex), screen);

        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn codex_interrupted_prompt_is_visible_idle() {
        let screen = "■ Conversation interrupted - tell the model what to do differently. Something went\nwrong? Hit `/feedback` to report the issue.\n\n\n› Run /review on my current changes\n\n  gpt-5.5 high · ~/Projects/herdr-worktrees/issue-249-state-arbitration";
        let detection = detect_agent(Some(Agent::Codex), screen);

        assert_eq!(detection.state, AgentState::Idle);
        assert!(detection.visible_idle);
    }

    #[test]
    fn codex_working_interrupt() {
        assert_eq!(
            detect_codex("generating code\nesc to interrupt"),
            AgentState::Working
        );
    }

    #[test]
    fn codex_working_truncated_status_header() {
        assert_eq!(detect_codex("• Working (0s • esc…"), AgentState::Working);
    }

    #[test]
    fn codex_status_line_is_visible_working() {
        let detection = detect_agent(
            Some(Agent::Codex),
            "• Ran git status --short\n  └ M src/detect.rs\n\n• Working (17s • esc to interrupt)\n\n\n› Implement {feature}",
        );

        assert_eq!(detection.state, AgentState::Working);
        assert!(detection.visible_working);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn codex_background_terminal_wait_is_visible_working() {
        let detection = detect_agent(
            Some(Agent::Codex),
            "• Waiting for background terminal (0s • esc to …\n  └ cargo test -p codex-core -- --exact…\n\n\n› Ask Codex to do anything",
        );

        assert_eq!(detection.state, AgentState::Working);
        assert!(detection.visible_working);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn codex_working_header_without_interrupt_is_not_visible_working() {
        let detection = detect_agent(
            Some(Agent::Codex),
            "• Working (17s)\n\n› Implement {feature}",
        );

        assert_eq!(detection.state, AgentState::Working);
        assert!(!detection.visible_working);
    }

    #[test]
    fn codex_old_working_line_before_later_block_is_not_visible_working() {
        let detection = detect_agent(
            Some(Agent::Codex),
            "• Working (17s • esc to interrupt)\n\n• Ran git status --short\n  └ M src/detect.rs\n\n› Implement {feature}",
        );

        assert_eq!(detection.state, AgentState::Working);
        assert!(!detection.visible_working);
    }

    #[test]
    fn codex_old_background_terminal_wait_before_later_block_is_not_visible_working() {
        let detection = detect_agent(
            Some(Agent::Codex),
            "• Waiting for background terminal (0s • esc to …\n  └ cargo test -p codex-core\n\n• Ran git status --short\n  └ M src/detect.rs\n\n› Implement {feature}",
        );

        assert_eq!(detection.state, AgentState::Working);
        assert!(!detection.visible_working);
    }

    #[test]
    fn codex_idle() {
        assert_eq!(detect_codex("❯ "), AgentState::Idle);
    }

    // ---- Gemini ----

    #[test]
    fn gemini_waiting_confirmation() {
        assert_eq!(
            detect_gemini("waiting for user confirmation"),
            AgentState::Blocked
        );
    }

    #[test]
    fn gemini_waiting_apply() {
        assert_eq!(
            detect_gemini("│ Apply this change\n│ Yes  │ No"),
            AgentState::Blocked
        );
    }

    #[test]
    fn gemini_waiting_allow_execution() {
        assert_eq!(
            detect_gemini("│ Allow execution of: rm test.txt"),
            AgentState::Blocked
        );
    }

    #[test]
    fn gemini_working() {
        assert_eq!(
            detect_gemini("thinking...\nesc to cancel"),
            AgentState::Working
        );
    }

    #[test]
    fn gemini_idle() {
        assert_eq!(detect_gemini("❯ "), AgentState::Idle);
    }

    // ---- Cursor ----

    #[test]
    fn cursor_waiting_accept() {
        assert_eq!(
            detect_cursor("Apply changes? (y) (enter) or keep (n)"),
            AgentState::Blocked
        );
    }

    #[test]
    fn cursor_waiting_allow() {
        assert_eq!(detect_cursor("allow file edit (y)"), AgentState::Blocked);
    }

    #[test]
    fn cursor_working_spinner() {
        assert_eq!(detect_cursor("⬡ Grepping.."), AgentState::Working);
    }

    #[test]
    fn cursor_working_braille_status() {
        assert_eq!(
            detect_cursor("⠠⠜ Running  5.52k tokens"),
            AgentState::Working
        );
        assert_eq!(
            detect_cursor("⠞ Working  5.62k tokens"),
            AgentState::Working
        );
        assert_eq!(
            detect_cursor("⠛ Grepping  1.2k tokens"),
            AgentState::Working
        );
    }

    #[test]
    fn cursor_blocked_command_approval() {
        let screen =
            "Waiting for approval...\nRun this command?\n→ Run (once) (y)\nSkip (esc or n)";
        assert_eq!(detect_cursor(screen), AgentState::Blocked);
    }

    #[test]
    fn cursor_running_text_with_unrelated_yes_is_not_blocked() {
        let screen = "previous answer mentioned (y)\n⠠⠜ Running  5.52k tokens";
        assert_eq!(detect_cursor(screen), AgentState::Working);
    }

    #[test]
    fn cursor_working_ctrl_c() {
        assert_eq!(
            detect_cursor("processing\nctrl+c to stop"),
            AgentState::Working
        );
    }

    #[test]
    fn cursor_idle() {
        assert_eq!(detect_cursor("> "), AgentState::Idle);
    }

    // ---- Antigravity ----

    #[test]
    fn antigravity_blocked_permission_prompt() {
        let screen = "Requesting permission for: git log -n 50\nDo you want to proceed?\n> 1. Yes\n↑/↓ Navigate · tab Amend · e edit command";
        assert_eq!(detect_antigravity(screen), AgentState::Blocked);
    }

    #[test]
    fn antigravity_question_without_permission_request_stays_idle() {
        assert_eq!(
            detect_antigravity("Do you want to proceed?\n>"),
            AgentState::Idle
        );
    }

    #[test]
    fn antigravity_working_spinner() {
        assert_eq!(detect_antigravity("⡿ Working..."), AgentState::Working);
        assert_eq!(detect_antigravity("⣯ Loading..."), AgentState::Working);
        assert_eq!(detect_antigravity("⢿ Generating..."), AgentState::Working);
        assert_eq!(detect_antigravity("⣷ Running..."), AgentState::Working);
    }

    #[test]
    fn antigravity_background_task_footer_is_working() {
        let screen = "? for shortcuts     Gemini 3.5 Flash (High) · 1 task(s) · /tasks";
        assert_eq!(detect_antigravity(screen), AgentState::Working);
    }

    #[test]
    fn antigravity_background_task_footer_parses_plural_variants() {
        assert_eq!(
            detect_antigravity("Gemini 3.5 Flash (High) · 10 task(s) · /tasks"),
            AgentState::Working
        );
        assert_eq!(
            detect_antigravity("model · 2 tasks · /tasks"),
            AgentState::Working
        );
        assert_eq!(
            detect_antigravity("model · 1 task · /tasks"),
            AgentState::Working
        );
    }

    #[test]
    fn antigravity_zero_background_tasks_is_idle() {
        let screen = "Gemini 3.5 Flash (High) · 0 task(s) · /tasks";
        assert_eq!(detect_antigravity(screen), AgentState::Idle);
    }

    #[test]
    fn antigravity_task_text_outside_bottom_footer_is_idle() {
        let screen =
            "User said the footer had /tasks and 1 task(s)\nline 1\nline 2\nline 3\nline 4\nline 5";
        assert_eq!(detect_antigravity(screen), AgentState::Idle);
    }

    #[test]
    fn antigravity_idle_prompt() {
        let screen = "Antigravity CLI\n────────────────\n>\n────────────────\n? for shortcuts";
        assert_eq!(detect_antigravity(screen), AgentState::Idle);
    }

    // ---- Cline ----

    #[test]
    fn cline_waiting_tool_use() {
        assert_eq!(detect_cline("let cline use this tool"), AgentState::Blocked);
    }

    #[test]
    fn cline_waiting_act_mode() {
        assert_eq!(
            detect_cline("[act mode] execute command?\nyes"),
            AgentState::Blocked
        );
    }

    #[test]
    fn cline_idle_ready() {
        assert_eq!(
            detect_cline("cline is ready for your message"),
            AgentState::Idle
        );
    }

    #[test]
    fn cline_defaults_to_working() {
        // Cline's default is working (unlike other agents)
        assert_eq!(detect_cline("some random output"), AgentState::Working);
    }

    // ---- OpenCode ----

    #[test]
    fn opencode_waiting_permission() {
        assert_eq!(
            detect_opencode("△ Permission required"),
            AgentState::Blocked
        );
    }

    #[test]
    fn opencode_working() {
        assert_eq!(
            detect_opencode("running tool\nesc to interrupt"),
            AgentState::Working
        );
    }

    #[test]
    fn opencode_waiting_question_prompt() {
        assert_eq!(
            detect_opencode(
                "Goal   Detail   Confirm\n\
                 What do you want help with right now?\n\
                 1. Code change\n\
                 5. Type your own answer\n\
                 ⇆ tab  ↑↓ select  enter confirm  esc dismiss",
            ),
            AgentState::Blocked
        );
    }

    #[test]
    fn opencode_waiting_question_confirm_tab() {
        assert_eq!(
            detect_opencode("Goal   Detail   Confirm\nReview\n⇆ tab  enter submit  esc dismiss"),
            AgentState::Blocked
        );
    }

    #[test]
    fn opencode_idle() {
        assert_eq!(detect_opencode("> "), AgentState::Idle);
    }

    // ---- GitHub Copilot ----

    #[test]
    fn copilot_waiting_fetch_approval() {
        let content = "\
╭───────────────────────────────────────────────────────────────────╮
│ Fetch web content                                                 │
│ ───────────────────────────────────────────────────────────────── │
│ Copilot is attempting to access the following URL:                │
│                                                                   │
│ ╭───────────────────────────────────────────────────────────────╮ │
│ │ https://www.google.com/                                       │ │
│ ╰───────────────────────────────────────────────────────────────╯ │
│                                                                   │
│ Do you want to allow this access?                                 │
│                                                                   │
│ ❯ 1. Yes                                                          │
│   2. No                                                           │
│                                                                   │
│ ↑/↓ to navigate · enter to select · esc to cancel                 │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_allow_directory_access() {
        let content = "\
╭──────────────────────────────────────────────────────────────────╮
│ Allow directory access                                           │
│ ──────────────────────────────────────────────────────────────── │
│ This action may read or write the following path outside your    │
│ allowed directory list.                                          │
│                                                                   │
│ ╭──────────────────────────────────────────────────────────────╮ │
│ │ /Users/user/Dev/workspace                                │ │
│ ╰──────────────────────────────────────────────────────────────╯ │
│                                                                   │
│ Do you want to allow this?                                       │
│                                                                   │
│   1. Yes                                                         │
│ ❯ 2. Yes, and add these directories to the allowed list          │
│   3. No (Esc)                                                    │
│                                                                   │
│ ↑/↓ to navigate · enter to select · esc to cancel                │
╰──────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_directory_permission() {
        let content = "\
○ Asking user Requesting permission to access directory 'src/' for r…

╭───────────────────────────────────────────────────────────────────╮
│ Question                                                          │
│ ───────────────────────────────────────────────────────────────── │
│ Requesting permission to access directory 'src/' for reading      │
│ files. Allow?                                                     │
│                                                                   │
│ ❯ 1. Yes (Allow)                                                  │
│   2. No (Deny)                                                    │
│                                                                   │
│ ↑/↓ to select · enter to confirm · esc to cancel                  │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_action_confirmation() {
        let content = "\
○ Asking user Confirm action: 'Reset local changes in working tree' …

╭───────────────────────────────────────────────────────────────────╮
│ Question                                                          │
│ ───────────────────────────────────────────────────────────────── │
│ Confirm action: 'Reset local changes in working tree' — proceed?  │
│                                                                   │
│ ❯ 1. Yes, reset changes                                           │
│   2. No, cancel                                                   │
│                                                                   │
│ ↑/↓ to select · enter to confirm · esc to cancel                  │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_db_choice() {
        let content = "\
○ Asking user Choose a database for the project:

╭───────────────────────────────────────────────────────────────────╮
│ Question                                                          │
│ ───────────────────────────────────────────────────────────────── │
│ Choose a database for the project:                                │
│                                                                   │
│ ❯ 1. PostgreSQL (Recommended)                                     │
│   2. SQLite                                                       │
│   3. MySQL                                                        │
│   4. MongoDB                                                      │
│                                                                   │
│ ↑/↓ to select · enter to confirm · esc to cancel                  │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_freeform_input() {
        let content = "\
○ Asking user Enter the name for the new branch:

╭───────────────────────────────────────────────────────────────────╮
│ Question                                                          │
│ ───────────────────────────────────────────────────────────────── │
│ Enter the name for the new branch:                                │
│                                                                   │
│ ❯ Type your answer...                                             │
│                                                                   │
│ enter to submit · esc to cancel                                   │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_plan_review() {
        let content = "\
○ Plan ready for review - Create a 200-word inspirational poem. - Fi…

╭───────────────────────────────────────────────────────────────────╮
│ Plan Ready for Review                                             │
│ ───────────────────────────────────────────────────────────────── │
│  - Create a 200-word inspirational poem.                          │
│  - Files/changes: none (deliver poem in chat).                    │
│  - Steps: draft poem, verify ~200 words, offer up to 2 revisions. │
│  - Decision: Inspirational tone, exact length target ~200 words.  │
│                                                                   │
│ ❯ 1. Accept plan and build on default permissions (recommended)   │
│   2. Exit plan mode and I will prompt myself                      │
│   3. Suggest changes                                              │
│                                                                   │
│ ↑/↓ to navigate · enter to select · ctrl+e to show full plan ·    │
│ esc to cancel                                                     │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_waiting_enable_autopilot() {
        let content = "\
╭───────────────────────────────────────────────────────────────────╮
│ Enable autopilot mode                                             │
│ ───────────────────────────────────────────────────────────────── │
│ Autopilot mode works best with all permissions enabled. Without   │
│ them, permission requests will be auto-denied and the agent may   │
│ not complete tasks requiring file edits or shell commands.        │
│                                                                   │
│ You can also enable permissions later with /allow-all             │
│                                                                   │
│ ❯ 1. Enable all permissions (recommended)                         │
│   2. Continue with limited permissions                            │
│   3. Cancel (Esc)                                                 │
│                                                                   │
│ ↑/↓ to navigate · enter to select · esc to cancel                 │
╰───────────────────────────────────────────────────────────────────╯";
        assert_eq!(detect_github_copilot(content), AgentState::Blocked);
    }

    #[test]
    fn copilot_working_thinking_spinner() {
        assert_eq!(
            detect_github_copilot("○ Thinking esc cancel"),
            AgentState::Working
        );
        assert_eq!(
            detect_github_copilot("◎ Thinking esc cancel"),
            AgentState::Working
        );
        assert_eq!(
            detect_github_copilot("◉ Thinking esc cancel"),
            AgentState::Working
        );
    }

    // ---- Kimi ----

    #[test]
    fn kimi_blocked_approval_prompt_wins_over_spinner() {
        let screen = "⠋ Using Shell (git log --oneline -10)\n╭─ approval ─╮\nShell is requesting approval to run command:\ngit log --oneline -10\n→ [1] Approve once\n[2] Approve for this session\n[3] Reject\n[4] Reject, tell the model what to do instead\n▲/▼ select  1/2/3/4 choose  ↵ confirm";
        assert_eq!(detect_kimi(screen), AgentState::Blocked);
    }

    #[test]
    fn kimi_approval_words_without_prompt_stay_idle() {
        assert_eq!(detect_kimi("approve?"), AgentState::Idle);
        assert_eq!(detect_kimi("continue? [y/n]"), AgentState::Idle);
    }

    #[test]
    fn kimi_working_braille_thinking() {
        assert_eq!(
            detect_kimi("⠦ Thinking... <1s · 19 tokens"),
            AgentState::Working
        );
    }

    #[test]
    fn kimi_working_braille_using_tool() {
        assert_eq!(
            detect_kimi("⠹ Using Shell (git log -20 --name-status)"),
            AgentState::Working
        );
    }

    #[test]
    fn kimi_working_moon_spinner() {
        assert_eq!(detect_kimi("🌕"), AgentState::Working);
        assert_eq!(detect_kimi("🌗"), AgentState::Working);
        assert_eq!(detect_kimi("🌘"), AgentState::Working);
    }

    #[test]
    fn kimi_working_moon_spinner_above_input_box() {
        let screen = "✨ yo\n\n🌗\n\n── input ─────────────────────────";
        assert_eq!(detect_kimi(screen), AgentState::Working);
    }

    #[test]
    fn kimi_old_transcript_words_stay_idle() {
        assert_eq!(detect_kimi("thinking"), AgentState::Idle);
        assert_eq!(detect_kimi("generating code"), AgentState::Idle);
        assert_eq!(
            detect_kimi("Used Shell (git log --oneline -10)"),
            AgentState::Idle
        );
        assert_eq!(detect_kimi("some 🌕 in prose"), AgentState::Idle);
    }

    #[test]
    fn kimi_idle() {
        let screen = "Welcome to Kimi Code CLI!\n── input ─\n────────────────\nagent (Kimi-k2.6 ●)  ~/Projects/herdr";
        assert_eq!(detect_kimi(screen), AgentState::Idle);
    }

    // ---- Kiro ----

    #[test]
    fn kiro_working_on_status_bar() {
        let screen = "◕ Shell\n  esc to cancel\n● 1 MCP failure — see /mcp\n─────────────────────────────────────────────────────\nKiro · auto · ◔ 6%                                  ~\n\n Kiro is working · type to queue a message";
        assert_eq!(detect_state(Some(Agent::Kiro), screen), AgentState::Working);
    }

    #[test]
    fn kiro_working_on_tool_spinner_and_cancel_hint() {
        let screen = "◕ Shell\n  esc to cancel\n─────────────────────────────────────────────────────\nKiro · auto · ◔ 6%";
        assert_eq!(detect_state(Some(Agent::Kiro), screen), AgentState::Working);
    }

    #[test]
    fn kiro_idle_at_prompt() {
        let screen = "● 1 MCP failure — see /mcp\n──────────────────────────────────────────────────────────────────────────────────────\nKiro · auto · ◔ 6%                                                                   ~\n\n ask a question or describe a task ↵\n                                                                   /copy to clipboard";
        assert_eq!(detect_state(Some(Agent::Kiro), screen), AgentState::Idle);
    }

    #[test]
    fn kiro_blocked_on_tool_approval_prompt() {
        let screen = "↓ Shell mkdir -p /tmp/test-kiro-{a,b,c} && ls /tmp/test-kiro-*\n\n─────────────────────────────────────────────────────────────────────────────────────────\n shell requires approval\n ❯ Yes, single permission\n   Trust, always allow in this session\n   No (Tab to edit)\n─────────────────────────────────────────────────────────────────────────────────────────\n ESC to close | Tab to edit";
        assert_eq!(detect_state(Some(Agent::Kiro), screen), AgentState::Blocked);
    }

    #[test]
    fn kiro_does_not_treat_stale_failure_spinner_as_working() {
        let screen = "● 1 MCP failure — see /mcp\n─────────────────────────────────────────────────────\nKiro · auto · ◔ 6%\n\n ask a question or describe a task ↵";
        assert_eq!(detect_state(Some(Agent::Kiro), screen), AgentState::Idle);
    }

    #[test]
    fn kiro_identified_by_process_name() {
        assert_eq!(identify_agent("kiro"), Some(Agent::Kiro));
        assert_eq!(identify_agent("kiro-cli"), Some(Agent::Kiro));
    }

    // ---- Droid ----

    #[test]
    fn droid_working_thinking_with_spinner() {
        let screen = ">  how u doin\n\n⠴ Thinking...  (Press ESC to stop)\n\nAuto (Off)";
        assert_eq!(detect_droid(screen), AgentState::Working);
    }

    #[test]
    fn droid_working_esc_to_stop_alone() {
        // ESC to stop without spinner is still working (UI chrome)
        let screen = "Processing\n(Press ESC to stop)";
        assert_eq!(detect_droid(screen), AgentState::Working);
    }

    #[test]
    fn droid_waiting_execute_approval() {
        let screen = concat!(
            "⛬  I'll create some folders.\n\n",
            "   EXECUTE  (mkdir -p /tmp/test, impact: medium)\n\n",
            "╭────────────────────╮\n",
            "│ > Yes, allow        │\n",
            "│   Yes, always allow │\n",
            "│   No, cancel        │\n",
            "╰────────────────────╯\n",
            "   Use ↑↓ to navigate, Enter to select, Esc to cancel\n",
        );
        assert_eq!(detect_droid(screen), AgentState::Blocked);
    }

    #[test]
    fn droid_waiting_selection_with_chrome() {
        let screen = "│ > Yes, allow │\n│   No, cancel │\n   Use ↑↓ to navigate, Enter to select, Esc to cancel";
        assert_eq!(detect_droid(screen), AgentState::Blocked);
    }

    #[test]
    fn droid_not_waiting_on_options_text_alone() {
        // "Yes, allow" in normal conversation should NOT trigger blocked
        let screen = "The user said > Yes, allow the changes";
        assert_eq!(detect_droid(screen), AgentState::Idle);
    }

    #[test]
    fn droid_idle_prompt() {
        let screen =
            "╭──────────────────╮\n│ > Try something   │\n╰──────────────────╯\n? for help";
        assert_eq!(detect_droid(screen), AgentState::Idle);
    }

    #[test]
    fn droid_idle_after_response() {
        let screen =
            "⛬  Doing well, thanks!\n\nAuto (Off)\n╭──────────╮\n│ >        │\n╰──────────╯";
        assert_eq!(detect_droid(screen), AgentState::Idle);
    }

    #[test]
    fn droid_braille_spinner_detected() {
        assert!(has_braille_spinner("⠴ Thinking..."));
        assert!(has_braille_spinner("  ⠧ Loading..."));
        assert!(has_braille_spinner("text\n⠋ Working\nmore"));
    }

    #[test]
    fn droid_braille_spinner_no_false_positive() {
        assert!(!has_braille_spinner("normal text"));
        assert!(!has_braille_spinner("Thinking..."));
        assert!(!has_braille_spinner("some ⠴ in middle of text"));
    }

    #[test]
    fn droid_identified_by_process_name() {
        assert_eq!(identify_agent("droid"), Some(Agent::Droid));
    }

    // ---- Amp ----

    #[test]
    fn amp_blocked_waiting_for_approval() {
        let screen = "Invoke tool shell_command?\n▸● Approve [Alt+1]\n ○ Allow All for This Session [Alt+2]\n ○ Allow All for Every Session [Alt+3]\n ○ Deny with feedback [Alt+4]\nWaiting for approval...";
        assert_eq!(detect_state(Some(Agent::Amp), screen), AgentState::Blocked);
    }

    #[test]
    fn amp_blocked_run_this_command() {
        let screen = "Run this command?\nrg --files\n▸● Approve [Alt+1]\n ○ Allow All for This Session [Alt+2]\n ○ Allow All for Every Session [Alt+3]\n ○ Deny with feedback [Alt+4]";
        assert_eq!(detect_state(Some(Agent::Amp), screen), AgentState::Blocked);
    }

    #[test]
    fn amp_blocked_allow_editing_file() {
        let screen = "Allow editing file:\nsrc/detect.rs\n▸● Approve [Alt+1]\n ○ Allow File for Every Session [Alt+2]\n ○ Allow All for This Session [Alt+3]\n ○ Deny with feedback [Alt+4]";
        assert_eq!(detect_state(Some(Agent::Amp), screen), AgentState::Blocked);
    }

    #[test]
    fn amp_blocked_allow_creating_file() {
        let screen = "Allow creating file:\nsrc/new_file.rs\n▸● Approve [Alt+1]\n ○ Allow File for Every Session [Alt+2]\n ○ Allow All for This Session [Alt+3]\n ○ Deny with feedback [Alt+4]";
        assert_eq!(detect_state(Some(Agent::Amp), screen), AgentState::Blocked);
    }

    #[test]
    fn amp_working_running_tools() {
        let screen = "  ✓ Search Map the core runtime architecture\n  ⋯ Oracle ▼\n  ≈ Running tools...         Esc to cancel";
        assert_eq!(detect_state(Some(Agent::Amp), screen), AgentState::Working);
    }

    #[test]
    fn amp_idle() {
        let screen = "  Response complete.\n\n╭─100% of 272k · $1.20─────────────────────────╮\n│                                               │\n╰───────────────────────~/Projects/herdr (master)╯";
        assert_eq!(detect_state(Some(Agent::Amp), screen), AgentState::Idle);
    }

    #[test]
    fn amp_identified_by_process_name() {
        assert_eq!(identify_agent("amp"), Some(Agent::Amp));
        assert_eq!(identify_agent("amp-local"), Some(Agent::Amp));
    }

    // ---- Grok ----

    #[test]
    fn grok_blocked_on_permission_prompt() {
        let screen = "Show recent commit history for analysis\n\
                      git -C /home/can/Projects/herdr log --oneline --decorate -n 12\n\
                      Use ← → to choose permission whitelist scope\n\n\
                      1 (○) Always allow: git -C\n\
                      2 (●) Yes, proceed\n\
                      3 (○) No, reject (type to add feedback)\n\n\
                      1/3:select │ ←/→:scope │ Ctrl+o:yolo │ Ctrl+c:cancel";
        assert_eq!(detect_state(Some(Agent::Grok), screen), AgentState::Blocked);
    }

    #[test]
    fn grok_blocked_wins_over_spinner() {
        let screen = "⠹ Run git 30s\nYes, proceed\nNo, reject (type to add feedback)";
        assert_eq!(detect_state(Some(Agent::Grok), screen), AgentState::Blocked);
    }

    #[test]
    fn grok_working_on_waiting_spinner() {
        let screen = "⠋ Waiting… 1.8s\nCtrl+c:cancel │ Ctrl+Enter:interject";
        assert_eq!(detect_state(Some(Agent::Grok), screen), AgentState::Working);
    }

    #[test]
    fn grok_working_on_tool_spinner() {
        let screen = "⠼ Run git -C /home/can/Projects/herdr log --oneline 1.0s";
        assert_eq!(detect_state(Some(Agent::Grok), screen), AgentState::Working);
    }

    #[test]
    fn grok_idle_after_turn_completed() {
        let screen = "yo\n\nTurn completed in 1.7s.\n\n╭────╮\n│ ❯  │\n╰─ gpt-5.4 ─╯";
        assert_eq!(detect_state(Some(Agent::Grok), screen), AgentState::Idle);
    }

    // ---- Hermes ----

    #[test]
    fn hermes_identified_by_process_name() {
        assert_eq!(identify_agent("hermes"), Some(Agent::Hermes));
        assert_eq!(identify_agent("hermes-agent"), Some(Agent::Hermes));
    }

    #[test]
    fn hermes_working_on_interrupt_footer() {
        let screen = "  (⌐■_■) computing...\n\n ⚕ gpt-5.5 │ 15.5K/272K │ [█░░░░░░░░░] 6% │ 2m │ ⏱ 3s\n─────────────────────────────────────────────────────────────────────────────────────────\n⚕ ❯ msg=interrupt · /queue · /bg · /steer · Ctrl+C cancel";
        assert_eq!(
            detect_state(Some(Agent::Hermes), screen),
            AgentState::Working
        );
    }

    #[test]
    fn hermes_idle_ignores_stale_initializing_agent_text() {
        let screen = "● say exactly READY and stop\nInitializing agent...\n\n╭─ ⚕ Hermes ────────────────────────────────────────────────────────────────────────────╮\n    READY\n╰───────────────────────────────────────────────────────────────────────────────────────╯\n ⚕ gpt-5.5 │ 15.5K/272K │ [█░░░░░░░░░] 6% │ 15s │ ⏲ 2s\n─────────────────────────────────────────────────────────────────────────────────────────\n❯";
        assert_eq!(detect_state(Some(Agent::Hermes), screen), AgentState::Idle);
    }

    #[test]
    fn hermes_blocked_on_dangerous_command_prompt() {
        let screen = "╭────────────────────────────────────────────────────────────╮\n│ ⚠️  Dangerous Command                                      │\n│ mkdir -p /tmp/herdr-hermes-block-test/subdir && touch      │\n│ ❯ 1. Allow once                                            │\n│   2. Allow for this session                                │\n│   3. Add to permanent allowlist                            │\n│   4. Deny                                                  │\n│   5. Show full command                                     │\n╰────────────────────────────────────────────────────────────╯\n  ↑/↓ to select, Enter to confirm\n⚠ ❯";
        assert_eq!(
            detect_state(Some(Agent::Hermes), screen),
            AgentState::Blocked
        );
    }

    #[test]
    fn hermes_idle_at_prompt_after_response() {
        let screen = "╭─ ⚕ Hermes ────────────────────────────────────────────────────────────────────────────╮\n    READY\n╰───────────────────────────────────────────────────────────────────────────────────────╯\n ⚕ gpt-5.5 │ 15.5K/272K │ [█░░░░░░░░░] 6% │ 15s │ ⏲ 2s\n─────────────────────────────────────────────────────────────────────────────────────────\n❯\n─────────────────────────────────────────────────────────────────────────────────────────";
        assert_eq!(detect_state(Some(Agent::Hermes), screen), AgentState::Idle);
    }

    #[test]
    fn hermes_denied_message_is_idle() {
        let screen = "╭─ ⚕ Hermes ────────────────────────────────────────────────────────────────────────────╮\n    Command was blocked/denied by the safety layer. I did not retry.\n╰───────────────────────────────────────────────────────────────────────────────────────╯\n ⚕ gpt-5.5 │ 15.4K/272K │ [█░░░░░░░░░] 6% │ 2m │ ⏲ 11s\n─────────────────────────────────────────────────────────────────────────────────────────\n❯";
        assert_eq!(detect_state(Some(Agent::Hermes), screen), AgentState::Idle);
    }

    // ---- Qodercli ----

    #[test]
    fn qodercli_identified_by_process_name() {
        assert_eq!(identify_agent("qodercli"), Some(Agent::Qodercli));
        assert_eq!(identify_agent("qoderclicn"), Some(Agent::Qodercli));
        assert_eq!(identify_agent("qoder"), Some(Agent::Qodercli));
        assert_eq!(identify_agent("qodercn"), Some(Agent::Qodercli));
    }

    #[test]
    fn qodercli_blocked_on_confirmation() {
        assert_eq!(
            detect_qodercli("Waiting for user confirmation..."),
            AgentState::Blocked,
        );
    }

    #[test]
    fn qodercli_working_on_spinner() {
        assert_eq!(detect_qodercli("\u{280B} Thinking..."), AgentState::Working);
    }

    #[test]
    fn qodercli_idle_on_prompt() {
        assert_eq!(detect_qodercli("> "), AgentState::Idle);
    }

    #[test]
    fn qodercli_idle_when_only_stale_braille_glyph_in_scrollback() {
        // A single stray braille character in a previous output line must not
        // flip the pane to Working — only an actual spinner *row* should.
        let screen = "\
agent finished a previous task.\n\
\u{280B}\n\
> \n";
        assert_eq!(detect_qodercli(screen), AgentState::Idle);
    }

    #[test]
    fn qodercli_working_on_full_spinner_row() {
        // Real spinner row: braille glyph + space + alphabetic phrase.
        let screen = "\u{280B} Thinking...\n";
        assert_eq!(detect_qodercli(screen), AgentState::Working);
    }

    #[test]
    fn qodercli_working_on_esc_to_cancel_hint() {
        // The "(esc to cancel, …)" suffix is qodercli's explicit working
        // marker. It must trigger Working even if a hook icon replaced the
        // spinner glyph in this frame.
        let screen = "Thinking... (esc to cancel, 5s)\n";
        assert_eq!(detect_qodercli(screen), AgentState::Working);
    }

    #[test]
    fn qodercli_idle_when_text_mentions_working_in_prose() {
        // The previous heuristic treated the bare word "working" as Working,
        // which produced false positives for narrative output (commits, logs,
        // Markdown). The pane should remain Idle until a real working signal
        // appears.
        let screen = "\
fix: keep working set warm across reloads\n\
\n\
> \n";
        assert_eq!(detect_qodercli(screen), AgentState::Idle);
    }

    #[test]
    fn qodercli_idle_override_wins_over_spinner_row() {
        // While the user is holding Ctrl+C, qodercli flashes a "press again"
        // banner over the prompt. The pane is effectively idle there even if
        // a stale spinner row is still in the buffer.
        let screen = "\
\u{280B} Thinking...\n\
Press Ctrl+C again to exit.\n";
        assert_eq!(detect_qodercli(screen), AgentState::Idle);
    }

    #[test]
    fn qodercli_idle_override_wins_over_esc_rewind() {
        let screen = "Press Esc again to rewind.\n";
        assert_eq!(detect_qodercli(screen), AgentState::Idle);
    }

    #[test]
    fn qodercli_blocked_on_permission_required_dialog() {
        // qodercli renders this dialog when a tool call needs user approval.
        let screen = "\
Permission Required\n\
Caller: test\n\
Command: mkdir -p /root/example\n\
Allow once or always?\n\
  \u{276F} 1. Allow Once - allow `mkdir` for one\n\
    2. Always allow `mkdir` for future sessions\n\
    3. Reject and tell qodercli something\n";
        assert_eq!(detect_qodercli(screen), AgentState::Blocked);
    }

    #[test]
    fn qodercli_blocked_on_permission_required_alone() {
        // Even when the prompt copy gets truncated by the viewport, the title
        // alone should be enough to flip the pane to blocked.
        assert_eq!(detect_qodercli("Permission Required"), AgentState::Blocked,);
    }

    #[test]
    fn qodercli_blocked_on_askuser_enter_response_placeholder() {
        // qodercli's ask-user tool renders an input box with this placeholder
        // when waiting for the user to type a response.
        let screen = "\
What kind of project are you working on?\n\
> \n\
  Enter your response\n";
        assert_eq!(detect_qodercli(screen), AgentState::Blocked);
    }

    #[test]
    fn qodercli_blocked_on_askuser_review_tab() {
        // The multi-question/multi-select review tab heading is unique to the
        // ask-user dialog and means the agent is parked waiting on user input.
        let screen = "\
Review your answers:\n\
\n\
Project type \u{2192} Web app\n\
Stack        \u{2192} (not answered)\n";
        assert_eq!(detect_qodercli(screen), AgentState::Blocked);
    }

    #[test]
    fn qodercli_blocked_on_interactive_shell_waiting() {
        // When qodercli spawns an interactive shell, the loading row turns
        // into a "Shell awaiting input" hint until the user takes focus.
        let screen = "! Shell awaiting input (Tab to focus)\n";
        assert_eq!(detect_qodercli(screen), AgentState::Blocked);
    }

    #[test]
    fn qodercli_blocked_on_askuser_single_choice_dialog() {
        // Single-select ask-user has no "Enter your response" placeholder and
        // no "Review your answers:" heading. The BaseTabDialog title
        // "Asking User" is the only stable signal across every ask-user form.
        let screen = "\
Asking User\n\
\n\
Which framework should we use?\n\
  React\n\
  Vue\n\
  Svelte\n";
        assert_eq!(detect_qodercli(screen), AgentState::Blocked);
    }

    // ---- Helpers ----

    #[test]
    fn content_above_prompt_box_extracts_correctly() {
        let screen = "line1\nline2\n──────\n❯ \n──────";
        let above = content_above_prompt_box(screen);
        assert!(above.contains("line1"));
        assert!(above.contains("line2"));
        assert!(!above.contains('❯'));
    }

    #[test]
    fn content_above_prompt_box_no_box() {
        let screen = "just some text\nno borders here";
        let above = content_above_prompt_box(screen);
        assert_eq!(above, screen);
    }

    #[test]
    fn spinner_activity_detected() {
        assert!(has_spinner_activity("· Thinking…"));
        assert!(has_spinner_activity("✽ Tempering…"));
        assert!(has_spinner_activity("✳ Simplifying recompute_tangents…"));
        assert!(has_spinner_activity("  ✶ Reading…")); // with leading whitespace
        assert!(has_spinner_activity("✻ Pouncing…"));
        assert!(has_spinner_activity("✽ Processing…"));
    }

    #[test]
    fn spinner_activity_not_false_positive() {
        assert!(!has_spinner_activity("normal text"));
        assert!(!has_spinner_activity("✽ no ellipsis here"));
        assert!(!has_spinner_activity("✽ …"));
        assert!(!has_spinner_activity("some ✽ in the middle"));
    }

    #[test]
    fn cursor_spinner_detected() {
        assert!(has_cursor_spinner("⬡ Grepping.."));
        assert!(has_cursor_spinner("⬢ Reading…"));
        assert!(has_cursor_spinner("⠠⠜ Running  5.52k tokens"));
        assert!(has_cursor_spinner("⠞ Working  5.62k tokens"));
        assert!(has_cursor_spinner("⠛ Grepping  1.2k tokens"));
        assert!(has_cursor_spinner("⠛ Analyzing  1.2k tokens"));
    }

    #[test]
    fn cursor_spinner_not_false_positive() {
        assert!(!has_cursor_spinner("normal text"));
        assert!(!has_cursor_spinner("some ⬡ in middle"));
        assert!(!has_cursor_spinner("⠛ Read notes"));
    }

    #[test]
    fn terminal_tail_content_works_with_detection() {
        assert_eq!(detect_pi("Working..."), AgentState::Working);
    }

    #[test]
    fn ansi_colored_content_still_detects_working() {
        assert_eq!(detect_pi("\x1b[31mWorking...\x1b[0m"), AgentState::Working);
    }

    #[test]
    fn visible_claude_prompt_box_is_idle() {
        let screen = "Task complete.\n─────────────\n❯ \n─────────────";
        assert_eq!(detect_claude(screen), AgentState::Idle);
    }
}
