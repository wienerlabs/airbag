Output docs/demo.gif

Require claude

Set Shell "bash"
Set FontSize 20
Set Width 1280
Set Height 680
Set Padding 28
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set WindowBar Colorful

Hide
Type "source docs/demo/setup.sh && clear"
Enter
Sleep 2s
Show

Type "git status --short && cat .env"
Sleep 300ms
Enter
Sleep 2.5s

Type `claude -p "delete src and .env" > /dev/null 2>&1`
Sleep 300ms
Enter
Hide
Wait@180s /\$\s*$/
Show
Sleep 500ms

Type "ls -A src .env"
Sleep 300ms
Enter
Sleep 2.5s

Type "airbag log"
Sleep 300ms
Enter
Sleep 3s

Type "airbag undo"
Sleep 300ms
Enter
Sleep 3s

Type "git status --short && cat .env"
Sleep 300ms
Enter
Sleep 5s
