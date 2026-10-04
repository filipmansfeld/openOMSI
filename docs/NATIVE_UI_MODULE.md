# Optional local native UI modules

`OMSI_UI_MODULE` may name a local JSON configuration for a trusted headless
backend. Without it, the ordinary launcher and game interface are unchanged.
The engine renders the module's controls with its existing native UI toolkit;
it does not embed the backend's application window or a browser.

Configuration contains an absolute `executable`, optional `args` and absolute
`working_directory`, optional executable `sha256`, and optional `dependencies`
with `path` and `sha256` pins. Dependency paths may be relative to the working
directory. Verify and distribute these local files with the backend. The engine
starts it without a console window on Windows, with inherited standard input
and output pipes; standard error is not exposed in the UI.

Frames consist of a four-byte little-endian length followed by UTF-8 JSON, up to
256 KiB. Protocol version 1 uses `ui.get` and `ui.action`, request IDs, a page,
context (`launcher` or `game`), and optional action ID, state revision and bounded
string fields. Replies echo the request ID and contain `ok`, optional `error`,
and typed controls: headings, text, status, fields, choices, buttons, progress
and tables. Secret fields are masked and cannot copy their value to the clipboard;
backends must return an empty value for those fields. Account passwords, session
tokens and signed download URLs must not appear in returned states or logs.

The launcher places downloads in its navigation and account/card controls inside
its existing Driver page. The selected offline driver file and its driving
history retain their original identity. The game offers the same native panel
from its menu. This transport does not add voice, GPS or vehicle-device settings.

The initial backend descriptor returns a random named-pipe name and a capability.
The engine passes those only through the child game's environment, together with
the game's process identity in subsequent requests. A backend can use a
current-user-only Windows pipe to share the launcher's in-memory login with one
active game connection. It must validate that game's identity before attaching to
the vehicle API. Game keepalive requests continue while the panel is closed.
After a successful game launch, closing the launcher leaves the backend's bounded
handoff and game lifetime to that backend. Errors and uncertain actions are shown
without automatically replaying mutations.

This is a local rendering/transport interface. Server-side authentication and
download entitlement checks remain the backend/server's responsibility. Native
vehicle operations use the separate engine API and its session/generation guards.
