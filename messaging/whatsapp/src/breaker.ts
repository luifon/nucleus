/** Connection circuit breaker (ADR-027).
 *
 *  The inner supervision layer between Baileys and launchd: transient
 *  closes reconnect in-process on an exponential ladder instead of the old
 *  fixed 2s/5s; a failure storm opens the circuit (stop hammering, hold
 *  the outbound queue, alert ONCE via Discord — the independent channel)
 *  and probes half-open on an interval. launchd stays the outer layer for
 *  crashes; this class never exits the process and NEVER touches auth
 *  state (Rule 8: no logout, no re-pair, no auth-dir writes).
 *
 *  Every close is classified and recorded (`connection_events` in
 *  whatsapp.db) — the churn-diagnosis dataset ADR-027 wants, populated
 *  passively. Pure state machine, clock injected, unit-tested.
 */

export interface BreakerConfig {
  /** Reconnect delays, indexed by consecutive-failure count (last repeats). */
  ladderMs: number[];
  /** A connection that stayed open at least this long resets the ladder. */
  stableMs: number;
  /** Failures within `windowMs` that open the circuit. */
  openThreshold: number;
  windowMs: number;
  /** Half-open probe interval while the circuit is open. */
  probeMs: number;
  /** Recovery from an outage longer than this alerts the operator. */
  alertAfterOutageMs: number;
}

export const DEFAULT_BREAKER: BreakerConfig = {
  ladderMs: [1_000, 2_000, 5_000, 15_000, 60_000, 300_000],
  stableMs: 120_000,
  openThreshold: 10,
  windowMs: 15 * 60_000,
  probeMs: 5 * 60_000,
  alertAfterOutageMs: 30 * 60_000,
};

/** Baileys DisconnectReason → taxonomy class. Only loggedOut is
 *  non-reconnectable: the device was unlinked and ONLY the operator can
 *  re-pair (Rule 8). Everything else goes through the ladder. */
export function classifyClose(code: number | undefined): { cls: string; reconnectable: boolean } {
  switch (code) {
    case 401:
      return { cls: "logged-out", reconnectable: false };
    case 403:
      return { cls: "forbidden", reconnectable: true };
    case 405:
      return { cls: "login-405", reconnectable: true };
    case 408:
      return { cls: "timed-out", reconnectable: true };
    case 411:
      return { cls: "multidevice-mismatch", reconnectable: true };
    case 428:
      return { cls: "connection-closed", reconnectable: true };
    case 440:
      return { cls: "connection-replaced", reconnectable: true };
    case 500:
      return { cls: "bad-session", reconnectable: true };
    case 503:
      return { cls: "service-unavailable", reconnectable: true };
    case 515:
      return { cls: "restart-required", reconnectable: true };
    default:
      return { cls: code === undefined ? "unknown" : `code-${code}`, reconnectable: true };
  }
}

export type Decision =
  /** schedule connect() in delayMs */
  | { action: "reconnect"; delayMs: number }
  /** circuit open: schedule a half-open probe; alert iff justOpened */
  | { action: "open-circuit"; probeMs: number; justOpened: boolean }
  /** device unlinked — hold everything, operator must re-pair */
  | { action: "hold" };

export interface CloseOutcome {
  cls: string;
  uptimeMs: number | null;
  decision: Decision;
}

export interface OpenOutcome {
  /** true when this open ends an open-circuit outage */
  recovered: boolean;
  outageMs: number;
}

export class ConnectionSupervisor {
  private cfg: BreakerConfig;
  private now: () => number;
  private connectedAt: number | null = null;
  private consecutive = 0;
  private failureTimes: number[] = [];
  private circuitOpen = false;
  private openedAt = 0;

  constructor(cfg: BreakerConfig = DEFAULT_BREAKER, now: () => number = () => Date.now()) {
    this.cfg = cfg;
    this.now = now;
  }

  get isOpen(): boolean {
    return this.circuitOpen;
  }

  onOpen(): OpenOutcome {
    const t = this.now();
    this.connectedAt = t;
    if (this.circuitOpen) {
      const outageMs = t - this.openedAt;
      this.circuitOpen = false;
      // Recovery clears the FAILURE WINDOW (re-opening requires a fresh
      // storm, so a flap right after recovery can't instantly re-alert)
      // but keeps the LADDER rung — a half-open success that dies in
      // seconds goes back to long delays, not to 1s hammering. Only a
      // stable stretch (onClose with uptime ≥ stableMs) resets the rung.
      this.failureTimes = [];
      return { recovered: true, outageMs };
    }
    return { recovered: false, outageMs: 0 };
  }

  onClose(code: number | undefined): CloseOutcome {
    const t = this.now();
    const { cls, reconnectable } = classifyClose(code);
    const uptimeMs = this.connectedAt !== null ? t - this.connectedAt : null;
    this.connectedAt = null;

    if (!reconnectable) {
      return { cls, uptimeMs, decision: { action: "hold" } };
    }

    // A stable stretch forgives history — churny days shouldn't
    // accumulate toward an open circuit across healthy hours.
    if (uptimeMs !== null && uptimeMs >= this.cfg.stableMs) {
      this.consecutive = 0;
      this.failureTimes = [];
    }

    this.failureTimes.push(t);
    const cutoff = t - this.cfg.windowMs;
    this.failureTimes = this.failureTimes.filter((ft) => ft >= cutoff);

    if (this.circuitOpen) {
      // half-open probe died — stay open, keep probing
      return {
        cls,
        uptimeMs,
        decision: { action: "open-circuit", probeMs: this.cfg.probeMs, justOpened: false },
      };
    }

    if (this.failureTimes.length >= this.cfg.openThreshold) {
      this.circuitOpen = true;
      this.openedAt = t;
      return {
        cls,
        uptimeMs,
        decision: { action: "open-circuit", probeMs: this.cfg.probeMs, justOpened: true },
      };
    }

    const idx = Math.min(this.consecutive, this.cfg.ladderMs.length - 1);
    this.consecutive += 1;
    return { cls, uptimeMs, decision: { action: "reconnect", delayMs: this.cfg.ladderMs[idx] } };
  }
}
