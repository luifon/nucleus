#!/usr/bin/env node
// playwright-auth — owns the shared Playwright storage state (ADR-022).
//
// Normal Nucleus sessions run @playwright/mcp with --isolated and seed their
// ephemeral browser context from ~/.nucleus/playwright-storage.json. That file
// is written ONLY by this tool, from the persistent auth profile at
// ~/.nucleus/playwright-profile — the one place logins live.
//
//   playwright-auth init                    ensure an (empty) storage state exists
//   playwright-auth capture [--origins a,b] export profile cookies/localStorage/
//                                           IndexedDB to the storage state file
//   playwright-auth login --url <url>       headed browser on the auth profile;
//                                           log in, close the window, state is
//                                           captured automatically
//
// Options: --profile <dir>  (default $HOME/.nucleus/playwright-profile)
//          --out <file>     (default $HOME/.nucleus/playwright-storage.json)
//
// localStorage/IndexedDB only export for origins the context visited, so
// token-in-localStorage sites must be listed via --origins (login implies its
// --url origin). Cookie-auth sites need nothing.

import { chromium } from 'playwright-core';
import { existsSync, mkdirSync, writeFileSync, chmodSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';

function parseArgs(argv) {
  const args = { _: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) args[a.slice(2)] = argv[++i] ?? '';
    else args._.push(a);
  }
  return args;
}

const args = parseArgs(process.argv.slice(2));
const cmd = args._[0] ?? 'capture';
const profileDir = args.profile ?? path.join(homedir(), '.nucleus', 'playwright-profile');
const outFile = args.out ?? path.join(homedir(), '.nucleus', 'playwright-storage.json');

function writeState(state) {
  mkdirSync(path.dirname(outFile), { recursive: true });
  writeFileSync(outFile, JSON.stringify(state, null, 2) + '\n');
  chmodSync(outFile, 0o600); // session cookies — operator-only
  const nCookies = state.cookies?.length ?? 0;
  const nOrigins = state.origins?.length ?? 0;
  console.log(`wrote ${outFile} (${nCookies} cookies, ${nOrigins} origins)`);
}

async function openProfile({ headless }) {
  try {
    return await chromium.launchPersistentContext(profileDir, {
      channel: 'chrome',
      headless,
    });
  } catch (err) {
    if (String(err).includes('ProcessSingleton') || String(err).includes('SingletonLock')) {
      console.error(
        `auth profile is in use (${profileDir}).\n` +
          `Only playwright-auth should open it — normal sessions run --isolated (ADR-022).\n` +
          `Close the browser holding it and retry.`
      );
      process.exit(2);
    }
    throw err;
  }
}

async function capture(origins) {
  const ctx = await openProfile({ headless: true });
  const page = ctx.pages()[0] ?? (await ctx.newPage());
  for (const origin of origins) {
    try {
      await page.goto(origin, { waitUntil: 'domcontentloaded', timeout: 30_000 });
      // let SPA auth bootstrapping settle so tokens land in storage
      await page.waitForTimeout(2_000);
    } catch (err) {
      console.warn(`warn: could not visit ${origin}: ${String(err).split('\n')[0]}`);
    }
  }
  let state;
  try {
    state = await ctx.storageState({ indexedDB: true });
  } catch {
    state = await ctx.storageState();
  }
  await ctx.close();
  writeState(state);
}

const originOf = (url) => new URL(url).origin;

switch (cmd) {
  case 'init': {
    if (existsSync(outFile)) {
      console.log(`${outFile} already exists — leaving it alone`);
      break;
    }
    writeState({ cookies: [], origins: [] });
    break;
  }

  case 'capture': {
    const origins = (args.origins ?? '').split(',').filter(Boolean);
    await capture(origins);
    break;
  }

  case 'login': {
    if (!args.url) {
      console.error('login requires --url <url>');
      process.exit(1);
    }
    const ctx = await openProfile({ headless: false });
    const page = ctx.pages()[0] ?? (await ctx.newPage());
    await page.goto(args.url, { waitUntil: 'domcontentloaded', timeout: 60_000 });
    console.log(
      'Log in in the opened window. Login is detected automatically — the\n' +
        'window closes itself once the session is captured. (Closing it\n' +
        'manually also works.)',
    );
    // Two exit signals, whichever comes first:
    //  1. auto-detect: an open page sits on the target origin, off any
    //     login/signin path, stable across two consecutive polls — then a
    //     grace period for token writes;
    //  2. the operator closes the window (context 'close') — works where
    //     the platform ends the browser with its last window. On macOS
    //     Chromium OUTLIVES its windows, so the pre-fix wait-for-close
    //     never fired and a manual kill could nuke the just-created
    //     session before it flushed (lost a login, 2026-07-20).
    const target = originOf(args.url);
    const looksLoggedIn = () =>
      ctx.pages().some((pg) => {
        try {
          const u = new URL(pg.url());
          return u.origin === target && !/login|signin|sign-in|auth/i.test(u.pathname);
        } catch {
          return false;
        }
      });
    let closed = false;
    ctx.on('close', () => {
      closed = true;
    });
    const deadline = Date.now() + 10 * 60_000;
    let stable = 0;
    while (!closed && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 3_000));
      stable = looksLoggedIn() ? stable + 1 : 0;
      if (stable >= 2) {
        console.log('login detected — capturing in 8s…');
        await new Promise((r) => setTimeout(r, 8_000)); // let token writes settle
        break;
      }
    }
    if (!closed) {
      await ctx.close(); // clean shutdown flushes cookies/localStorage
    }
    await capture([target, ...(args.origins ?? '').split(',').filter(Boolean)]);
    break;
  }

  default:
    console.error(`unknown command: ${cmd} (expected init | capture | login)`);
    process.exit(1);
}
