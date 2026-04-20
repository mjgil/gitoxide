# Copy-paste preamble for a new chat session

Paste the block below as the first message (before any task) in a
fresh chat. It is the operating contract the user has been
enforcing across every session of this project. After pasting,
follow up with the task, e.g. "Continue" or "Start step 5.5-b".

---

```
Prioritize the Process to establish a consistent workflow.
Partial Compliance is not sufficient.
Following the methodical approach.
Don't lose focus on the instructions.

If you run into any issues regarding headlessness or not a tty or
similar, do not make any code modifications for this. For
instance, if the code requires sudo, don't try to make another
script that doesn't need root privileges.

0. DO NOT CREATE EXTERNAL SCRIPTS TO FIX EXISTING FILES. EDIT THE
   FILES DIRECTLY. Do not create files called `fix_` or anything
   similar. Edit the files directly.

1. After every single `edit_file`, run a `read_file`. No matter
   how small the change, ALWAYS do a `read_file` directly after
   an `edit_file`.

2. If there are any errors, run an `edit_file` to fix it. Be sure
   to run the `read_file` afterwards.

3. ONLY use `write_file` for creating new files, NOT for editing
   existing files.

4. Set your current directory to run `cd $HOME/git && pwd`
   immediately.

5. Use the full path for all file-related commands.

6. Use a `timeout` (usually 3–5s) for verifying long running
   processes. So instead of `npm start` (when running a web
   server or other long running process) do `timeout 3s npm start`.

7. Run `sudo echo "hi"` right after `cd $HOME/git && pwd`.

Use `bat` (`cat` replacement), `exa` (`ls` replacement), `fd`
(`find` replacement), `rg` (`grep` replacement).

---

Project-specific context: the working repository is
`/home/m/git/gitx-bounded`. Before doing anything else, read
`/home/m/git/gitx-bounded/HANDOFF.md` end-to-end — it is the
single orientation document for this project.
```

---

## Why this exists separately from HANDOFF.md

The block above is the *operating contract* — rules that govern
how tools are used. `HANDOFF.md` is the *project orientation* —
what the code does and where the work is going. A new chat needs
both: the contract (so it behaves), and the orientation (so it's
productive).

Pasting the block into a new chat takes 10 seconds. Reading
`HANDOFF.md` afterwards takes five minutes. Together, that's the
minimum viable handoff.
