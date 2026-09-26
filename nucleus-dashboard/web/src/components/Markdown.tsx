import { useMemo, type ReactNode } from "react";
import { parseMarkdown, type Block, type Inline } from "@/lib/markdown";

// Renders agent-written Markdown (lib/markdown.ts) in the dashboard's
// type and colours. Everything is React text: no HTML from the source is
// injected. Wide content (code, tables) scrolls sideways inside its own
// box so a phone-width column never overflows.

export default function Markdown({ source, className = "" }: { source: string; className?: string }) {
  const blocks = useMemo(() => parseMarkdown(source), [source]);
  return <div className={`space-y-2.5 break-words text-sm leading-relaxed text-[var(--color-nucleus-text)] ${className}`}>{renderBlocks(blocks)}</div>;
}

function renderBlocks(blocks: Block[]): ReactNode[] {
  return blocks.map((b, k) => renderBlock(b, k));
}

const HEADING_CLASS: Record<number, string> = {
  1: "text-base text-[var(--color-nucleus-accent)]",
  2: "text-sm text-[var(--color-nucleus-accent)]",
  3: "text-sm text-[var(--color-nucleus-text)]",
};

function renderBlock(b: Block, key: number): ReactNode {
  switch (b.t) {
    case "heading": {
      const cls = `${HEADING_CLASS[b.level] ?? "text-sm text-[var(--color-nucleus-faint)]"} font-semibold pt-1`;
      const Tag = `h${Math.min(b.level + 1, 6)}` as "h2";
      return (
        <Tag key={key} className={cls}>
          {renderInline(b.c)}
        </Tag>
      );
    }
    case "para":
      return <p key={key}>{renderInline(b.c)}</p>;
    case "code":
      return (
        <pre
          key={key}
          className="overflow-x-auto rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 text-xs leading-relaxed"
          data-lang={b.lang || undefined}
        >
          <code>{b.v}</code>
        </pre>
      );
    case "quote":
      return (
        <blockquote key={key} className="space-y-2 border-l-2 border-[var(--color-nucleus-border)] pl-3 text-[var(--color-nucleus-faint)]">
          {renderBlocks(b.blocks)}
        </blockquote>
      );
    case "list": {
      const items = b.items.map((it, k) => (
        <li key={k} className="space-y-1.5 pl-1">
          {it.checked !== null && (
            <span className={it.checked ? "text-[var(--color-status-ok)]" : "text-[var(--color-nucleus-faint)]"}>{it.checked ? "[x] " : "[ ] "}</span>
          )}
          {renderListItemBlocks(it.blocks)}
        </li>
      ));
      return b.ordered ? (
        <ol key={key} start={b.start} className="list-decimal space-y-1 pl-6 marker:text-[var(--color-nucleus-faint)]">
          {items}
        </ol>
      ) : (
        <ul key={key} className="list-disc space-y-1 pl-5 marker:text-[var(--color-nucleus-accent)]">
          {items}
        </ul>
      );
    }
    case "table":
      return (
        <div key={key} className="overflow-x-auto">
          <table className="w-full border-collapse text-xs">
            <thead>
              <tr>
                {b.head.map((c, k) => (
                  <th
                    key={k}
                    style={{ textAlign: b.align[k] ?? "left" }}
                    className="border-b border-[var(--color-nucleus-border)] px-2 py-1 font-normal text-[var(--color-nucleus-faint)]"
                  >
                    {renderInline(c)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {b.rows.map((row, r) => (
                <tr key={r}>
                  {row.map((c, k) => (
                    <td key={k} style={{ textAlign: b.align[k] ?? "left" }} className="border-b border-[var(--color-nucleus-border)] px-2 py-1 align-top">
                      {renderInline(c)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    case "hr":
      return <hr key={key} className="border-[var(--color-nucleus-border)]" />;
  }
}

/** A list item whose only block is a paragraph renders inline, so the
 *  text sits on the marker's line. */
function renderListItemBlocks(blocks: Block[]): ReactNode {
  if (blocks.length > 0 && blocks[0].t === "para") {
    return (
      <>
        {renderInline(blocks[0].c)}
        {renderBlocks(blocks.slice(1))}
      </>
    );
  }
  return renderBlocks(blocks);
}

function renderInline(nodes: Inline[]): ReactNode[] {
  return nodes.map((n, k) => {
    switch (n.t) {
      case "text":
        return n.v;
      case "code":
        return (
          <code key={k} className="rounded bg-[var(--color-nucleus-bg)] px-1 py-px text-[0.9em] text-[var(--color-nucleus-accent)]">
            {n.v}
          </code>
        );
      case "strong":
        return (
          <strong key={k} className="font-semibold">
            {renderInline(n.c)}
          </strong>
        );
      case "em":
        return <em key={k}>{renderInline(n.c)}</em>;
      case "del":
        return (
          <del key={k} className="text-[var(--color-nucleus-faint)]">
            {renderInline(n.c)}
          </del>
        );
      case "link":
        return (
          <a key={k} href={n.href} target="_blank" rel="noreferrer" className="break-all text-[var(--color-nucleus-accent)] underline-offset-2 hover:underline">
            {renderInline(n.c)}
          </a>
        );
      case "br":
        return <br key={k} />;
    }
  });
}
