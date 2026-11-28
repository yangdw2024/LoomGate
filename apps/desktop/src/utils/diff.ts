import { ForkDiffResult } from "../api/client";

export interface DiffLine {
  kind: "same" | "del" | "add";
  text: string;
  oldLine?: number;
  newLine?: number;
}

export function computeLineDiff(oldText: string, newText: string): DiffLine[] {
  const a = oldText.split(/\r?\n/);
  const b = newText.split(/\r?\n/);
  const m = a.length;
  const n = b.length;
  const dp: number[][] = Array.from({ length: m + 1 }, () => Array(n + 1).fill(0));
  for (let i = m - 1; i >= 0; i--) {
    for (let j = n - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const result: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < m && j < n) {
    if (a[i] === b[j]) {
      result.push({ kind: "same", text: a[i], oldLine: i + 1, newLine: j + 1 });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      result.push({ kind: "del", text: a[i], oldLine: i + 1 });
      i++;
    } else {
      result.push({ kind: "add", text: b[j], newLine: j + 1 });
      j++;
    }
  }
  while (i < m) {
    result.push({ kind: "del", text: a[i], oldLine: i + 1 });
    i++;
  }
  while (j < n) {
    result.push({ kind: "add", text: b[j], newLine: j + 1 });
    j++;
  }
  return result;
}

export function escapeXml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

export function buildShareCardSvg(
  diff: ForkDiffResult,
  origText: string,
  forkText: string,
  redacted: boolean,
  redactionsApplied: number
): string {
  const lines = computeLineDiff(origText, forkText).slice(0, 18);
  const width = 960;
  const headerH = 132;
  const rowH = 22;
  const height = headerH + Math.max(lines.length, 1) * rowH + 44;
  const origTotalIn = diff.original.input_tokens + diff.original.cache_read_tokens;
  const forkTotalIn = diff.forked.input_tokens + diff.forked.cache_read_tokens;
  const origHit = origTotalIn > 0 ? Math.round((diff.original.cache_read_tokens / origTotalIn) * 100) : 0;
  const forkHit = forkTotalIn > 0 ? Math.round((diff.forked.cache_read_tokens / forkTotalIn) * 100) : 0;
  const ttftSign = diff.ttft_delta_ms >= 0 ? "+" : "";
  const outSign = diff.output_tokens_delta >= 0 ? "+" : "";

  const diffRows = lines
    .map((dl, idx) => {
      const y = headerH + idx * rowH;
      const bg =
        dl.kind === "add"
          ? "rgba(63,185,80,0.18)"
          : dl.kind === "del"
          ? "rgba(248,81,73,0.18)"
          : "transparent";
      const fg = dl.kind === "add" ? "#aff5b4" : dl.kind === "del" ? "#ffdcd7" : "#8b949e";
      const sigil = dl.kind === "add" ? "+" : dl.kind === "del" ? "-" : " ";
      const text = (dl.text || " ").slice(0, 108);
      return `<rect x="24" y="${y}" width="${width - 48}" height="${rowH}" fill="${bg}"/>
        <text x="36" y="${y + 15}" fill="${fg}" font-family="ui-monospace,Consolas,monospace" font-size="12">${escapeXml(
        `${sigil} ${text}`
      )}</text>`;
    })
    .join("\n");

  const badge = redacted
    ? `Guardrail Redacted (${redactionsApplied})`
    : "Raw Local Export";

  return `<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">
  <rect width="${width}" height="${height}" rx="14" fill="#0d1117"/>
  <rect x="12" y="12" width="${width - 24}" height="${height - 24}" rx="10" fill="#161b22" stroke="#30363d"/>
  <text x="28" y="42" fill="#e6edf3" font-family="Segoe UI,Arial,sans-serif" font-size="18" font-weight="700">LoomGate Trajectory Fork &amp; Diff</text>
  <text x="${width - 28}" y="40" text-anchor="end" fill="#3fb950" font-family="Segoe UI,Arial,sans-serif" font-size="12">${escapeXml(badge)}</text>
  <text x="28" y="68" fill="#58a6ff" font-family="ui-monospace,Consolas,monospace" font-size="13">${escapeXml(
    `${diff.original.actual_model}  →  ${diff.forked.actual_model}`
  )}</text>
  <text x="28" y="92" fill="#8b949e" font-family="Segoe UI,Arial,sans-serif" font-size="12">${escapeXml(
    `TTFT: ${diff.original.ttft_ms}ms vs ${diff.forked.ttft_ms}ms (${ttftSign}${diff.ttft_delta_ms}ms)  |  Output: ${diff.original.output_tokens} vs ${diff.forked.output_tokens} tok (${outSign}${diff.output_tokens_delta})  |  KV Cache: ${origHit}% vs ${forkHit}%`
  )}</text>
  <rect x="24" y="${headerH - 14}" width="${width - 48}" height="${Math.max(lines.length, 1) * rowH + 20}" rx="6" fill="#090c10" stroke="#30363d"/>
  ${diffRows}
  <text x="28" y="${height - 18}" fill="#6e7681" font-family="Segoe UI,Arial,sans-serif" font-size="11">Generated locally by LoomGate Studio (zero raw prompts uploaded)</text>
</svg>`;
}

/** Pure Canvas 2D share card renderer (NO SVG foreignObject -> never taints Windows WebView2 canvas). */
export function renderShareCardPngDataUrl(
  diff: ForkDiffResult,
  origText: string,
  forkText: string,
  redacted: boolean,
  redactionsApplied: number
): string {
  const lines = computeLineDiff(origText, forkText).slice(0, 18);
  const width = 960;
  const headerH = 132;
  const rowH = 22;
  const height = headerH + Math.max(lines.length, 1) * rowH + 44;

  const canvas = document.createElement("canvas");
  canvas.width = width * 2;
  canvas.height = height * 2;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("Canvas 2D context unavailable");
  ctx.scale(2, 2);

  // Outer background + card border
  ctx.fillStyle = "#0d1117";
  ctx.fillRect(0, 0, width, height);
  ctx.fillStyle = "#161b22";
  ctx.fillRect(12, 12, width - 24, height - 24);
  ctx.strokeStyle = "#30363d";
  ctx.lineWidth = 1;
  ctx.strokeRect(12.5, 12.5, width - 25, height - 25);

  // Header
  ctx.fillStyle = "#e6edf3";
  ctx.font = "700 18px 'Segoe UI', Arial, sans-serif";
  ctx.fillText("LoomGate Trajectory Fork & Diff", 28, 42);

  const badge = redacted
    ? `Guardrail Redacted (${redactionsApplied})`
    : "Raw Local Export";
  ctx.fillStyle = redacted ? "#3fb950" : "#d29922";
  ctx.font = "600 12px 'Segoe UI', Arial, sans-serif";
  ctx.textAlign = "right";
  ctx.fillText(badge, width - 28, 40);
  ctx.textAlign = "left";

  ctx.fillStyle = "#58a6ff";
  ctx.font = "600 13px Consolas, monospace";
  ctx.fillText(`${diff.original.actual_model}  ->  ${diff.forked.actual_model}`, 28, 68);

  const origTotalIn = diff.original.input_tokens + diff.original.cache_read_tokens;
  const forkTotalIn = diff.forked.input_tokens + diff.forked.cache_read_tokens;
  const origHit = origTotalIn > 0 ? Math.round((diff.original.cache_read_tokens / origTotalIn) * 100) : 0;
  const forkHit = forkTotalIn > 0 ? Math.round((diff.forked.cache_read_tokens / forkTotalIn) * 100) : 0;
  const ttftSign = diff.ttft_delta_ms >= 0 ? "+" : "";
  const outSign = diff.output_tokens_delta >= 0 ? "+" : "";

  ctx.fillStyle = "#8b949e";
  ctx.font = "12px 'Segoe UI', Arial, sans-serif";
  ctx.fillText(
    `TTFT: ${diff.original.ttft_ms}ms vs ${diff.forked.ttft_ms}ms (${ttftSign}${diff.ttft_delta_ms}ms)  |  Output: ${diff.original.output_tokens} vs ${diff.forked.output_tokens} tok (${outSign}${diff.output_tokens_delta})  |  KV Cache: ${origHit}% vs ${forkHit}%`,
    28,
    92
  );

  // Diff box
  const boxH = Math.max(lines.length, 1) * rowH + 20;
  ctx.fillStyle = "#090c10";
  ctx.fillRect(24, headerH - 14, width - 48, boxH);
  ctx.strokeStyle = "#30363d";
  ctx.strokeRect(24.5, headerH - 13.5, width - 49, boxH - 1);

  ctx.font = "12px Consolas, monospace";
  lines.forEach((dl, idx) => {
    const y = headerH + idx * rowH;
    if (dl.kind === "add") {
      ctx.fillStyle = "rgba(63,185,80,0.18)";
      ctx.fillRect(25, y, width - 50, rowH);
      ctx.fillStyle = "#aff5b4";
    } else if (dl.kind === "del") {
      ctx.fillStyle = "rgba(248,81,73,0.18)";
      ctx.fillRect(25, y, width - 50, rowH);
      ctx.fillStyle = "#ffdcd7";
    } else {
      ctx.fillStyle = "#8b949e";
    }
    const sigil = dl.kind === "add" ? "+" : dl.kind === "del" ? "-" : " ";
    ctx.fillText(`${sigil} ${(dl.text || " ").slice(0, 108)}`, 36, y + 15);
  });

  ctx.fillStyle = "#6e7681";
  ctx.font = "11px 'Segoe UI', Arial, sans-serif";
  ctx.fillText("Generated locally by LoomGate Studio (zero raw prompts uploaded)", 28, height - 18);

  return canvas.toDataURL("image/png");
}

export function triggerDownload(filename: string, href: string) {
  const a = document.createElement("a");
  a.href = href;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
}
