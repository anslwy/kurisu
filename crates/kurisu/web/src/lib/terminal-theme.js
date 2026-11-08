// xterm's ITheme fields, populated from the app's light/dark palette. No second
// colour palette or xterm package dependency alongside the vendored terminal.
const ansi = {
  black: "--fg", red: "--accent", green: "--ok", yellow: "--warn",
  blue: "--busy", magenta: "--accent", cyan: "--busy", white: "--fg",
  brightBlack: "--muted", brightRed: "--err", brightGreen: "--ok", brightYellow: "--warn",
  brightBlue: "--busy", brightMagenta: "--accent", brightCyan: "--ok", brightWhite: "--fg",
};
export function terminalTheme(read) {
  const theme = Object.fromEntries(Object.entries(ansi).map(([name, variable]) => [name, read(variable)]));
  const accent = read("--accent");
  // Existing palette colours are six-digit hex. xterm accepts hex alpha (25%).
  return { ...theme, background: read("--bg2"), foreground: read("--fg"),
    cursor: accent, cursorAccent: read("--bg2"), selectionBackground: `${accent}40`,
    selectionForeground: read("--fg") };
}
