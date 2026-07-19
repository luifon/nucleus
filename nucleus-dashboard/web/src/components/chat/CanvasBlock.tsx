import { useState } from "react";
import {
  type CanvasBlockData,
  type CanvasResponseValue,
  buildResponse,
} from "@/lib/canvas";

/**
 * Renderer for one canvas block (ADR-012). `answered` disables the whole
 * widget — derived from message history upstream, never local-only, so a
 * reloaded transcript renders identically to the live one.
 */
export default function CanvasBlock({
  block,
  answered,
  disabled,
  onSubmit,
}: {
  block: CanvasBlockData;
  answered: boolean;
  /** true while any send is in flight — prevents double-submits. */
  disabled: boolean;
  onSubmit: (message: string) => void;
}) {
  const inert = answered || disabled;
  const submit = (value: CanvasResponseValue) => onSubmit(buildResponse(block, value));

  return (
    <div
      className={[
        "my-2 rounded border px-3 py-2",
        "border-[var(--color-nucleus-accent)]",
        "bg-[color-mix(in_srgb,var(--color-nucleus-accent)_5%,var(--color-nucleus-surface))]",
        answered ? "opacity-60" : "",
      ].join(" ")}
    >
      <div className="mb-2 flex items-center gap-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
        <span>⛶ {block.type}</span>
        {block.title && <span className="normal-case tracking-normal text-[var(--color-nucleus-text)]">{block.title}</span>}
        {answered && <span className="ml-auto">answered</span>}
      </div>
      {block.type === "decision" && <Decision block={block} inert={inert} submit={submit} />}
      {(block.type === "multi-select" || block.type === "review") && (
        <MultiSelect block={block} inert={inert} submit={submit} />
      )}
      {block.type === "confirm" && <Confirm block={block} inert={inert} submit={submit} />}
      {block.type === "form" && <Form block={block} inert={inert} submit={submit} />}
    </div>
  );
}

/** Malformed / unsupported blocks: visible, inert, raw payload collapsed. */
export function CanvasFallback({ reason, raw }: { reason: string; raw: string }) {
  return (
    <div className="my-2 rounded border border-[var(--color-nucleus-border)] px-3 py-2 text-xs text-[var(--color-nucleus-faint)]">
      <div>⛶ canvas block not rendered — {reason}</div>
      <details className="mt-1">
        <summary className="cursor-pointer">raw</summary>
        <pre className="mt-1 whitespace-pre-wrap">{raw}</pre>
      </details>
    </div>
  );
}

const BTN =
  "rounded border px-2 py-1 text-sm border-[var(--color-nucleus-border)] " +
  "hover:border-[var(--color-nucleus-accent)] disabled:cursor-not-allowed disabled:opacity-50";

function Decision({
  block,
  inert,
  submit,
}: {
  block: CanvasBlockData;
  inert: boolean;
  submit: (v: CanvasResponseValue) => void;
}) {
  return (
    <div className="flex flex-wrap gap-2">
      {(block.options ?? []).map((o) => (
        <button
          key={o.key}
          className={BTN}
          disabled={inert}
          title={o.hint}
          onClick={() => submit({ choice: o.key })}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

function MultiSelect({
  block,
  inert,
  submit,
}: {
  block: CanvasBlockData;
  inert: boolean;
  submit: (v: CanvasResponseValue) => void;
}) {
  const options = block.options ?? [];
  const [checked, setChecked] = useState<Set<string>>(
    () => new Set(options.filter((o) => o.checked !== false).map((o) => o.key)),
  );
  const toggle = (key: string) =>
    setChecked((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  return (
    <div className="flex flex-col gap-1">
      {options.map((o) => (
        <label key={o.key} className="flex cursor-pointer items-baseline gap-2 text-sm">
          <input
            type="checkbox"
            checked={checked.has(o.key)}
            disabled={inert}
            onChange={() => toggle(o.key)}
            className="accent-[var(--color-nucleus-accent)]"
          />
          <span>
            {o.label}
            {o.detail && (
              <span className="block text-xs text-[var(--color-nucleus-faint)]">{o.detail}</span>
            )}
          </span>
        </label>
      ))}
      <div>
        <button
          className={BTN + " mt-1"}
          disabled={inert}
          onClick={() =>
            submit({
              selected: options.filter((o) => checked.has(o.key)).map((o) => o.key),
              unselected: options.filter((o) => !checked.has(o.key)).map((o) => o.key),
            })
          }
        >
          submit
        </button>
      </div>
    </div>
  );
}

function Confirm({
  block,
  inert,
  submit,
}: {
  block: CanvasBlockData;
  inert: boolean;
  submit: (v: CanvasResponseValue) => void;
}) {
  return (
    <div className="flex flex-col gap-2">
      {block.prompt && <div className="text-sm">{block.prompt}</div>}
      <div className="flex gap-2">
        <button
          className={
            BTN +
            (block.danger
              ? " border-red-700 text-red-400 hover:border-red-500"
              : " border-[var(--color-nucleus-accent)]")
          }
          disabled={inert}
          onClick={() => submit({ confirmed: true })}
        >
          {block.danger ? "confirm (destructive)" : "confirm"}
        </button>
        <button className={BTN} disabled={inert} onClick={() => submit({ confirmed: false })}>
          cancel
        </button>
      </div>
    </div>
  );
}

function Form({
  block,
  inert,
  submit,
}: {
  block: CanvasBlockData;
  inert: boolean;
  submit: (v: CanvasResponseValue) => void;
}) {
  const fields = block.fields ?? [];
  const [values, setValues] = useState<Record<string, string>>(() =>
    Object.fromEntries(fields.map((f) => [f.key, f.value ?? ""])),
  );
  return (
    <div className="flex flex-col gap-2">
      {fields.map((f) => (
        <label key={f.key} className="flex flex-col gap-1 text-sm">
          <span className="text-xs text-[var(--color-nucleus-faint)]">{f.label}</span>
          <input
            type={f.kind ?? "text"}
            value={values[f.key] ?? ""}
            placeholder={f.placeholder}
            disabled={inert}
            onChange={(e) => setValues((prev) => ({ ...prev, [f.key]: e.target.value }))}
            className="rounded border border-[var(--color-nucleus-border)] bg-transparent px-2 py-1 text-sm outline-none focus:border-[var(--color-nucleus-accent)]"
          />
        </label>
      ))}
      <div>
        <button className={BTN} disabled={inert} onClick={() => submit({ values })}>
          submit
        </button>
      </div>
    </div>
  );
}
