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
    fs.writeSync(
      fh,
      `## ${nowHHMM()} — ${context}\n${summary.trim()}\n- ${tag}: ${summary.trim()}\n\n`,
    );
  } finally {
    fs.closeSync(fh);
  }
}
