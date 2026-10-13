// The log of every build, one per area, fed by the same events that drive the progress bars.
//
// It used to live inside the Build view as a flat array of strings, rebuilt into one text node
// on every line: scrolling up to read something put you back at the bottom a second later, and
// it could only be seen from one tab. It is its own module now so that the Build view, the strip
// that is visible from every tab, and anything added later read one buffer.
//
// Lines arrive in bursts - a tool that warns once per tile prints thousands in a second - so they
// are queued and folded into the visible list a few times a second, and identical consecutive
// lines are counted instead of repeated.
import { listen } from "./api.js";

/** Lines kept per area. An hour of planetiler at one line a second is 3600. */
const MAX = 30000;
/** How long arriving lines are held back, so a burst is one update rather than a thousand. */
const FLUSH_MS = 150;

const ANSI = /\x1b\[[0-9;]*[a-zA-Z]/g;
/** A leading timestamp, which makes two otherwise identical lines look different. */
const STAMP = /^\s*(\d{4}[-/]\d\d[-/]\d\d[ T])?\d\d:\d\d:\d\d(\.\d+)?\s+/;

export function levelOf(text) {
  if (/\b(ERROR|SEVERE|FATAL|FAILED|panicked)\b|\[ERROR\]/.test(text)) return "error";
  if (/\b(WARN|WARNING|warning)\b|\[WARN\]/.test(text)) return "warn";
  return "info";
}

class AreaLog {
  /** Replaced, never mutated: a few thousand rows are not worth a deep proxy. */
  lines = $state.raw([]);
  /** When anything last happened in this area's run - a log line or a progress reading. */
  activityAt = $state(0);
  seq = 0;
  /** Lines received and not yet folded into `lines`. */
  pending = [];
  timer = null;
}

/** area name -> AreaLog */
export const logs = $state({});

export function logFor(area) {
  if (!logs[area]) logs[area] = new AreaLog();
  return logs[area];
}

const NONE = [];

/** An area's lines, without creating its log: safe to call while rendering. */
export function linesOf(area) {
  return logs[area]?.lines ?? NONE;
}

/** When an area's run last did anything, or 0. Safe to call while rendering. */
export function activityOf(area) {
  return logs[area]?.activityAt ?? 0;
}

function fold(log) {
  log.timer = null;
  if (!log.pending.length) return;
  const next = log.lines.slice();
  for (const incoming of log.pending) {
    const last = next[next.length - 1];
    if (last && last.key === incoming.key && last.step === incoming.step && last.level === incoming.level) {
      next[next.length - 1] = { ...last, count: last.count + 1, at: incoming.at };
    } else {
      next.push(incoming);
    }
  }
  log.pending = [];
  log.lines = next.length > MAX ? next.slice(next.length - MAX) : next;
}

/** Add one line to an area's log. `level` overrides what the text looks like. */
export function pushLog(area, text, { step = "", level = null, at = Date.now() } = {}) {
  const log = logFor(area);
  const clean = String(text).replace(ANSI, "").replace(/\r/g, "");
  if (!clean.trim()) return;
  log.pending.push({
    id: ++log.seq,
    step,
    text: clean,
    key: clean.replace(STAMP, ""),
    count: 1,
    at,
    level: level ?? levelOf(clean),
  });
  log.activityAt = at;
  if (!log.timer) log.timer = setTimeout(() => fold(log), FLUSH_MS);
}

/** Note that something happened without saying anything: a progress reading. */
export function touch(area, at = Date.now()) {
  logFor(area).activityAt = at;
}

export function clearLog(area) {
  const log = logFor(area);
  clearTimeout(log.timer);
  log.timer = null;
  log.pending = [];
  log.lines = [];
}

/** The log as text, for copying. */
export function logText(area, lines = linesOf(area)) {
  return lines.map((l) => (l.count > 1 ? `${l.text}  (x${l.count})` : l.text)).join("\n");
}

function format(ev) {
  switch (ev.event) {
    case "started":
      return [`${ev.step} started`, "mark"];
    case "phase":
      return [`${ev.name}`, "phase"];
    case "skipped":
      return [`${ev.step} skipped: ${ev.reason}`, "mark"];
    case "finished":
      return ev.ok
        ? [`${ev.step} finished${ev.elapsed ? ` in ${ev.elapsed}` : ""}`, "done"]
        : [`${ev.step} failed`, "error"];
    case "cli":
      return [`$ ${ev.line}`, "command"];
    case "command":
      return [`$ ${ev.argv.join(" ")}`, "command"];
    default:
      return null;
  }
}

/** Turn one step event into log lines. Progress readings are activity, not text. */
export function ingest(ev) {
  if (!ev?.area) return;
  if (ev.event === "log") {
    pushLog(ev.area, ev.line, { step: ev.step });
  } else if (ev.event === "progress") {
    touch(ev.area);
  } else {
    const made = format(ev);
    if (made) pushLog(ev.area, made[0], { step: ev.step ?? "", level: made[1] });
  }
}

/** Start reading events. Called once, from the app shell; returns the way to stop. */
export async function startLogging() {
  return listen("step", (e) => ingest(e.payload));
}
