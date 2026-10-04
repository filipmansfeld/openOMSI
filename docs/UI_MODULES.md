# Native local UI modules

An optional headless local process can provide controls inside the native launcher
and game. The engine uses its own `omsi-ui` widgets and theme. It does not embed a
foreign window, capture a form, or treat HTMLTexture as a browser. The process owns
its application logic; the engine only sends explicit control actions and displays
the returned state.

Without `OMSI_UI_MODULE`, the existing interface is unchanged. With that variable,
its value is an explicit local JSON configuration filename. The configuration has
`executable` (an absolute path), optional `args`, `working_directory`, `sha256`, and
`dependencies` containing `{path,sha256}` pins. Relative dependency paths resolve
against the working directory. A mismatched pin prevents process startup. The
configuration is installation data, not something supplied by a remote server.

The parent engine starts the process with stdin/stdout pipes and no console window
on Windows. All messages are a four-byte little-endian byte count followed by UTF-8
JSON; the count must be between 1 and 262144. Stdout is exclusively this protocol.
The first response is `{v:1,pipe_name,capability}`. The pipe name is a simple local
name, and the capability is 64 lowercase hexadecimal characters. A Windows backend
must restrict its named pipe to the current user and use byte transmission mode.

The launcher retains this descriptor in RAM. A game launched by it receives
`OMSI_UI_PIPE` and `OMSI_UI_CAP` only in that child's environment. These values are
never a command-line argument, duty field, instance record, configuration file or
log entry. The launcher uses its original stdio connection, while the game connects
to `\\.\pipe\<pipe_name>` and includes the capability on each request. Both connections
share the backend's account session. A successful launch transfers process lifetime:
after parent stdio closes, the backend allows a bounded handoff grace (30 seconds)
and remains alive while a game pipe is connected, then exits. A process not handed
to a game is terminated with its owning launcher.

Requests have `{v:1,id,op,params}` and optionally the game-pipe `capability`.
`op` is `ui.get` or `ui.action`. Params contain `context` (`launcher` or `game`),
`page`, and optionally `action_id`, `revision`, and `values` (field ID to string).
Game requests additionally contain the exact `engine_pid`; the backend must bind
native vehicle operations to that process's bridge manifest. Launcher context must
not consume a native vehicle-bridge connection.

Replies have `{v:1,id,ok,state,error?}`. IDs must match exactly. State contains
`revision`, `title`, `page`, `pages:[{id,label}]`, `auth:{state,display_name}`, `busy`,
and `items`. Items have an ID, label and one of these kinds:

- `heading`, `text`, `status`: native text, with optional `value`.
- `field`: an editable string; `secret:true` masks it and prevents copying it.
- `choice`: `options:[{id,label}]` and the selected option ID in `value`.
- `button`: an opaque `action_id` and `enabled` flag.
- `progress`: optional finite `fraction` from 0 to 1, and optional `current`/`total`.
- `table`: string rows and cells.

Optional strings/arrays are omitted rather than returned as JSON null. Control IDs
are bounded ASCII identifiers; field values are at most 4096 UTF-8 bytes. Secret
fields always return an empty value. Account/API tokens are never UI state. The
backend validates actions, revision and field values; it does not execute arbitrary
commands supplied through the control schema. `ok:true,busy:true` acknowledges a
started operation and does not report its completion. A cancel button can remain
enabled while other actions are busy.

The `downloads` page appears in the native sidebar. The `profile` page is embedded
in the existing driver page, retaining its offline `.odr` identity and statistics.
The game's menu can open the same native controls. The module adds no voice/GPS
configuration authority: existing vehicle configuration and simulation date retain
their meaning.

Visible controls poll at most twice per second, with an additional two-second
keepalive while the game's panel is closed. I/O runs off the UI thread. Frames,
tables and queues are bounded, and a failed action is never retried automatically.
The engine requires a fresh state before another action and forwards its revision.
