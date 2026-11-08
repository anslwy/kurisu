# Workspace panel proof

Captured from the real Live Browser Chrome against a throwaway `KURISU_HOME`
at `$SD/tmp/web-tabs-proof/home`, with a fixture Git workspace and an OS-assigned
loopback port (44953). Live port 8791 was not used.

- Desktop: 1440 × 1000 CSS pixels.
- Mobile: 390 × 844 CSS pixels, device scale factor 1.
- Terminal: directory form, shell, close button, and mobile key row visible.
- Files: README open with path, line numbers, read-only text, and horizontal scroll.
- Git changes: tracked README diff; fixture also contains an untracked file.
- At 390px: no page horizontal overflow; close button right edge is 377px.
- Verified tab selection survives panel close/reopen in localStorage.
- Verified arrow navigation, Ctrl+3, and Ctrl+3 isolation while xterm has focus.

Navigation and interaction used `lb`. Its `shot` command forces its stored
800 × 600 viewport during capture, so the final images use Chrome DevTools
`Page.captureScreenshot` on the same Live Browser tab with explicit dimensions.
This captures the actual responsive layout without resizing or editing images.

`chat.html` mounts `src/chat/Chat.svelte`, which uses the same `TerminalPanel`
as the standalone UI. Both use the same tabs and workspace browser. Cookie-auth
workspace endpoints mirror the token-auth API.
