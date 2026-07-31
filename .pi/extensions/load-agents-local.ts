/**
 * load-agents-local — pi extension (auto-discovered from `.pi/extensions/`).
 *
 * Why this exists: pi auto-reads only `AGENTS.md` / `CLAUDE.md` (a hardcoded list) and has no
 * config field to add another instructions file (unlike opencode's `opencode.jsonc` `instructions`).
 * This extension is the pi counterpart of that committed opencode loader: it injects a personal,
 * gitignored `AGENTS.local.md` into the system prompt — the same place pi puts `AGENTS.md`.
 *
 * Behaviour: read-only, side-effect-free, fail-open. If no `AGENTS.local.md` is found it is a no-op
 * (safe to commit for every contributor, whether or not they keep a local file). Discovery is
 * anchored to the project: it walks up from the working directory to the nearest ancestor that
 * contains `AGENTS.md` and reads `AGENTS.local.md` beside it. That handles running pi from a
 * subdirectory, while guaranteeing it never reads a stray `AGENTS.local.md` from a parent *outside*
 * the repo — the local file is defined as the gitignored sibling of the committed `AGENTS.md`.
 *
 * Note for contributors: because pi auto-discovers this directory, this file runs automatically in
 * your pi sessions in this repo. It only ever reads `AGENTS.local.md` and appends it to the prompt.
 */

import { existsSync, readFileSync } from "node:fs";
import { dirname, join, parse } from "node:path";

// Minimal structural type — keeps the extension self-contained (no dependency on a package import
// path that can move between pi versions). pi strips types at load time; this is editor help only.
type PiApi = {
  on(
    event: "before_agent_start",
    handler: (
      event: { systemPrompt: string },
      ctx: { cwd: string },
    ) => { systemPrompt: string } | void,
  ): void;
};

const LOCAL_FILE = "AGENTS.local.md";
const ANCHOR_FILE = "AGENTS.md";

/**
 * Walk up from `start` to the nearest ancestor holding `AGENTS.md` (the project anchor) and return
 * the `AGENTS.local.md` beside it. Bounded to the project: it stops at the anchor, so a local file
 * in a parent directory *outside* the repo can never leak in. Returns nothing if no anchor is found.
 */
function findLocalInstructions(start: string): string | undefined {
  let dir = start;
  const rootDir = parse(dir).root;
  for (;;) {
    if (existsSync(join(dir, ANCHOR_FILE))) {
      // Anchor found — read the sibling local file here and stop (never escape the project root).
      try {
        const text = readFileSync(join(dir, LOCAL_FILE), "utf8").trim();
        if (text) return text;
      } catch {
        // No readable AGENTS.local.md beside the anchor → inject nothing.
      }
      return undefined;
    }
    if (dir === rootDir) return undefined;
    dir = dirname(dir);
  }
}

// pi's loader calls the default export with the ExtensionAPI object.
export default function loadAgentsLocal(pi: PiApi): void {
  pi.on("before_agent_start", (event, ctx) => {
    const local = findLocalInstructions(ctx.cwd);
    if (!local) return; // no personal file → no-op

    // `event.systemPrompt` is rebuilt fresh each turn, so this replaces (not accumulates) and is
    // chain-safe with other extensions that also return a systemPrompt.
    return {
      systemPrompt: `${event.systemPrompt}\n\n# ${LOCAL_FILE} (personal, machine-local)\n\n${local}`,
    };
  });
}
