import { type ReactNode } from "react";

// Common page layout — sets max-width, padding, and stacks an optional
// title block above the page body. Every page should wrap its content
// in this so margins/widths stay consistent.
export default function PageShell({
  title,
  subtitle,
  actions,
  children,
}: {
  title?: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="mx-auto max-w-6xl px-8 py-7">
      {(title || actions || subtitle) && (
        <header className="mb-7 flex items-start justify-between gap-6">
          <div className="min-w-0">
            {title && <h1 className="text-2xl leading-tight">{title}</h1>}
            {subtitle && (
              <p className="mt-1.5 max-w-2xl text-sm leading-relaxed text-[var(--color-nucleus-faint)]">
                {subtitle}
              </p>
            )}
          </div>
          {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
        </header>
      )}
      {children}
    </div>
  );
}
