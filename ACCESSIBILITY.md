# Accessibility

hostmic has no graphical interface: it is a driver, command-line tools and documentation.
This file says what can be expected of them, where the limits are, and how to report a
barrier.

## What we aim for

- **The microphone itself** is a standard Windows audio device: it appears in the Windows
  sound settings and in recording applications like any microphone, with the accessibility
  those already provide.
- **The tools** (`hostmic-ctl`, `test-host`, `capture-check`) print plain text, one value per
  line with its unit, in English; they use no color, animation or terminal positioning, so
  their output reads the same in a screen reader, a braille display or a log file. Their exit
  status says whether they succeeded.
- **The documentation** is Markdown: headings, lists and tables, readable on GitHub and in any
  text editor; commands are given as text to copy, never as screenshots.

## Known limitations

- The README's diagram is drawn with text characters; a screen reader reads it as symbols. The
  paragraphs around it say the same in words.
- Testing the driver requires a Hyper-V virtual machine and its console window
  ([docs/test-vm.md](docs/test-vm.md)); the console's accessibility is Windows'.
- Everything is in English only.

## Reporting a barrier

Open an issue describing what you tried, what you use (screen reader, magnifier, keyboard
only…) and what got in the way. Accessibility problems are bugs and are handled as such
([CONTRIBUTING.md](CONTRIBUTING.md)).
