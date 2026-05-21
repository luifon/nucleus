// Direct Discord REST helper for one-line alerts from the WhatsApp bot.
// Mirrors core/src/discord_sdk.rs::send_announcement in shape: same endpoint,
// same headers, suppresses URL embeds. Used by the connection-rot watchdog
// to post a heads-up before process.exit(1) so the operator finds out via
// Discord, not just by noticing failed reminder deliveries.
//
// Logs but never throws — callers (notably the exit path) shouldn't fail
// because Discord is also misbehaving.

import pino from "pino";

const log = pino({ level: process.env.NUCLEUS_LOG ?? "info" });

const SUPPRESS_EMBEDS = 4;

export async function alertDiscordHome(body: string): Promise<void> {
  const token = process.env.DISCORD_BOT_TOKEN;
  const channelId = process.env.DISCORD_HOME_CHANNEL_ID;
  if (!token || !channelId) {
    log.warn(
      { hasToken: !!token, hasChannel: !!channelId },
      "discord_alert: skipping — DISCORD_BOT_TOKEN or DISCORD_HOME_CHANNEL_ID unset",
    );
    return;
  }
  try {
    const res = await fetch(
      `https://discord.com/api/v10/channels/${channelId}/messages`,
      {
        method: "POST",
        headers: {
          "Authorization": `Bot ${token}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ content: body, flags: SUPPRESS_EMBEDS }),
      },
    );
    if (!res.ok) {
      const text = await res.text().catch(() => "");
      log.warn(
        { status: res.status, text: text.slice(0, 200) },
        "discord_alert: POST failed",
      );
      return;
    }
    log.info("discord_alert: posted");
  } catch (e) {
    log.warn({ err: (e as Error).message }, "discord_alert: fetch threw");
  }
}
