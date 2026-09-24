// ts-rs writes a space before each line break that follows a doc comment
// in a generated type (`{ \n/**`). Strip trailing whitespace from every
// generated file so `git diff --check` stays clean. Run by
// `npm run generate:api` after the export; never edit generated files by hand.
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const dir = new URL("../src/lib/api/generated/", import.meta.url).pathname;
for (const name of readdirSync(dir)) {
  if (!name.endsWith(".ts")) continue;
  const path = join(dir, name);
  const text = readFileSync(path, "utf8");
  const clean = text.replace(/[ \t]+$/gm, "");
  if (clean !== text) writeFileSync(path, clean);
}
