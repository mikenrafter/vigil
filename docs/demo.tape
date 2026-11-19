Output docs/demo.gif
Require vigil
Require jq
Require bash
Require tee

Set Framerate 12
Set PlaybackSpeed 1.5
Set LoopOffset 0%
Set FontSize 22
Set Width 1500
Set Height 620
Set TypingSpeed 25ms
Set Shell bash
Set Theme "Catppuccin Mocha"

# After cargo build --release, record from the repository root with:
# PATH="$PWD/target/release:$PATH" vhs docs/demo.tape
# Prepare an isolated scratch directory and a minimal PATH, without boat or
# qex. Start a clean shell so personal prompts, agent settings and vigil state stay out.
Hide
Type `export DEMO_BIN="$PWD/target/release"; exec env -i HOME="$HOME" TERM=xterm-256color DEMO_BIN="$DEMO_BIN" PATH=/usr/bin:/bin bash --noprofile --norc`
Enter
Sleep 500ms
Type `export PATH="$DEMO_BIN:/usr/bin:/bin"; demo_dir=$(mktemp -d); cd "$demo_dir"; export XDG_STATE_HOME="$demo_dir/state" XDG_CONFIG_HOME="$demo_dir/config" PS1='$ ' PS2='  '`
Enter
Type `! command -v boat && ! command -v qex`
Enter
# The demo build takes seven seconds. tee writes both the log and progress
# in the terminal, so viewers can see work happening during the blocking call.
Type `printf '%s\n' '#!/bin/bash' '{ sleep 5; echo "build: compiling..."; sleep 1; echo "build: linking..."; sleep 1; echo "build: finished"; } | tee build.log' > build; chmod +x build`
Enter
Type `clear`
Enter
Show

# Establish what the tool does before starting the work.
Type `# vigil keeps the agent here until the build ends.`
Enter
Type `# keepalive blocks; vigil checks the build every 30s.`
Enter
Type `# --orchestrator false skips main-agent checks in this shell demo.`
Enter
Sleep 2s
Type `./build &`
Enter

# Capture the real hold id, label the process, and explicitly keep this a
# plain call. The line break keeps every flag readable at README size.
Type `id=$(vigil hold --id demo --for pid:$! --label build --expect-s 30 \`
Enter
Type `     --orchestrator false | jq -r .watch_id)`
Enter
Type `vigil keepalive "$id" --brief`
Enter

# This is the actual blocking call, not staged output. Its next process
# check observes the completed build and prints the release and next action.
Sleep 30s
# At 1.5x playback, six more seconds leave about 4s to read the result.
Sleep 6s

Hide
Type `cd /; rm -rf "$demo_dir"; exit`
Enter
