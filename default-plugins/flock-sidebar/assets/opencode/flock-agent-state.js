// installed by flock-sidebar (flock)
// managed by flock-sidebar; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// FLOCK_INTEGRATION_ID=opencode
// FLOCK_INTEGRATION_VERSION=5
//
// Ported from herdr's opencode integration plugin (herdr-agent-state.js,
// HERDR_INTEGRATION_VERSION=8). Instead of writing herdr's unix socket, it
// reports the agent's state to the flock-sidebar plugin over a Zellij CLI pipe:
//
//   flock pipe --name flock-state --args 'pane_id=<id>,state=<state>,agent=opencode,...'
//
// Flock exports the running pane's id as $FLOCK_PANE_ID, which the plugin maps
// back to the pane it tracks. Install by copying this file to
// ~/.config/opencode/plugins/flock-agent-state.js (the directory herdr installs
// its plugin to). herdr's session reporting (pane.report_agent_session) is
// dropped — flock has no session-resume consumer — but the subagent-suppression
// logic is herdr's, kept verbatim so task-tool sessions can't clobber the pane's
// root-agent state.
//
// v2 added a devcontainer file channel. v3 makes the transport explicit:
// flock's devcontainer wrappers forward both FLOCK_PANE_ID and
// FLOCK_STATE_CHANNEL=file. Reports are written to /tmp/flock-state/pane-<id>
// (one line, the same key=value format as the pipe args, plus ts=<epoch secs>),
// which flock-sidebar polls from the host via `docker exec … cat`. For older
// wrappers without the marker, a failed `zellij pipe` delivery switches the
// plugin to the file channel. Locally the successful pipe remains the channel.
// v5 adds the Coder remote-agent channel. The same plugin file is used in every
// environment; FLOCK_STATE_CHANNEL selects the transport at runtime.

import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync, renameSync } from "node:fs";
import { join } from "node:path";

const SOURCE = "flock:opencode";
const AGENT = "opencode";

// Where the file channel writes, and the sidebar's `docker exec` poll reads.
// Must stay in sync with `hooks_cat_argv` in flock-sidebar/src/devcontainer.rs.
const STATE_DIR = "/tmp/flock-state";

// Bound devcontainer sessions mark the file transport explicitly. The common
// devcontainer markers cover older wrappers; absence of FLOCK_SESSION_NAME
// lets a failed pipe below identify the older pane-id-only handoff without
// turning a transient failure in a real local session into a permanent switch.
const fileChannelRequested =
  process.env.FLOCK_STATE_CHANNEL?.trim().toLowerCase() === "file" ||
  Boolean(process.env.REMOTE_CONTAINERS || process.env.DEVCONTAINER);
const remoteAgentRequested =
  process.env.FLOCK_STATE_CHANNEL?.trim().toLowerCase() === "remote-agent";
const fileFallbackAllowed = !process.env.FLOCK_SESSION_NAME;
let useFileChannel = fileChannelRequested;

// Best-effort atomic write (tmp + rename) so the sidebar's poll never reads a
// half-written line. The pane id lands in the filename, so only the server's
// bare-integer $FLOCK_PANE_ID shape is accepted.
function writeStateFile(paneId, state) {
  if (!/^\d+$/.test(paneId)) {
    return;
  }
  const line = `pane_id=${paneId},state=${state},agent=${AGENT},source=${SOURCE},ts=${Math.floor(
    Date.now() / 1000,
  )}\n`;
  try {
    mkdirSync(STATE_DIR, { recursive: true });
    const path = join(STATE_DIR, `pane-${paneId}`);
    const tmp = `${path}.tmp-${process.pid}`;
    writeFileSync(tmp, line);
    renameSync(tmp, path);
  } catch {
    // Reporting is best-effort, like the pipe channel.
  }
}

// Subagent (task tool) sessions carry a parentID; the main agent session does
// not. Their lifecycle events would otherwise clobber the pane's real state, so
// learn child session ids from session.created/updated and drop their reports.
const childSessions = new Set();

function sessionIDFromProperties(properties) {
  return typeof properties?.sessionID === "string" && properties.sessionID
    ? properties.sessionID
    : undefined;
}

function stateFromSessionStatus(status) {
  // session.status carries { type: "idle" | "busy" | "retry" }; older builds used a bare string.
  const kind = typeof status === "string" ? status : status?.type;
  if (typeof kind !== "string") return undefined;
  switch (kind.toLowerCase()) {
    case "idle":
      return "idle";
    case "active":
    case "busy":
    case "pending":
    case "running":
    case "streaming":
    case "working":
    case "retry":
      return "working";
    default:
      return undefined;
  }
}

function reportState(state) {
  const paneId = process.env.FLOCK_PANE_ID;
  if (!paneId) {
    return Promise.resolve();
  }

  if (useFileChannel) {
    writeStateFile(paneId, state);
    return Promise.resolve();
  }

  if (remoteAgentRequested) {
    const flockExecutable = process.env.FLOCK_EXECUTABLE;
    if (!flockExecutable) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      let settled = false;
      const finish = () => {
        if (settled) return;
        settled = true;
        resolve();
      };
      try {
        const child = spawn(
          flockExecutable,
          [
            "remote-agent",
            "report-state",
            "--pane-id",
            paneId,
            "--state",
            state,
            "--agent",
            AGENT,
          ],
          { stdio: "ignore", timeout: 2000 },
        );
        child.once("error", finish);
        child.once("exit", finish);
      } catch {
        finish();
      }
    });
  }

  const args = `pane_id=${paneId},state=${state},agent=${AGENT},source=${SOURCE}`;

  return new Promise((resolve) => {
    let settled = false;
    const finish = (delivered) => {
      if (settled) return;
      settled = true;
      if (!delivered && fileFallbackAllowed) {
        // The command existing is not enough: inside a container it may have
        // no route to the host server. Preserve this report and send future
        // ones through the file bridge.
        useFileChannel = true;
        writeStateFile(paneId, state);
      }
      resolve();
    };
    let child;
    try {
      // stdio must be closed: `flock pipe` blocks reading stdin otherwise
      // (the shell hooks pipe from /dev/null for the same reason).
      const flockExecutable = process.env.FLOCK_EXECUTABLE;
      if (!flockExecutable) {
        finish(false);
        return;
      }
      child = spawn(flockExecutable, ["pipe", "--name", "flock-state", "--args", args], {
        stdio: "ignore",
        timeout: 2000,
      });
    } catch {
      if (fileFallbackAllowed) {
        useFileChannel = true;
        writeStateFile(paneId, state);
      }
      resolve();
      return;
    }
    child.once("error", () => finish(false));
    child.once("exit", (code) => finish(code === 0));
  });
}

export const FlockAgentStatePlugin = async () => {
  if (!process.env.FLOCK_PANE_ID) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      if (sessionID && childSessions.has(sessionID)) {
        return;
      }
      await reportState("working");
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      const info = properties.info;
      if (info?.id && info.parentID) {
        childSessions.add(info.id);
      }
      if (sessionID && childSessions.has(sessionID)) {
        // Child session events are dropped so they cannot clobber the pane's
        // root-agent state, but a subagent waiting on the user must still
        // surface as blocked (and clear once answered).
        switch (type) {
          case "permission.asked":
          case "question.asked":
            await reportState("blocked");
            break;
          case "permission.replied":
          case "question.replied":
          case "question.rejected":
            await reportState("working");
            break;
          default:
            break;
        }
        return;
      }

      switch (type) {
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            await reportState(state);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          await reportState("working");
          break;
        case "permission.asked":
        case "question.asked":
        case "session.error":
          await reportState("blocked");
          break;
        case "session.idle":
          await reportState("idle");
          break;
        default:
          break;
      }
    },
  };
};
