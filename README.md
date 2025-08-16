# Logrok

## What it does
You have millions and millions of debug log lines, with the traces of a bug
buried in them.
The goal is to quickly find relevant lines to the problem at hand and to
reduce the display to only those lines while highlighting relevant items.

Main features include
- mark and color keywords
- tag lines based on marks
- filter untagged lines
- fast search
- indenting line continuations
- folding long lines
- vim motions

Hit `Ctrl-h` to see the help screen.

## Installation
Type `cargo install logrok` to install the latest version. It will be
installed under the name **lg**.

## Usage
Run `lg <filename>` to view a logfile.

## Workflow
The basic idea of logrok is to quickly filter down a large logfile to
the relevant lines for the current debugging problem. To make it visually
even easier to see key information, you can color matching strings with
different colors, all with a few keystrokes.
To start, move the cursor to a word you want to mark and hit `m` or `M`
to mark it. This will highlight all occurances of the it. While the cursor
is still on the word, hit `t` to tag all lines that contain the marked word.
Now you can filter all other lines by hitting `f`. This switches the
display mode from `Normal` to `Tagged` (dispayed in the status line to the
right). To switch back to `Normal`, hit `d`.
By setting multiple marks, you can quickly filter down the logfile to the
lines you need to see.
Use `Ctrl-h` to see key bindings.

## Marking
`m` marks the word under the cursor, while `M` marks a word including special
characters. With `.` and `,` you can extend/shrink the mark at the end, while
`<` and `>` extend/shrink the mark at the beginning.
Another way to select a word to mark is by using search and hitting `m` to
convert the search into a mark.
Hitting `m` on a mark will remove it.

## Tagging
`t` on a mark tags all lines that contain the mark, while hitting `t` on an
unmarked part of the line will tag only this line. Hitting `t` again removes
the tag.
`x` acts the same way, but instead of tagging, it hides the lines.
If you only want to tag the current line while the cursor is on a mark, use
`T`/`X` instead.
The status of the line is displayed in the first column:
- `T` manually tagged line
- `*` tagged by a mark
- `H` manually hidden line
- `-` hidden by a mark

## Display modes
`f` switches the display mode forward to more restrictive modes, while `d`
cycles back to less restrictive modes. The modes are:
- `All`: all lines are displayed
- `Normal`: only hidden lines are filtered out
- `Tagged`: only tagged lines and lines with a search hit are displayed
- `Manual`: only manually tagged lines and lines with a search hit are
  displayed

## Searching
`/` does a simple text search, `?` does the same backwards. Using `&` you
can do a regex search.
Lines containing search results are always displayed, independent of the
display mode.

## Indent
Overlong lines get wrapped to the next line(s). This can make it visually
harder to get an overview. To make it easier, logrok indents line
continuations. The default is 2 spaces. You can change this by moving the
cursor to the column you want the indent at and hitting `i`. A good point
for an indentation is for example after the timestamp.

## Overlong lines
If you are on an overlong line, you can fold it by hitting `o`. This will
clip the number of display lines for it. Increase/decrease the number of
lines with `+`/`-`. To unfold the line, hit `o` again.

## Miscellaneous
`u` for undo, `q` for quit. See `Ctrl-h` for supported vim-motions.
