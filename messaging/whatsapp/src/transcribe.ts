// Voice memo transcription via whisper.cpp running locally.
//
// Pipeline:
//   buffer (OGG/Opus from WhatsApp) → ffmpeg → 16kHz mono WAV → whisper-cli → text
//
// Privacy: audio never leaves the machine.

import { exec } from "node:child_process";
import { promisify } from "node:util";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";

const execAsync = promisify(exec);

const DEFAULT_MODEL = path.join(
  os.homedir(),
  ".cache/whisper/models/ggml-large-v3.bin",
);
const DEFAULT_BINARY = "whisper-cli";

export interface TranscribeResult {
  text: string;
  durationMs: number;
}

export async function transcribe(audioBuffer: Buffer): Promise<TranscribeResult> {
  const t0 = Date.now();
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "nucleus-audio-"));
  const oggPath = path.join(dir, "input.ogg");
  const wavPath = path.join(dir, "input.wav");

  try {
    await fs.writeFile(oggPath, audioBuffer);

    // Normalize to 16kHz mono PCM WAV — what whisper expects.
    await execAsync(
      `ffmpeg -y -i "${oggPath}" -ar 16000 -ac 1 -c:a pcm_s16le "${wavPath}" 2>&1`,
      { maxBuffer: 16 * 1024 * 1024 },
    );

    const model = process.env.WHISPER_MODEL_PATH ?? DEFAULT_MODEL;
    const binary = process.env.WHISPER_BINARY ?? DEFAULT_BINARY;

    // -l auto: detect language (PT for BR voice memos, EN for English, etc.)
    // -nt:    suppress timestamps in output
    // -np:    no print extras / progress
    // -otxt:  write transcript to <wav>.txt
    await execAsync(
      `${binary} -m "${model}" -l auto -nt -np -f "${wavPath}" -otxt 2>&1`,
      { maxBuffer: 16 * 1024 * 1024 },
    );

    const txt = await fs.readFile(`${wavPath}.txt`, "utf-8");
    return { text: txt.trim(), durationMs: Date.now() - t0 };
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
}
