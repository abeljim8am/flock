// installed by flock-sidebar (zellij)
// managed by flock-sidebar; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// FLOCK_INTEGRATION_ID=opencode
// FLOCK_INTEGRATION_VERSION=1
//
// Ported from herdr's opencode integration plugin (herdr-agent-state.js,
// HERDR_INTEGRATION_VERSION=8). Instead of writing herdr's unix socket, it
// reports the agent's state to the flock-sidebar plugin over a Zellij CLI pipe:
//
//   zellij pipe --name flock-state --args 'pane_id=<id>,state=<state>,agent=opencode,...'
//
// Zellij exports the running pane's id as $ZELLIJ_PANE_ID, which the plugin maps
// back to the pane it tracks. Install by copying this file to
// ~/.config/opencode/plugins/flock-agent-state.js (the directory herdr installs
// its plugin to). herdr's session reporting (pane.report_agent_session) is
// dropped — flock has no session-resume consumer — but the subagent-suppression
// logic is herdr's, kept verbatim so task-tool sessions can't clobber the pane's
// root-agent state.

import { spawn } from "node:child_process";

const SOURCE = "flock:opencode";
const AGENT = "opencode";

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
  const paneId = process.env.ZELLIJ_PANE_ID;
  if (!paneId) {
    return Promise.resolve();
  }

  const args = `pane_id=${paneId},state=${state},agent=${AGENT},source=${SOURCE}`;

  return new Promise((resolve) => {
    let child;
    try {
      // stdio must be closed: `zellij pipe` blocks reading stdin otherwise
      // (the shell hooks pipe from /dev/null for the same reason).
      child = spawn("zellij", ["pipe", "--name", "flock-state", "--args", args], {
        stdio: "ignore",
        timeout: 2000,
      });
    } catch {
      resolve();
      return;
    }
    child.on("error", () => resolve());
    child.on("exit", () => resolve());
  });
}

export const FlockAgentStatePlugin = async () => {
  if (!process.env.ZELLIJ_PANE_ID) {
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
