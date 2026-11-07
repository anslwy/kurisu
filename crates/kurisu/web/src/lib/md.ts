// Tiny, safe markdown: escape everything first, then add a few constructs.
function esc(t: string) {
  return t.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}
function inline(t: string) {
  return t
    .replace(/`([^`\n]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[\s(])\*([^*\n]+)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
}
export function md(text: string): string {
  let out = "";
  esc(text).split(/```/).forEach((p, i) => {
    if (i % 2 === 1) { out += "<pre><code>" + p.replace(/^[a-zA-Z0-9_+-]*\n/, "") + "</code></pre>"; return; }
    for (const b of p.split(/\n{2,}/)) {
      if (!b.trim()) continue;
      const lines = b.replace(/^\n+|\n+$/g, "").split("\n");
      if (lines.every((l) => /^\s*[-*] /.test(l))) out += "<ul>" + lines.map((l) => "<li>" + inline(l.replace(/^\s*[-*] /, "")) + "</li>").join("") + "</ul>";
      else out += "<p>" + lines.map(inline).join("<br>") + "</p>";
    }
  });
  return out;
}
export function plain(text: string) {
  return text.replace(/```[a-zA-Z0-9_+-]*/g, "").replace(/[*`_#>]+/g, "").replace(/\s+/g, " ").trim();
}
