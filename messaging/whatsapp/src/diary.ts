import fs from "node:fs";
import path from "node:path";

export type DiaryTag = "FACT" | "FEEDBACK" | "OBSERVATION" | "NOTABLE";

const AGENT = "whatsapp";

function todayPath(diaryRoot: string): string {
  const date = new Date().toISOString().slice(0, 10);
  return path.join(diaryRoot, AGENT, `${date}.md`);
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

function nowHHMM(): string {
  const d = new Date();
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** Replace personal identifiers with placeholders before a line is written.
 *  Mirrors core's `diary::redact`: diaries feed every autonomous writer, so a
 *  JID or phone number here is one hop from T2 memory, the vault or a skill.
 *  Covered: WhatsApp JIDs (`<digits>[:n]@s.whatsapp.net` / `@lid` / group
 *  `@g.us`), emails, bare 10–13 digit phone numbers, home directories. */
export function redact(text: string): string {
  return text
    .replace(/\b\d{5,20}(?:-\d{5,20})?(?::\d{1,3})?@(?:s\.whatsapp\.net|lid|g\.us)\b/g, "<jid>")
    .replace(/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, "<email>")
    .replace(/(?<![\w.-])\+?\d{10,13}(?![\w.-])/g, "<phone>")
    .replace(/\/(?:Users|home)\/[^/\s'"]+/g, "~");
}

export function record(
  diaryRoot: string,
  context: string,
  summary: string,
  tag: DiaryTag = "OBSERVATION",
): void {
  const filePath = todayPath(diaryRoot);
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  const newFile = !fs.existsSync(filePath);
  const fh = fs.openSync(filePath, "a");
  try {
    if (newFile) {
      const date = new Date().toISOString().slice(0, 10);
      fs.writeSync(fh, `---\nagent: ${AGENT}\ndate: ${date}\n---\n\n`);
    }
    const clean = redact(summary.trim());
    fs.writeSync(fh, `## ${nowHHMM()} — ${redact(context)}\n${clean}\n- ${tag}: ${clean}\n\n`);
  } finally {
    fs.closeSync(fh);
  }
}

/** Append a context + summary without a tagged-bullet line. Used by the
 *  daily rotation, where the summary IS the body — duplicating it as a
 *  tag bullet would just be noise. */
export function appendEntry(
  diaryRoot: string,
  context: string,
  summary: string,
): void {
  const filePath = todayPath(diaryRoot);
  fs.mkdirSync(path.dirname(filePath), { recursive: true });
  const newFile = !fs.existsSync(filePath);
  const fh = fs.openSync(filePath, "a");
  try {
    if (newFile) {
      const date = new Date().toISOString().slice(0, 10);
      fs.writeSync(fh, `---\nagent: ${AGENT}\ndate: ${date}\n---\n\n`);
    }
    fs.writeSync(fh, `## ${nowHHMM()} — ${redact(context)}\n${redact(summary.trim())}\n\n`);
  } finally {
    fs.closeSync(fh);
  }
}
