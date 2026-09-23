import { useEffect, useRef } from "react";

// Inline confirmation strip for destructive row actions. Rendered inside
// the row it confirms, in place of the browser's native confirmation dialog. Focus
// moves to the confirm button when the strip opens; Escape cancels.
// While `busy`, both buttons and Escape are inert so the operator cannot
// dismiss the strip or fire the action twice mid-request.

export default function InlineConfirm({
  message,
  confirmLabel,
  cancelLabel = "keep",
  busy = false,
  onConfirm,
  onCancel,
  className = "px-4 py-2",
}: {
  /** States what the action does, e.g. "Delete chat "x"? This can't be undone." */
  message: string;
  confirmLabel: string;
  cancelLabel?: string;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
  /** Padding for the strip; the host row decides its own inset. */
  className?: string;
}) {
  const confirmRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    confirmRef.current?.focus();
  }, []);

  return (
    <div
      role="alertdialog"
      aria-label={message}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        if (e.key === "Escape" && !busy) {
          e.stopPropagation();
          onCancel();
        }
      }}
      className={`flex flex-wrap items-center gap-2 border-t border-[var(--color-status-down)] text-xs ${className}`}
    >
      <span className="min-w-0 flex-1 text-[var(--color-status-down)]">{message}</span>
      <button
        ref={confirmRef}
        onClick={onConfirm}
        disabled={busy}
        className="rounded border border-[var(--color-status-down)] px-2 py-0.5 text-[var(--color-status-down)] transition-colors hover:bg-[var(--color-nucleus-bg)] disabled:opacity-40"
      >
        {confirmLabel}
      </button>
      <button
        onClick={onCancel}
        disabled={busy}
        className="rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[var(--color-nucleus-faint)] transition-colors hover:text-[var(--color-nucleus-text)] disabled:opacity-40"
      >
        {cancelLabel}
      </button>
    </div>
  );
}
