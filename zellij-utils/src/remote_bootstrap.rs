//! Provider-agnostic install scripts for the remote-agent binary. Each remote
//! provider (coder, ssh, later devcontainer) wraps these scripts in its own
//! transport argv; the script bodies never change per provider.

pub const RELEASE_BASE_URL: &str = "https://github.com/abeljim8am/flock/releases/download";

/// The release whose assets the remote installs, derived from the running
/// build rather than hand-edited. A constant that drifts behind the local
/// version leaves the remote permanently, unfixably out of date: the installer
/// keys its destination directory on this tag, so a stale tag means every
/// bootstrap re-confirms a build the local side already considers old.
///
/// `VERSION` is the workspace version (this plugin crate carries its own
/// `0.1.0`, which is not what the remote should be pinned to).
pub fn release_tag() -> String {
    format!("v{}", crate::consts::VERSION)
}

/// Detect the remote architecture and pick the matching static musl release
/// asset. Linux x86_64 and aarch64 are supported; anything else fails with a
/// deliberate exit 65 so callers surface an actionable error.
const ARCH_CASE: &str = r#"case "$(uname -s)/$(uname -m)" in
Linux/x86_64) triple=x86_64-unknown-linux-musl ;;
Linux/aarch64|Linux/arm64) triple=aarch64-unknown-linux-musl ;;
*) echo "flock: persistent remote sessions require Linux x86_64 or aarch64" >&2; exit 65 ;;
esac"#;

/// Bootstrap the fork into a versioned user directory on the remote host.
/// Installation is atomic and checksum verified; repeated calls are cheap
/// because an already-installed tag short-circuits.
pub fn install_script() -> String {
    install_script_inner(true)
}

/// The same install, with the already-installed short-circuit removed. This is
/// what makes a repair possible: without it there is no way to replace a
/// corrupt or wrong-protocol binary that happens to sit at the current tag,
/// and the only remedy on offer is closing panes, which cannot help.
pub fn reinstall_script() -> String {
    install_script_inner(false)
}

fn install_script_inner(skip_when_present: bool) -> String {
    let short_circuit = if skip_when_present {
        r#"[ -x "$dest/flock" ] && { mkdir -p "$root" "$HOME/.local/bin"; ln -sfn "$dest" "$root/current"; ln -sfn "$dest/flock" "$HOME/.local/bin/flock"; exit 0; }
"#
    } else {
        ""
    };
    format!(
        r#"set -eu
{arch_case}
root="$HOME/.local/share/flock"
dest="$root/{tag}"
{short_circuit}tmp="$root/.bootstrap.$$"
mkdir -p "$tmp" "$dest"
trap "rm -rf \"$tmp\"" EXIT HUP INT TERM
base="{base}/{tag}"
archive="$tmp/flock.tar.gz"
checksum="$tmp/flock.sha256sum"
fetch() {{ if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"; elif command -v wget >/dev/null 2>&1; then wget -qO "$2" "$1"; elif command -v python3 >/dev/null 2>&1; then python3 -c "import sys,urllib.request; urllib.request.urlretrieve(sys.argv[1],sys.argv[2])" "$1" "$2"; else echo "flock: curl, wget, or python3 is required to install remote Zellij" >&2; exit 69; fi; }}
fetch "$base/flock-$triple.tar.gz" "$archive"
fetch "$base/flock-$triple.sha256sum" "$checksum"
tar -xzf "$archive" -C "$tmp"
IFS=" " read -r expected _ < "$checksum"
actual="$(sha256sum "$tmp/flock")"
actual="${{actual%% *}}"
[ -n "$expected" ] && [ "$expected" = "$actual" ] || {{ echo "flock: remote Zellij checksum verification failed" >&2; exit 74; }}
install -m 0755 "$tmp/flock" "$dest/flock.new"
mv -f "$dest/flock.new" "$dest/flock"
mkdir -p "$HOME/.local/bin"
ln -sfn "$dest" "$root/current"
ln -sfn "$dest/flock" "$HOME/.local/bin/flock""#,
        arch_case = ARCH_CASE,
        short_circuit = short_circuit,
        tag = release_tag(),
        base = RELEASE_BASE_URL,
    )
}

/// Remote half of the debug streaming bootstrap: receive an explicitly
/// selected local binary on stdin and install it. No architecture guard
/// beyond Linux — the developer chose the binary, and the `--version`
/// self-check below rejects a wrong-arch build before it replaces anything.
pub fn debug_install_script() -> String {
    format!(
        r#"set -eu
[ "$(uname -s)" = Linux ] || {{ echo "flock: debug remote agent requires Linux" >&2; exit 65; }}
root="$HOME/.local/share/flock"
dest="$root/{tag}-debug"
tmp="$dest/.flock.$$"
mkdir -p "$dest" "$HOME/.local/bin"
trap "rm -f \"$tmp\"" EXIT HUP INT TERM
cat > "$tmp"
chmod 0755 "$tmp"
"$tmp" --version >/dev/null
mv -f "$tmp" "$dest/flock"
ln -sfn "$dest" "$root/current"
ln -sfn "$dest/flock" "$HOME/.local/bin/flock""#,
        tag = release_tag(),
    )
}

/// SSH-style transports join command arguments into one command line for the
/// remote login shell before invoking `sh`. A single-quoted argument is
/// understood by both POSIX shells and Fish, but there is no shared way to
/// escape a single quote inside it. Keep generated scripts free of single
/// quotes and fail loudly if a future edit violates that transport invariant.
/// Use a non-login `sh`: login-shell logout hooks can overwrite a successful
/// exit code.
pub fn quote_remote_script_arg(value: &str) -> String {
    assert!(
        !value.contains('\''),
        "remote scripts must not contain single quotes"
    );
    format!("'{value}'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_detects_both_supported_arches() {
        let script = install_script();
        assert!(script.contains("x86_64-unknown-linux-musl"));
        assert!(script.contains("aarch64-unknown-linux-musl"));
        assert!(script.contains(r#"case "$(uname -s)/$(uname -m)""#));
        assert!(script.contains("Linux/aarch64|Linux/arm64"));
        assert!(script.contains("flock-$triple.tar.gz"));
        assert!(script.contains("flock-$triple.sha256sum"));
        assert!(!script.contains('\''));
    }

    #[test]
    fn reinstall_drops_the_already_installed_short_circuit() {
        // The short-circuit is exactly what makes a same-tag repair impossible,
        // so its absence is the whole point of the reinstall variant.
        assert!(install_script().contains(r#"[ -x "$dest/flock" ]"#));
        assert!(!reinstall_script().contains(r#"[ -x "$dest/flock" ]"#));
        // Everything else must stay identical, checksum verification included.
        assert!(reinstall_script().contains("exit 74"));
        assert!(reinstall_script().contains("flock-$triple.tar.gz"));
        assert!(!reinstall_script().contains('\''));
    }

    #[test]
    fn release_tag_tracks_the_running_build() {
        assert_eq!(release_tag(), format!("v{}", crate::consts::VERSION));
        assert!(install_script().contains(&release_tag()));
    }

    #[test]
    fn quote_wraps_and_rejects_single_quotes() {
        assert_eq!(quote_remote_script_arg("printf %s"), "'printf %s'");
        assert!(std::panic::catch_unwind(|| quote_remote_script_arg("printf '%s'")).is_err());
    }
}
