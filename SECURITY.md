# Security Policy

DAWStream runs inside your DAW (as a plugin) and next to it (as an app), and it shares your audio between them through Windows shared memory. If you find a way to abuse that, I'd really like to know.

## Reporting a vulnerability

**Please don't open a public issue for security problems.** Everyone can read issues, including people who'd use the bug before it's fixed.

Report it privately instead:

1. Go to the [Security tab](https://github.com/emreYilmaz28845/DAWStream/security) of this repository.
2. Click **Report a vulnerability**.
3. Describe the problem, how to reproduce it, and what an attacker could do with it.

You'll get a reply within a few days. Once there's a fix, it ships in a new release and the advisory is published, with credit to you unless you'd rather stay anonymous.

## What counts

Some examples of things worth reporting:

- Another program reading, injecting or blocking the audio in the shared-memory ring buffer
- The app capturing or streaming a window you didn't choose
- The plugin launching an executable other than DAWStream, especially from an elevated DAW
- `Install.bat` being tricked into writing files somewhere it shouldn't
- Problems in the release build or the published zip

Bugs that only crash or glitch the audio without a security impact are fine as normal [issues](https://github.com/emreYilmaz28845/DAWStream/issues/new/choose).

## Supported versions

Only the latest release gets security fixes. Please check that the problem still happens there before reporting.
