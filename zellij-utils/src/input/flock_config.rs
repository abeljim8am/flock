//! The `flock { }` configuration section — one place to tell Flock about your
//! world.
//!
//! Both Flock plugins take the same folder sources and provider flags as KDL
//! args. Stating them per-call-site meant saying the same thing in a layout, a
//! keybinding, and every generated remote layout, and a set that disagreed with
//! another had a silent, destructive failure mode (see
//! `running_plugin_satisfies_request` in `zellij-server`). This block is the
//! single source those call sites derive from:
//!
//! ```kdl
//! flock {
//!     root_dirs "~/src" "~/work"     // each scanned one level deep
//!     individual_dirs "~/dotfiles"   // each is itself one project
//!     devcontainers true
//!     ssh true
//! }
//! ```
//!
//! The values are projected **underneath** the `flock-selector` / `flock-sidebar`
//! plugin aliases, so anything a call site states still wins and a single layout
//! can opt out. See [`crate::input::config::Config::plugin_aliases_with_flock_defaults`].
//!
//! Note the names here are the *config* names, which deliberately read better
//! than the plugin arg names they translate to: `devcontainers true` rather than
//! `devcontainers_enabled "true"`, and a real list rather than a `;`-joined
//! string. [`FlockConfig::to_plugin_configuration`] owns that translation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Values from the `flock { }` config section. Every field is optional: unset
/// means "say nothing", which leaves the plugin's own default in force rather
/// than overriding it with a zero value.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct FlockConfig {
    /// Folders scanned one level deep; each immediate subdirectory is a project.
    pub root_dirs: Vec<String>,
    /// Folders that are each themselves one project.
    pub individual_dirs: Vec<String>,
    /// Layout new local project sessions open with.
    pub session_layout: Option<String>,
    /// Layout used as the base for remote-bound sessions.
    pub remote_session_layout: Option<String>,
    pub codespaces: Option<bool>,
    pub devcontainers: Option<bool>,
    pub coder: Option<bool>,
    pub ssh: Option<bool>,
    pub coder_dotfiles_uri: Option<String>,
    pub coder_dotfiles_branch: Option<String>,
    pub coder_dotfiles_parameter: Option<String>,
    pub coder_dotfiles_branch_parameter: Option<String>,
    /// Whether bare `flock` opens the project selector. Defaults to on; see
    /// [`FlockConfig::selector_on_startup`]. Unlike every other field this one is
    /// consumed by the CLI rather than passed to a plugin, so it is deliberately
    /// absent from [`FlockConfig::to_plugin_configuration`].
    pub selector_on_startup: Option<bool>,
}

impl FlockConfig {
    /// `other` wins field by field. A list is replaced only when the incoming one
    /// is non-empty, so a later file that says nothing about folders does not
    /// erase an earlier file's.
    pub fn merge(&self, other: FlockConfig) -> Self {
        FlockConfig {
            root_dirs: if other.root_dirs.is_empty() {
                self.root_dirs.clone()
            } else {
                other.root_dirs
            },
            individual_dirs: if other.individual_dirs.is_empty() {
                self.individual_dirs.clone()
            } else {
                other.individual_dirs
            },
            session_layout: other.session_layout.or_else(|| self.session_layout.clone()),
            remote_session_layout: other
                .remote_session_layout
                .or_else(|| self.remote_session_layout.clone()),
            codespaces: other.codespaces.or(self.codespaces),
            devcontainers: other.devcontainers.or(self.devcontainers),
            coder: other.coder.or(self.coder),
            ssh: other.ssh.or(self.ssh),
            coder_dotfiles_uri: other
                .coder_dotfiles_uri
                .or_else(|| self.coder_dotfiles_uri.clone()),
            coder_dotfiles_branch: other
                .coder_dotfiles_branch
                .or_else(|| self.coder_dotfiles_branch.clone()),
            coder_dotfiles_parameter: other
                .coder_dotfiles_parameter
                .or_else(|| self.coder_dotfiles_parameter.clone()),
            coder_dotfiles_branch_parameter: other
                .coder_dotfiles_branch_parameter
                .or_else(|| self.coder_dotfiles_branch_parameter.clone()),
            selector_on_startup: other.selector_on_startup.or(self.selector_on_startup),
        }
    }

    /// Whether bare `flock` opens the project selector instead of dropping
    /// straight into a shell. On by default — it is what makes a fresh install
    /// land in Flock rather than in a plain multiplexer. Setting it to false keeps
    /// Flock usable as a drop-in Zellij replacement; `flock pick` still opens the
    /// selector on demand.
    pub fn selector_on_startup(&self) -> bool {
        self.selector_on_startup.unwrap_or(true)
    }

    /// True when nothing was configured, so callers can skip projecting entirely
    /// and leave plugin configuration byte-identical to what the call site said.
    pub fn is_empty(&self) -> bool {
        self == &FlockConfig::default()
    }

    /// Translate into the plugin arg names and value encodings the two Flock
    /// plugins parse: `;`-joined path lists and `"true"` / `"false"` strings.
    ///
    /// The same map goes to both plugins. The sidebar reads a subset of it (it
    /// never creates workspaces, so the Coder dotfiles keys mean nothing to it)
    /// and ignores the rest — projecting one map keeps the two from drifting as
    /// either plugin gains keys, and keeps their configuration identical so
    /// plugin matching cannot near-miss between them.
    pub fn to_plugin_configuration(&self) -> BTreeMap<String, String> {
        let mut configuration = BTreeMap::new();
        let mut insert_paths = |key: &str, paths: &Vec<String>| {
            if !paths.is_empty() {
                configuration.insert(key.to_owned(), paths.join(";"));
            }
        };
        insert_paths("root_dirs", &self.root_dirs);
        insert_paths("individual_dirs", &self.individual_dirs);

        let mut insert_string = |key: &str, value: &Option<String>| {
            if let Some(value) = value {
                configuration.insert(key.to_owned(), value.clone());
            }
        };
        insert_string("session_layout", &self.session_layout);
        insert_string("remote_session_layout", &self.remote_session_layout);
        insert_string("coder_dotfiles_uri", &self.coder_dotfiles_uri);
        insert_string("coder_dotfiles_branch", &self.coder_dotfiles_branch);
        insert_string("coder_dotfiles_parameter", &self.coder_dotfiles_parameter);
        insert_string(
            "coder_dotfiles_branch_parameter",
            &self.coder_dotfiles_branch_parameter,
        );

        let mut insert_flag = |key: &str, value: Option<bool>| {
            if let Some(value) = value {
                configuration.insert(key.to_owned(), value.to_string());
            }
        };
        insert_flag("codespaces_enabled", self.codespaces);
        insert_flag("devcontainers_enabled", self.devcontainers);
        insert_flag("coder_enabled", self.coder);
        insert_flag("ssh_enabled", self.ssh);

        configuration
    }
}

/// The alias name of the project selector plugin.
pub const FLOCK_SELECTOR_PLUGIN_ALIAS: &str = "flock-selector";

/// The plugin alias names the `flock { }` section feeds.
pub const FLOCK_PLUGIN_ALIASES: [&str; 2] = [FLOCK_SELECTOR_PLUGIN_ALIAS, "flock-sidebar"];

/// The fixed session name the project selector runs in.
///
/// Fixed rather than generated so that repeatedly reaching for the picker lands
/// in the same session instead of accumulating throwaway ones, and so the sidebar
/// can recognise and hide it. The bundled `flock-selector` layout states the same
/// name in its `session_name` plugin arg; a test keeps the two in step.
pub const FLOCK_SELECTOR_SESSION_NAME: &str = "flock-selector";

/// The layout the selector session opens with. Resolved by name, so a user's own
/// `layouts/flock-selector.kdl` takes precedence over the bundled one.
pub const FLOCK_SELECTOR_LAYOUT_NAME: &str = "flock-selector";
